//! The masks of a database's games (#272), `<id>.masks` beside its move
//! stream: for each record, [`ROW`] bytes saying what its main line ever had,
//! so that a search by a position fragment or by material rules most games out
//! before it replays the rest ([`super::fragment`]), as ChessBase's own search
//! booster does.
//!
//! A row holds, for each side, the squares its pawns, its minor pieces
//! (knights and bishops) and its major pieces (rooks and queens) stood on at
//! some point of the main line, and the least and the most of each kind it
//! had. Every position of the line is in them, so a game whose row lacks what
//! a filter needs cannot match it ([`super::fragment::Filter::may_match`]).
//!
//! The file is built from the index's move stream the first time a database is
//! searched so ([`build`]), on at most half of the search workers, and belongs
//! to that stream's build: a new index, after the database changed, makes it
//! stale, and the next search builds it again. Layout: a 64-byte header, the
//! rows in record order, then one CRC-32 per block of [`BLOCK`] rows, which is
//! checked the first time a search reads the block.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use chesscore::{Bitboard, Color, Piece, Replayer};

use crate::indexdir::{self, crc32, u32_at, u64_at};
use crate::search::SearchError;
use crate::search::memory::Cancel;
use crate::search::workers::{self, threads};

use super::file::{Bad, write_at};
use super::fragment::Variant;
use super::map::Map;
use super::runs::Progress;
use super::stream::{Stream, moves, standard};

const MAGIC: [u8; 8] = *b"OSCBMSK\0";
const VERSION: u32 = 1;
const HEADER: usize = 64;
/// Bytes of one game's row.
pub const ROW: usize = 64;
/// Rows a CRC covers, and a worker takes at a time.
pub const BLOCK: usize = 4096;
/// The kinds a row counts: pawns, knights, bishops, rooks and queens.
const COUNTED: usize = 5;
/// The most a count holds; more counts as this.
const MOST: u8 = 15;

/// The masks file beside the index at `index`: `<id>.masks`.
pub fn path_of(index: &Path) -> PathBuf {
    index.with_extension("masks")
}

/// What a game's main line ever had.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Row {
    /// For each side ([`Color::index`]), the squares its pawns, its knights
    /// and bishops, and its rooks and queens stood on.
    pub pawns: [Bitboard; 2],
    pub minors: [Bitboard; 2],
    pub majors: [Bitboard; 2],
    /// For each side and counted kind ([`Piece::index`]), its least and most.
    pub least: [[u8; COUNTED]; 2],
    pub most: [[u8; COUNTED]; 2],
    /// The index holds the game: a standard game, not deleted, whose moves
    /// could be read. Any other record matches nothing.
    pub indexed: bool,
}

