//! The move stream (#145): each game's main line as 2CBH move words, in a
//! file of the bridge's own beside the position index, `<id>.moves`
//! (`docs/format-notes.md`, "Move stream"). A build writes it from the walk
//! that builds the index, each word checked as it was played, so every part
//! of the index sees the same lines. The deep section's candidates are then
//! replayed from it, mapped read-only, without legality checks and without
//! reading the database's files.
//!
//! A 2CBH word names one move from a list of every move each piece can make
//! on an empty board, so it means the same in any position: the words of a
//! classic database or a PGN file are those of the same moves, and one
//! stream format serves all three.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use chesscore::{Board, BoardBuilder, CastleSide, Color, Move, Piece, Square};

use cbformat::movetable::FIRST_PIECE_WORD;
use cbformat::replay;

use crate::indexdir::{crc32, u32_at, u64_at};
use crate::search::SearchError;
use crate::search::memory::{Cancel, Hold, Refused};
use crate::search::workers::{self, threads};

use super::file::{Bad, read_at, write_at};
use super::format::{NO_MOVE, Outcome, pack_move};
use super::map::Map;
use super::runs::{Progress, io, reserve};
use super::source::Line;

pub const MAGIC: [u8; 8] = *b"OSCBMOV\0";
pub const VERSION: u32 = 1;
pub const HEADER_LEN: usize = 128;
/// The words of a line kept in its record's prefix slot: the tree's depth
/// once #143 moves it to ply 20, and one more.
pub const PREFIX_WORDS: usize = 21;
const PREFIX_BYTES: usize = 2 * PREFIX_WORDS;
/// A record's directory entry.
pub const ENTRY_BYTES: usize = 16;
/// A set-up start in a tail: 18 words.
pub const SETUP_BYTES: usize = 36;
/// The most plies of a line the stream keeps; the line ends there.
pub const MAX_PLIES: usize = u16::MAX as usize;
/// The bytes each CRC of the chunk table covers.
pub const CHUNK: usize = 1 << 20;
/// Records a worker writes at a time, as the build reads them.
pub const BATCH: usize = 4096;
/// A worker's part of a build: its tail chunk, a batch's directory entries,
/// prefix slots and pending tails, and a line's words, which the walk keeps
/// ([`super::source::Workspace::keep_words`]).
pub const WORKER_BYTES: usize = CHUNK + BATCH * (ENTRY_BYTES + PREFIX_BYTES + 4) + MAX_PLIES * 2;

/// Directory flags beside the outcome (bits 0-1) and the average rating
/// (bits 2-13): a set-up start, and a game the index holds.
const SETUP: u16 = 1 << 14;
const INDEXED: u16 = 1 << 15;

/// The stream of the index at `index`: `<id>.moves` beside `<id>.idx`.
pub fn path_of(index: &Path) -> PathBuf {
    index.with_extension("moves")
}

/// What the stream holds and where.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Records `first_record..=last_record` have a directory entry each.
    pub first_record: u32,
    pub last_record: u32,
    /// The database's generation when built.
    pub generation: u64,
    /// The build's id, which the index built with it carries too.
    pub build_id: u64,
    pub games: u64,
    pub plies: u64,
    pub tail_offset: u64,
    pub tail_len: u64,
    pub table_offset: u64,
    pub chunks: u32,
    pub table_crc: u32,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..8].copy_from_slice(&MAGIC);
        b[8..12].copy_from_slice(&VERSION.to_le_bytes());
        b[12..16].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
        b[16] = PREFIX_WORDS as u8;
        b[20..24].copy_from_slice(&self.first_record.to_le_bytes());
        b[24..28].copy_from_slice(&self.last_record.to_le_bytes());
        b[32..40].copy_from_slice(&self.generation.to_le_bytes());
        b[40..48].copy_from_slice(&self.build_id.to_le_bytes());
        b[48..56].copy_from_slice(&self.games.to_le_bytes());
        b[56..64].copy_from_slice(&self.plies.to_le_bytes());
        b[64..72].copy_from_slice(&self.tail_offset.to_le_bytes());
        b[72..80].copy_from_slice(&self.tail_len.to_le_bytes());
        b[80..88].copy_from_slice(&self.table_offset.to_le_bytes());
        b[88..92].copy_from_slice(&self.chunks.to_le_bytes());
        b[92..96].copy_from_slice(&self.table_crc.to_le_bytes());
        let crc = crc32(&b[..124]);
        b[124..128].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// The header, or `None` when the bytes are not a header of this version.
    pub fn decode(b: &[u8]) -> Option<Header> {
        if b.len() < HEADER_LEN
            || b[0..8] != MAGIC
            || u32_at(b, 8) != VERSION
            || u32_at(b, 12) as usize != HEADER_LEN
            || usize::from(b[16]) != PREFIX_WORDS
            || crc32(&b[..124]) != u32_at(b, 124)
        {
            return None;
        }
        Some(Header {
            first_record: u32_at(b, 20),
            last_record: u32_at(b, 24),
            generation: u64_at(b, 32),
            build_id: u64_at(b, 40),
            games: u64_at(b, 48),
            plies: u64_at(b, 56),
            tail_offset: u64_at(b, 64),
            tail_len: u64_at(b, 72),
            table_offset: u64_at(b, 80),
            chunks: u32_at(b, 88),
            table_crc: u32_at(b, 92),
        })
    }

    /// The records with a directory entry.
    pub fn records(&self) -> u64 {
        records(self.first_record, self.last_record)
    }

    fn prefix_offset(&self) -> u64 {
        prefix_offset(self.records())
    }
}

fn records(first: u32, last: u32) -> u64 {
    (u64::from(last) + 1).saturating_sub(u64::from(first))
}

/// Where the prefix area starts, after the header and the directory.
fn prefix_offset(records: u64) -> u64 {
    HEADER_LEN as u64 + records * ENTRY_BYTES as u64
}

/// Where the tail area starts: 4-byte aligned after the prefix area.
fn tail_offset(records: u64) -> u64 {
    (prefix_offset(records) + records * PREFIX_BYTES as u64).next_multiple_of(4)
}

/// A record's directory entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// Where its tail starts, in words from the tail area's start: the set-up
    /// start, then the words past its prefix slot.
    pub tail: u32,
    pub plies: u16,
    pub flags: u16,
    pub departures: Departures,
}

impl Entry {
    fn encode(&self) -> [u8; ENTRY_BYTES] {
        let mut b = [0u8; ENTRY_BYTES];
        b[0..4].copy_from_slice(&self.tail.to_le_bytes());
        b[4..6].copy_from_slice(&self.plies.to_le_bytes());
        b[6..8].copy_from_slice(&self.flags.to_le_bytes());
        b[8..16].copy_from_slice(&self.departures.0.to_le_bytes());
        b
    }

    fn decode(b: &[u8]) -> Entry {
        Entry {
            tail: u32_at(b, 0),
            plies: u32_at(b, 4) as u16,
            flags: (u32_at(b, 4) >> 16) as u16,
            departures: Departures(u64_at(b, 8)),
        }
    }

    /// A standard game, not deleted, whose moves could be read: the index
    /// holds it.
    pub fn indexed(&self) -> bool {
        self.flags & INDEXED != 0
    }

    /// The game starts from a set-up position, which its tail holds.
    pub fn setup(&self) -> bool {
        self.flags & SETUP != 0
    }

    pub fn outcome(&self) -> Outcome {
        Outcome::from_bits(u32::from(self.flags))
    }

    pub fn elo(&self) -> u16 {
        self.flags >> 2 & 0xfff
    }

    /// Where its tail lies in the tail area, in bytes, and its set-up bytes.
    fn tail_span(&self) -> (u64, usize, usize) {
        let setup = if self.setup() { SETUP_BYTES } else { 0 };
        let past = usize::from(self.plies).saturating_sub(PREFIX_WORDS);
        (2 * u64::from(self.tail), setup, setup + 2 * past)
    }
}

/// The home pawns a game's line moves or loses, in the order they leave home
/// (white a-h 0-7, black a-h 8-15): the first 15 in bits 0-59, 4 bits each,
/// and how many in bits 60-63, where 15 means 15 or 16. A pawn on its home
/// square never came there, and once gone never returns, so the home pawns a
/// position has tell which of them a line must have lost to reach it, and in
/// no other order than theirs (Scid's home-pawn test).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Departures(pub u64);

impl Departures {
    pub fn push(&mut self, pawn: u32) {
        let n = self.count();
        if n < 15 {
            self.0 |= u64::from(pawn & 15) << (4 * n);
            self.0 = self.0 & !(15 << 60) | u64::from(n + 1) << 60;
        }
    }

    pub fn count(self) -> u32 {
        (self.0 >> 60) as u32
    }

    /// Whether a line that lost its home pawns in this order can reach a
    /// position whose home pawns are `home`: the first pawns it lost must be
    /// exactly those `home` lacks, since a ply loses at most one.
    pub fn allows(self, home: u16) -> bool {
        let missing = !home;
        let k = missing.count_ones();
        if k == 0 {
            return true;
        }
        let n = self.count();
        if k == 16 {
            return n == 15;
        }
        if n < k {
            return false;
        }
        let first = (0..k).fold(0u16, |set, i| set | 1 << (self.0 >> (4 * i) & 15));
        first == missing
    }
}