impl Row {
    pub fn encode(&self) -> [u8; ROW] {
        let mut b = [0u8; ROW];
        for (i, v) in [self.pawns, self.minors, self.majors].iter().flatten().enumerate() {
            b[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        for c in 0..2 {
            for k in 0..COUNTED {
                b[48 + c * COUNTED + k] = self.least[c][k] | self.most[c][k] << 4;
            }
        }
        b[58] = u8::from(self.indexed);
        b
    }

    pub fn decode(b: &[u8]) -> Row {
        let at = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap_or_default());
        let mut row = Row {
            pawns: [at(0), at(1)],
            minors: [at(2), at(3)],
            majors: [at(4), at(5)],
            indexed: b[58] & 1 != 0,
            ..Row::default()
        };
        for c in 0..2 {
            for k in 0..COUNTED {
                let v = b[48 + c * COUNTED + k];
                row.least[c][k] = v & 15;
                row.most[c][k] = v >> 4;
            }
        }
        row
    }

    /// The least and the most of `piece` that `color` had; a count of
    /// [`MOST`] stands for that many or more.
    pub fn range(&self, color: Color, piece: Piece) -> (u8, u8) {
        let (c, k) = (color.index(), piece.index());
        if k >= COUNTED {
            return (1, 1);
        }
        let most = self.most[c][k];
        (self.least[c][k], if most >= MOST { u8::MAX } else { most })
    }

    /// Whether a line with this row may hold the fragment's form `v`: every
    /// square it looks for saw such a man, and a square of its Or did. Kings
    /// are not kept, and never rule a game out.
    pub(super) fn may_hold(&self, v: &Variant) -> bool {
        let (p, n, b, r, q, k) = (0, 1, 2, 3, 4, 5);
        for c in 0..2 {
            let look = &v.look[c];
            if look[p] & !self.pawns[c] != 0
                || (look[n] | look[b]) & !self.minors[c] != 0
                || (look[r] | look[q]) & !self.majors[c] != 0
            {
                return false;
            }
        }
        if !v.has_or() {
            return true;
        }
        (0..2).any(|c| {
            let or = &v.or[c];
            or[p] & self.pawns[c] != 0
                || (or[n] | or[b]) & self.minors[c] != 0
                || (or[r] | or[q]) & self.majors[c] != 0
                || or[k] != 0
        })
    }

    /// Adds the position `board` to the row.
    fn see(&mut self, board: &Replayer) {
        for color in Color::ALL {
            let c = color.index();
            let of = |piece: Piece| board.colored(piece, color);
            self.pawns[c] |= of(Piece::Pawn);
            self.minors[c] |= of(Piece::Knight) | of(Piece::Bishop);
            self.majors[c] |= of(Piece::Rook) | of(Piece::Queen);
            for k in 0..COUNTED {
                let Some(piece) = Piece::from_index(k) else { continue };
                let n = of(piece).count_ones().min(u32::from(MOST)) as u8;
                self.least[c][k] = self.least[c][k].min(n);
                self.most[c][k] = self.most[c][k].max(n);
            }
        }
    }

    /// The row of a line from `start` (the standard start for `None`) whose
    /// move words are `words`, every position of it seen.
    pub(super) fn of_line(start: Option<chesscore::Board>, words: impl Iterator<Item = u16>) -> Result<Row, Bad> {
        let mut row = Row { least: [[MOST; COUNTED]; 2], indexed: true, ..Row::default() };
        let mut board = Replayer::new(start.unwrap_or_else(|| standard().clone()));
        row.see(&board);
        let moves = moves();
        for word in words {
            let mv = moves.get(usize::from(word)).copied().flatten().ok_or(Bad::Corrupt("stream word"))?;
            board.play(mv);
            row.see(&board);
        }
        Ok(row)
    }
}

/// The masks of the games of one build of an index, open.
pub struct Masks {
    pub path: PathBuf,
    pub build_id: u64,
    first: u32,
    last: u32,
    map: Map,
    crcs: Vec<u32>,
    /// Whether each block was checked against its CRC.
    checked: Vec<AtomicBool>,
}

impl Masks {
    /// The masks at `path` when they were built for the stream `stream` and
    /// read back whole; `None` otherwise.
    pub fn open(path: &Path, stream: &Stream) -> Option<Masks> {
        let file = File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        let mut h = [0u8; HEADER];
        super::file::read_at(&file, 0, &mut h).ok()?;
        let header = &stream.header;
        if h[0..8] != MAGIC
            || u32_at(&h, 8) != VERSION
            || u32_at(&h, 12) as usize != ROW
            || u32_at(&h, 16) as usize != BLOCK
            || u32_at(&h, 20) != header.first_record
            || u32_at(&h, 24) != header.last_record
            || u64_at(&h, 32) != header.build_id
            || crc32(&h[..60]) != u32_at(&h, 60)
        {
            return None;
        }
        let records = rows(header.first_record, header.last_record);
        let blocks = records.div_ceil(BLOCK);
        let table_at = (HEADER + records * ROW) as u64;
        if u32_at(&h, 40) as usize != blocks || len != table_at + blocks as u64 * 4 {
            return None;
        }
        let mut table = vec![0u8; blocks * 4];
        super::file::read_at(&file, table_at, &mut table).ok()?;
        if crc32(&table) != u32_at(&h, 44) {
            return None;
        }
        let map = Map::new(&file, usize::try_from(len).ok()?).ok()?;
        Some(Masks {
            path: path.to_path_buf(),
            build_id: header.build_id,
            first: header.first_record,
            last: header.last_record,
            map,
            crcs: table.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect(),
            checked: (0..blocks).map(|_| AtomicBool::new(false)).collect(),
        })
    }