/// The home pawns of `board`: white pawns on the second rank in bits 0-7,
/// black pawns on the seventh in bits 8-15, a to h.
pub(super) fn home_pawns(board: &Board) -> u16 {
    let white = board.colored(Piece::Pawn, Color::White) >> 8 & 0xff;
    let black = board.colored(Piece::Pawn, Color::Black) >> 48 & 0xff;
    (white | black << 8) as u16
}

/// A set-up start as the stream keeps it: 32 bytes of pieces from a1 to h8,
/// a square in each half of a byte, low half first (0 empty, 1-6 white pawn,
/// knight, bishop, rook, queen, king, 9-14 the same for black); the side to
/// move (0 white, 1 black); the castling rights (1 white O-O-O, 2 white O-O,
/// 4 black O-O-O, 8 black O-O); the en passant file when a capture is
/// possible, else 8; and a zero byte.
pub fn setup_of(board: &Board) -> [u8; SETUP_BYTES] {
    let mut s = [0u8; SETUP_BYTES];
    for i in 0..64u8 {
        if let Some((p, c)) = Square::from_index(i).and_then(|sq| board.piece_at(sq)) {
            let code = 1 + p.index() as u8 + if c == Color::Black { 8 } else { 0 };
            s[usize::from(i / 2)] |= code << (4 * (i % 2));
        }
    }
    s[32] = u8::from(board.side_to_move() == Color::Black);
    for (bit, c, side) in [
        (1, Color::White, CastleSide::Long),
        (2, Color::White, CastleSide::Short),
        (4, Color::Black, CastleSide::Long),
        (8, Color::Black, CastleSide::Short),
    ] {
        if board.castling_rook(c, side).is_some() {
            s[33] |= bit;
        }
    }
    s[34] = board.en_passant().map_or(8, |sq| sq.file());
    s
}

/// The position [`setup_of`] wrote; `None` when the bytes hold none.
pub fn board_of(s: &[u8]) -> Option<Board> {
    let s: &[u8; SETUP_BYTES] = s.try_into().ok()?;
    let mut b = BoardBuilder::empty();
    for i in 0..64u8 {
        let code = s[usize::from(i / 2)] >> (4 * (i % 2)) & 15;
        if code != 0 {
            let piece = Piece::from_index(usize::from(code & 7).checked_sub(1)?)?;
            let color = if code & 8 != 0 { Color::Black } else { Color::White };
            b.set(Square::from_index(i)?, Some((piece, color)));
        }
    }
    b.side_to_move = if s[32] == 1 { Color::Black } else { Color::White };
    for (bit, c, side, file) in [
        (1, Color::White, CastleSide::Long, 0),
        (2, Color::White, CastleSide::Short, 7),
        (4, Color::Black, CastleSide::Long, 0),
        (8, Color::Black, CastleSide::Short, 7),
    ] {
        if s[33] & bit != 0 {
            b.castling[c.index()][side as usize] = Some(file);
        }
    }
    b.en_passant_file = (s[34] < 8).then_some(s[34]);
    b.build().ok()
}

/// The standard start as [`setup_of`] writes it: a line from it is no set-up.
pub(super) fn standard_setup() -> &'static [u8; SETUP_BYTES] {
    static START: OnceLock<[u8; SETUP_BYTES]> = OnceLock::new();
    START.get_or_init(|| setup_of(standard()))
}

fn standard() -> &'static Board {
    static START: OnceLock<Board> = OnceLock::new();
    START.get_or_init(Board::startpos)
}

/// The move each word below the set-up piece words names in standard chess;
/// `None` for the words a stream never holds.
fn moves() -> &'static [Option<Move>] {
    static MOVES: OnceLock<Vec<Option<Move>>> = OnceLock::new();
    MOVES.get_or_init(|| (0..FIRST_PIECE_WORD).map(replay::standard_move).collect())
}

/// The stream being written, `<id>.moves.partial`: each worker writes the
/// directory entries and prefix slots of its records in place, and appends
/// their tails in chunks, so that a game's tail is found only by its offset.
pub struct Writer {
    file: File,
    path: PathBuf,
    first: u32,
    records: u64,
    tail_offset: u64,
    /// The words appended to the tail area.
    tails: Mutex<u64>,
    games: AtomicU64,
    plies: AtomicU64,
}

impl Writer {
    /// A new stream at `path` for records `first..=last`.
    pub fn create(path: &Path, first: u32, last: u32) -> Result<Writer, SearchError> {
        let file =
            File::options().read(true).write(true).create(true).truncate(true).open(path).map_err(|e| io(path, e))?;
        let records = records(first, last);
        Ok(Writer {
            file,
            path: path.to_path_buf(),
            first,
            records,
            tail_offset: tail_offset(records),
            tails: Mutex::new(0),
            games: AtomicU64::new(0),
            plies: AtomicU64::new(0),
        })
    }

    /// A worker's part: its buffers, allocated fallibly, which the worker
    /// holds in the budget first ([`WORKER_BYTES`]).
    pub fn part(&self) -> Option<Part<'_>> {
        fn buf<T>(n: usize) -> Option<Vec<T>> {
            let mut v = Vec::new();
            v.try_reserve_exact(n).ok()?;
            Some(v)
        }
        Some(Part {
            writer: self,
            first: 0,
            count: 0,
            entries: buf(BATCH * ENTRY_BYTES)?,
            prefixes: buf(BATCH * PREFIX_BYTES)?,
            tail: buf(CHUNK)?,
            pending: buf(BATCH)?,
            games: 0,
            plies: 0,
        })
    }

    /// Appends a worker's tail chunk; where it starts, in words from the tail
    /// area's start.
    fn append(&self, chunk: &[u8]) -> Result<u64, SearchError> {
        let base = {
            let mut tails = self.tails.lock().unwrap_or_else(|e| e.into_inner());
            let base = *tails;
            *tails += chunk.len() as u64 / 2;
            base
        };
        write_at(&self.file, self.tail_offset + 2 * base, chunk).map_err(|e| io(&self.path, e))?;
        Ok(base)
    }

    /// Ends the stream of the database at `generation`, built with
    /// `build_id`: the CRC of each chunk, computed on up to half the workers,
    /// their table after the tails, then the header. The file is not synced:
    /// a torn one fails its CRCs and is rebuilt.
    pub fn finish(self, generation: u64, build_id: u64, progress: &Progress) -> Result<Header, SearchError> {
        let tail_len = 2 * *self.tails.lock().unwrap_or_else(|e| e.into_inner());
        let table_offset = self.tail_offset + tail_len;
        let chunks = (table_offset - HEADER_LEN as u64).div_ceil(CHUNK as u64);
        let chunks = u32::try_from(chunks).map_err(|_| SearchError::TooLarge)?;
        // The directory and prefixes are written whole, and the tails end
        // here; only the alignment before the tails may be missing.
        self.file.set_len(table_offset).map_err(|e| io(&self.path, e))?;
        let table = self.crcs(chunks as usize, table_offset, progress)?;
        write_at(&self.file, table_offset, &table).map_err(|e| io(&self.path, e))?;
        let header = Header {
            first_record: self.first,
            last_record: (u64::from(self.first) + self.records).saturating_sub(1) as u32,
            generation,
            build_id,
            games: self.games.load(Ordering::Relaxed),
            plies: self.plies.load(Ordering::Relaxed),
            tail_offset: self.tail_offset,
            tail_len,
            table_offset,
            chunks,
            table_crc: crc32(&table),
        };
        write_at(&self.file, 0, &header.encode()).map_err(|e| io(&self.path, e))?;
        Ok(header)
    }

    /// The chunk table: the CRC of each chunk from the header to `end`, read
    /// back by as many workers as half the budget's share holds a chunk for.
    fn crcs(&self, chunks: usize, end: u64, progress: &Progress) -> Result<Vec<u8>, SearchError> {
        let mut memory = reserve(CHUNK, progress)?;
        let mut want = threads().div_ceil(2).min(chunks).max(1);
        while want > 1 && memory.grow_quietly((want - 1) * CHUNK).is_err() {
            want -= 1;
        }
        let parts = workers::run(want, 0, &Cancel::never(), |w| {
            let mut buf = Vec::new();
            buf.try_reserve_exact(CHUNK).map_err(|_| Refused::Busy)?;
            buf.resize(CHUNK, 0);
            let mut crcs = Vec::new();
            for c in (w.index..chunks).step_by(w.count) {
                if w.stopped() || progress.stop.load(Ordering::Relaxed) {
                    return Err(SearchError::Superseded);
                }
                let at = (HEADER_LEN + c * CHUNK) as u64;
                let n = (end - at).min(CHUNK as u64) as usize;
                read_at(&self.file, at, &mut buf[..n]).map_err(|e| io(&self.path, e))?;
                crcs.push(crc32(&buf[..n]));
            }
            Ok(crcs)
        })?;
        let mut table = vec![0u8; 4 * chunks];
        for (k, crcs) in parts.iter().enumerate() {
            for (i, crc) in crcs.iter().enumerate() {
                let c = k + i * parts.len();
                table[4 * c..4 * c + 4].copy_from_slice(&crc.to_le_bytes());
            }
        }
        drop(memory);
        Ok(table)
    }
}