    /// The rows of block `block`, records `first + block * BLOCK` on, checked
    /// against their CRC the first time; damage is `Corrupt`.
    pub fn block(&self, block: usize) -> Result<&[u8], Bad> {
        let records = rows(self.first, self.last);
        let start = block * BLOCK;
        let n = records.saturating_sub(start).min(BLOCK);
        let at = HEADER + start * ROW;
        let bytes = self.map.bytes().get(at..at + n * ROW).ok_or(Bad::Corrupt("masks block"))?;
        let checked = self.checked.get(block).ok_or(Bad::Corrupt("masks block"))?;
        if !checked.load(Ordering::Relaxed) {
            if crc32(bytes) != self.crcs[block] {
                return Err(Bad::Corrupt("masks block"));
            }
            checked.store(true, Ordering::Relaxed);
        }
        Ok(bytes)
    }
}

fn rows(first: u32, last: u32) -> usize {
    (u64::from(last) + 1).saturating_sub(u64::from(first)) as usize
}

/// Why a build of masks did not end with them.
#[derive(Debug)]
pub struct Unbuilt {
    /// For the log; it names no path.
    pub why: String,
    /// The build was stopped, or wanted a worker or memory that searches
    /// held: worth trying again at once.
    pub again: bool,
}

impl Unbuilt {
    fn io(what: &str, e: std::io::Error) -> Unbuilt {
        Unbuilt { why: format!("{what}: {e}"), again: false }
    }
}

/// Builds the masks of the games of `stream` at `path`: written beside it as
/// `<path>.partial`, then renamed, and opened. Each worker takes a block of
/// [`BLOCK`] records at a time, replays their main lines from the stream and
/// writes their rows; `progress` counts the records done, and `cancel` stops
/// the build at its next block.
pub fn build(stream: &Stream, path: &Path, progress: &Progress, cancel: &Cancel) -> Result<Masks, Unbuilt> {
    let io = |e: std::io::Error| Unbuilt::io("writing the masks", e);
    let header = stream.header;
    let records = rows(header.first_record, header.last_record);
    let blocks = records.div_ceil(BLOCK);
    let partial = indexdir::partial(path);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let file = File::options().read(true).write(true).create(true).truncate(true).open(&partial).map_err(io)?;
    let crcs: Vec<AtomicU32> = (0..blocks).map(|_| AtomicU32::new(0)).collect();
    progress.start("masks", records as u64);
    let next = workers::Parts::new(blocks);
    let want = blocks.min((threads() / 2).max(1));
    let written = workers::run(want, BLOCK * ROW, cancel, |w| {
        let mut buf = w.buffer()?;
        while let Some(block) = next.take(w, cancel)? {
            let first = header.first_record + (block * BLOCK) as u32;
            let n = records.saturating_sub(block * BLOCK).min(BLOCK);
            for (i, number) in (first..first + n as u32).enumerate() {
                let record = stream.record(number).map_err(|_| SearchError::IndexDamaged)?;
                let row = if record.entry.indexed() {
                    let start = record.start().map_err(|_| SearchError::IndexDamaged)?;
                    Row::of_line(start, record.words()).map_err(|_| SearchError::IndexDamaged)?
                } else {
                    Row::default()
                };
                buf[i * ROW..(i + 1) * ROW].copy_from_slice(&row.encode());
            }
            let bytes = &buf[..n * ROW];
            write_at(&file, (HEADER + block * BLOCK * ROW) as u64, bytes).map_err(|_| SearchError::Busy)?;
            crcs[block].store(crc32(bytes), Ordering::Relaxed);
            progress.done.fetch_add(n as u64, Ordering::Relaxed);
        }
        Ok(())
    });
    let finish = || -> Result<(), Unbuilt> {
        written.map_err(|e| Unbuilt {
            again: matches!(e, SearchError::Busy | SearchError::WorkersBusy | SearchError::Superseded),
            why: format!("building the masks: {e:?}"),
        })?;
        let table: Vec<u8> = crcs.iter().flat_map(|c| c.load(Ordering::Relaxed).to_le_bytes()).collect();
        write_at(&file, (HEADER + records * ROW) as u64, &table).map_err(io)?;
        let mut h = [0u8; HEADER];
        h[0..8].copy_from_slice(&MAGIC);
        h[8..12].copy_from_slice(&VERSION.to_le_bytes());
        h[12..16].copy_from_slice(&(ROW as u32).to_le_bytes());
        h[16..20].copy_from_slice(&(BLOCK as u32).to_le_bytes());
        h[20..24].copy_from_slice(&header.first_record.to_le_bytes());
        h[24..28].copy_from_slice(&header.last_record.to_le_bytes());
        h[32..40].copy_from_slice(&header.build_id.to_le_bytes());
        h[40..44].copy_from_slice(&(blocks as u32).to_le_bytes());
        h[44..48].copy_from_slice(&crc32(&table).to_le_bytes());
        let crc = crc32(&h[..60]);
        h[60..64].copy_from_slice(&crc.to_le_bytes());
        write_at(&file, 0, &h).map_err(io)?;
        file.sync_all().map_err(io)?;
        Ok(())
    };
    let result = finish();
    drop(file);
    if let Err(why) = result {
        let _ = std::fs::remove_file(&partial);
        return Err(why);
    }
    indexdir::replace(&partial, path).map_err(|e| Unbuilt::io("renaming the masks", e))?;
    Masks::open(path, stream).ok_or_else(|| Unbuilt { why: "the new masks do not read back".into(), again: false })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_reads_back_as_written() {
        let row = Row {
            pawns: [0x00ff_0000_0000_ff00, 1],
            minors: [2, 3],
            majors: [4, u64::MAX],
            least: [[0, 1, 2, 3, 4], [5, 6, 7, 8, 15]],
            most: [[8, 2, 2, 3, 9], [8, 7, 7, 9, 15]],
            indexed: true,
        };
        assert_eq!(Row::decode(&row.encode()), row);
        assert_eq!(Row::decode(&Row::default().encode()), Row::default());
        assert_eq!(row.range(Color::Black, Piece::Queen), (15, u8::MAX));
        assert_eq!(row.range(Color::White, Piece::King), (1, 1));
    }
}