/// A worker's part of the stream: one batch of records at a time.
pub struct Part<'a> {
    writer: &'a Writer,
    /// The batch's first record, and how many.
    first: u32,
    count: usize,
    entries: Vec<u8>,
    prefixes: Vec<u8>,
    /// Tails not yet appended, and the batch's entries whose tails they hold,
    /// by their place in the batch: those entries count their offsets from
    /// the chunk's start until it is appended.
    tail: Vec<u8>,
    pending: Vec<u32>,
    games: u64,
    plies: u64,
}

impl Part<'_> {
    /// Starts the batch of records `first..=last`, at most [`BATCH`]: none is
    /// indexed until added.
    pub fn begin(&mut self, first: u32, last: u32) {
        self.first = first;
        self.count = (last.saturating_sub(first) as usize + 1).min(BATCH);
        self.entries.clear();
        self.entries.resize(self.count * ENTRY_BYTES, 0);
        self.prefixes.clear();
        self.prefixes.resize(self.count * PREFIX_BYTES, 0xff);
    }

    /// Adds `line`, a game of the batch that the index holds.
    pub fn add(&mut self, line: &Line) -> Result<(), SearchError> {
        let Some(i) = line.number.checked_sub(self.first).map(|i| i as usize).filter(|&i| i < self.count) else {
            return Ok(());
        };
        let words = &line.words[..line.words.len().min(MAX_PLIES)];
        let slot = &mut self.prefixes[i * PREFIX_BYTES..(i + 1) * PREFIX_BYTES];
        for (to, w) in slot.as_chunks_mut::<2>().0.iter_mut().zip(words) {
            *to = w.to_le_bytes();
        }
        let past = &words[words.len().min(PREFIX_WORDS)..];
        let setup = line.setup.as_ref();
        let bytes = setup.map_or(0, |_| SETUP_BYTES) + 2 * past.len();
        let mut tail = 0;
        if bytes > 0 {
            // A tail is at most 131,064 bytes: it always fits an empty chunk.
            if self.tail.len() + bytes > CHUNK {
                self.flush()?;
            }
            tail = (self.tail.len() / 2) as u32;
            if let Some(s) = setup {
                self.tail.extend_from_slice(s);
            }
            for w in past {
                self.tail.extend(w.to_le_bytes());
            }
            self.pending.push(i as u32);
        }
        let mut flags = line.outcome as u16 | line.elo.min(4095) << 2 | INDEXED;
        if setup.is_some() {
            flags |= SETUP;
        }
        let departures = if setup.is_some() { Departures::default() } else { line.departures };
        let entry = Entry { tail, plies: words.len() as u16, flags, departures };
        self.entries[i * ENTRY_BYTES..(i + 1) * ENTRY_BYTES].copy_from_slice(&entry.encode());
        self.games += 1;
        self.plies += words.len() as u64;
        Ok(())
    }

    /// Appends the tails gathered, and sets the offsets of the entries whose
    /// tails they are.
    fn flush(&mut self) -> Result<(), SearchError> {
        if self.tail.is_empty() {
            return Ok(());
        }
        let base = self.writer.append(&self.tail)?;
        for &i in &self.pending {
            let at = i as usize * ENTRY_BYTES;
            let tail = u32::try_from(base + u64::from(u32_at(&self.entries, at))).map_err(|_| SearchError::TooLarge)?;
            self.entries[at..at + 4].copy_from_slice(&tail.to_le_bytes());
        }
        self.pending.clear();
        self.tail.clear();
        Ok(())
    }

    /// Ends the batch: its tails appended, then its directory entries and
    /// prefix slots written in place.
    pub fn end(&mut self) -> Result<(), SearchError> {
        self.flush()?;
        let w = self.writer;
        let at = u64::from(self.first.saturating_sub(w.first));
        write_at(&w.file, HEADER_LEN as u64 + at * ENTRY_BYTES as u64, &self.entries).map_err(|e| io(&w.path, e))?;
        write_at(&w.file, prefix_offset(w.records) + at * PREFIX_BYTES as u64, &self.prefixes)
            .map_err(|e| io(&w.path, e))?;
        w.games.fetch_add(std::mem::take(&mut self.games), Ordering::Relaxed);
        w.plies.fetch_add(std::mem::take(&mut self.plies), Ordering::Relaxed);
        Ok(())
    }
}

/// A position looked for in games' lines: its key, each side's men and
/// pawns, and its home pawns. A line only ever loses men, pawns and home
/// pawns, so once it has fewer men or pawns of a side than the position, or
/// lacks a home pawn the position keeps, it can no longer reach it. A
/// promotion keeps the men and a capture of a piece keeps the pawns, so
/// nothing is concluded from the pieces of each kind, which a promotion adds.
#[derive(Clone, Copy, Debug)]
pub struct Target {
    key: u64,
    counts: [u32; 4],
    home: u16,
}

impl Target {
    pub fn of(board: &Board) -> Target {
        Target { key: board.hash(), counts: counts(board), home: home_pawns(board) }
    }

    /// Whether a line at `board` can no longer reach the position.
    fn passed(&self, board: &Board) -> bool {
        counts(board).iter().zip(self.counts).any(|(&have, need)| have < need)
            || home_pawns(board) & self.home != self.home
    }
}

/// Each side's men, then each side's pawns.
fn counts(board: &Board) -> [u32; 4] {
    [
        board.colors(Color::White).count_ones(),
        board.colors(Color::Black).count_ones(),
        board.colored(Piece::Pawn, Color::White).count_ones(),
        board.colored(Piece::Pawn, Color::Black).count_ones(),
    ]
}

/// A game whose line reaches a position: the move it played from its first
/// visit (`NO_MOVE` at the line's end), its outcome and rating.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub mv: u16,
    pub outcome: Outcome,
    pub elo: u16,
}

/// A game's line as the stream holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Game {
    pub entry: Entry,
    /// The set-up start; `None` for the standard one.
    pub start: Option<Board>,
    pub words: Vec<u16>,
}

/// A stream mapped read-only. The header and the chunk table are checked
/// when it opens; each chunk against its CRC the first time any reader
/// touches it. The table and a bit per chunk are held in the search budget;
/// the mapped file is the operating system's file cache, outside the budget.
pub struct Stream {
    pub path: PathBuf,
    pub header: Header,
    map: Map,
    crcs: Vec<u32>,
    /// A bit per chunk, set once its CRC matched.
    checked: Vec<AtomicU64>,
    _memory: Hold,
}

impl Stream {
    pub fn open(path: &Path) -> Result<Stream, Bad> {
        let file = File::open(path).map_err(Bad::Io)?;
        let len = file.metadata().map_err(Bad::Io)?.len();
        let mut head = [0u8; HEADER_LEN];
        read_at(&file, 0, &mut head).map_err(Bad::Io)?;
        let header = Header::decode(&head).ok_or(Bad::Corrupt("stream header"))?;
        let chunks = u64::from(header.chunks);
        // Every area must be where the header's counts put it, and the file
        // end with the table, before anything is allocated from them.
        if header.tail_offset != tail_offset(header.records())
            || header.tail_len % 2 != 0
            || header.tail_offset.checked_add(header.tail_len) != Some(header.table_offset)
            || header.table_offset.checked_add(4 * chunks) != Some(len)
            || (header.table_offset - HEADER_LEN as u64).div_ceil(CHUNK as u64) != chunks
            || header.games > header.records()
        {
            return Err(Bad::Corrupt("stream layout"));
        }
        let (table_len, words) = (4 * chunks as usize, (chunks as usize).div_ceil(64));
        let memory = Hold::reserve_quietly(table_len + 8 * words).map_err(|r| {
            if r == Refused::TooLarge { Bad::Corrupt("stream table larger than memory") } else { Bad::Busy }
        })?;
        let mut table = Vec::new();
        table.try_reserve_exact(table_len).map_err(|_| Bad::Busy)?;
        table.resize(table_len, 0);
        read_at(&file, header.table_offset, &mut table).map_err(Bad::Io)?;
        if crc32(&table) != header.table_crc {
            return Err(Bad::Corrupt("stream table"));
        }
        let mut crcs = Vec::new();
        crcs.try_reserve_exact(chunks as usize).map_err(|_| Bad::Busy)?;
        crcs.extend(table.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)));
        drop(table);
        let mut checked = Vec::new();
        checked.try_reserve_exact(words).map_err(|_| Bad::Busy)?;
        checked.extend((0..words).map(|_| AtomicU64::new(0)));
        let size = usize::try_from(len).map_err(|_| Bad::Corrupt("stream larger than memory"))?;
        let map = Map::new(&file, size).map_err(Bad::Io)?;
        Ok(Stream { path: path.to_path_buf(), header, map, crcs, checked, _memory: memory })
    }

    /// Bytes `at..at + len` of the areas before the table, their chunks
    /// checked first.
    fn bytes(&self, at: u64, len: usize) -> Result<&[u8], Bad> {
        let end = at
            .checked_add(len as u64)
            .filter(|&end| at >= HEADER_LEN as u64 && end <= self.header.table_offset)
            .ok_or(Bad::Corrupt("stream range"))?;
        if len > 0 {
            let chunk = |b: u64| ((b - HEADER_LEN as u64) / CHUNK as u64) as usize;
            for c in chunk(at)..=chunk(end - 1) {
                self.check(c)?;
            }
        }
        // Within the file: the table ends it.
        Ok(&self.map.bytes()[at as usize..end as usize])
    }

    fn check(&self, chunk: usize) -> Result<(), Bad> {
        let (Some(seen), Some(&crc)) = (self.checked.get(chunk / 64), self.crcs.get(chunk)) else {
            return Err(Bad::Corrupt("stream chunk"));
        };
        let bit = 1u64 << (chunk % 64);
        if seen.load(Ordering::Relaxed) & bit != 0 {
            return Ok(());
        }
        let start = HEADER_LEN + chunk * CHUNK;
        let end = (start + CHUNK).min(self.header.table_offset as usize);
        if crc32(&self.map.bytes()[start..end]) != crc {
            return Err(Bad::Corrupt("stream chunk"));
        }
        seen.fetch_or(bit, Ordering::Relaxed);
        Ok(())
    }

    /// Record `number`'s place among the directory's.
    fn index(&self, number: u32) -> Result<u64, Bad> {
        number
            .checked_sub(self.header.first_record)
            .map(u64::from)
            .filter(|&i| i < self.header.records())
            .ok_or(Bad::Corrupt("stream record"))
    }

    fn entry_at(&self, i: u64) -> Result<Entry, Bad> {
        Ok(Entry::decode(self.bytes(HEADER_LEN as u64 + i * ENTRY_BYTES as u64, ENTRY_BYTES)?))
    }

    /// Record `number`'s directory entry.
    pub fn entry(&self, number: u32) -> Result<Entry, Bad> {
        self.entry_at(self.index(number)?)
    }

    /// The line of `entry`, the `i`th record's.
    fn line(&self, i: u64, entry: &Entry) -> Result<LineBytes<'_>, Bad> {
        let plies = usize::from(entry.plies);
        let prefix = self.bytes(self.header.prefix_offset() + i * PREFIX_BYTES as u64, 2 * plies.min(PREFIX_WORDS))?;
        let (at, setup, len) = entry.tail_span();
        if len == 0 {
            return Ok(LineBytes { prefix, past: &[], setup: None });
        }
        if at.checked_add(len as u64).is_none_or(|end| end > self.header.tail_len) {
            return Err(Bad::Corrupt("stream tail"));
        }
        let (start, past) = self.bytes(self.header.tail_offset + at, len)?.split_at(setup);
        Ok(LineBytes { prefix, past, setup: (setup > 0).then_some(start) })
    }

    /// Game `number`'s line; its words empty when the index does not hold it.
    pub fn game(&self, number: u32) -> Result<Game, Bad> {
        let i = self.index(number)?;
        let entry = self.entry_at(i)?;
        let line = self.line(i, &entry)?;
        Ok(Game { entry, start: line.start()?, words: line.words().collect() })
    }

    /// Replays game `number`'s line to the first position that is `target`:
    /// the game, with the move played from there; `None` when the index does
    /// not hold the game, or its line never reaches the position. The words
    /// were checked when the stream was built, so they are played unchecked.
    /// A line stops as soon as it can no longer reach the position (see
    /// [`Target`]), and one whose home pawns left in an order the position
    /// does not allow is not played.
    pub fn find(&self, number: u32, target: &Target) -> Result<Option<Hit>, Bad> {
        let i = self.index(number)?;
        let entry = self.entry_at(i)?;
        if !entry.indexed() || (!entry.setup() && !entry.departures.allows(target.home)) {
            return Ok(None);
        }
        let line = self.line(i, &entry)?;
        let mut board = line.start()?.unwrap_or_else(|| standard().clone());
        let moves = moves();
        let mut words = line.words();
        loop {
            if target.passed(&board) {
                return Ok(None);
            }
            let mv = match words.next() {
                Some(w) => Some(moves.get(usize::from(w)).copied().flatten().ok_or(Bad::Corrupt("stream word"))?),
                None => None,
            };
            if board.hash() == target.key {
                return Ok(Some(Hit { mv: mv.map_or(NO_MOVE, pack_move), outcome: entry.outcome(), elo: entry.elo() }));
            }
            let Some(mv) = mv else { return Ok(None) };
            board.play_unchecked(mv);
        }
    }
}

/// A game's line in a stream: the words of its prefix slot, the words past
/// it, and its set-up start.
struct LineBytes<'a> {
    prefix: &'a [u8],
    past: &'a [u8],
    setup: Option<&'a [u8]>,
}

impl LineBytes<'_> {
    fn words(&self) -> impl Iterator<Item = u16> {
        self.prefix.as_chunks::<2>().0.iter().chain(self.past.as_chunks::<2>().0).map(|w| u16::from_le_bytes(*w))
    }

    /// The set-up start; `None` for the standard one.
    fn start(&self) -> Result<Option<Board>, Bad> {
        self.setup.map(|s| board_of(s).ok_or(Bad::Corrupt("stream set-up"))).transpose()
    }
}

/// A new build's id, which its index and its stream both carry.
pub fn build_id() -> u64 {
    getrandom::u64().unwrap_or_else(|_| {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        nanos as u64 ^ u64::from(std::process::id()) << 32
    })
}

/// Renames the stream `from` to `to`, replacing the one there. On Windows a
/// stream still mapped, by an answer in flight, cannot be replaced: the
/// rename is tried again until that answer is done, for up to a minute.
pub fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
    let deadline = std::time::Instant::now() + super::runs::MEMORY_WAIT;
    loop {
        match std::fs::rename(from, to) {
            Err(_) if cfg!(windows) && std::time::Instant::now() < deadline && from.exists() => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_and_entries_round_trip() {
        let h = Header {
            first_record: 1,
            last_record: 99,
            generation: 5,
            build_id: 0x1234_5678_9abc_def0,
            games: 90,
            plies: 7000,
            tail_offset: tail_offset(99),
            tail_len: 4000,
            table_offset: tail_offset(99) + 4000,
            chunks: 1,
            table_crc: 7,
        };
        let e = h.encode();
        assert_eq!(Header::decode(&e), Some(h));
        let mut bad = e;
        bad[44] ^= 1;
        assert_eq!(Header::decode(&bad), None, "the header's CRC covers the build id");
        assert_eq!(tail_offset(99) % 4, 0);
        assert_eq!(tail_offset(98), prefix_offset(98) + 98 * 42);
        let entry =
            Entry { tail: 70_000, plies: 65_535, flags: INDEXED | SETUP | 2400 << 2 | 1, departures: Departures(5) };
        assert_eq!(Entry::decode(&entry.encode()), entry);
        assert!(entry.indexed() && entry.setup());
        assert_eq!((entry.elo(), entry.outcome()), (2400, Outcome::Draw));
    }

    /// The home-pawn test lets through exactly the lines whose first pawns
    /// gone are the ones the position lacks.
    #[test]
    fn home_pawn_departures_allow_only_their_positions() {
        let mut d = Departures::default();
        // e2, e7, then f7 taken, then a2.
        for pawn in [4, 12, 13, 0] {
            d.push(pawn);
        }
        assert_eq!(d.count(), 4);
        let all = u16::MAX;
        assert!(d.allows(all), "the start");
        assert!(d.allows(all & !(1 << 4)), "after 1.e4");
        assert!(d.allows(all & !(1 << 4) & !(1 << 12)));
        assert!(d.allows(all & !(1 << 4) & !(1 << 12) & !(1 << 13) & !1));
        assert!(!d.allows(all & !(1 << 12)), "e7 went only after e2");
        assert!(!d.allows(all & !(1 << 4) & !(1 << 12) & !(1 << 14)), "g7 never went");
        assert!(!d.allows(all & !0x3f), "six gone, and the line lost four");
        assert!(!d.allows(0), "every home pawn gone");
        // Fifteen or sixteen gone: the order is kept for the first fifteen.
        let mut full = Departures::default();
        for pawn in 0..16 {
            full.push(pawn);
        }
        assert_eq!(full.count(), 15);
        assert!(full.allows(0), "all sixteen may be gone");
        assert!(full.allows(1 << 15));
        assert!(!full.allows(1 << 14), "the fifteenth to go was g7");
    }

    #[test]
    fn a_set_up_start_round_trips() {
        for fen in [
            "7k/P7/8/8/8/8/8/K7 w - - 0 1",
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R b Kq - 0 1",
            "rnbqkbnr/ppp1p1pp/8/3pPp2/8/8/PPPP1PPP/RNBQKBNR w KQkq f6 0 3",
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
        ] {
            let board = Board::from_fen(fen).unwrap();
            let s = setup_of(&board);
            assert_eq!(s[35], 0);
            let back = board_of(&s).unwrap();
            assert_eq!(back.hash(), board.hash(), "{fen}");
            assert_eq!(back.fen().split(' ').take(4).collect::<Vec<_>>(), fen.split(' ').take(4).collect::<Vec<_>>());
        }
        assert_eq!(&setup_of(&Board::startpos()), standard_setup());
        // An en passant file with no capture possible is not kept: the key
        // does not hold it either.
        let no_capture = Board::from_fen("4k3/8/8/8/4P3/8/8/4K3 b - e3 0 1").unwrap();
        assert_eq!(setup_of(&no_capture)[34], 8);
        assert_eq!(board_of(&[0; SETUP_BYTES]), None, "no kings");
    }
}
