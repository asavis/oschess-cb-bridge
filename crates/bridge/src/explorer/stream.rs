//! The move stream (#145): each game's main line as 2CBH move words, in a
//! file of the bridge's own beside the position index, `<id>.moves`
//! (`docs/format-notes.md`, "Move stream"). A build writes it first, from its
//! walk of the database's games, each word checked as it was played; the
//! tree and the deep section are then built from it (#147), so every part of
//! the index sees the same lines. The deep section's candidates are replayed
//! from it too, mapped read-only, without legality checks and without reading
//! the database's files.
//!
//! A 2CBH word names one move from a list of every move each piece can make
//! on an empty board, so it means the same in any position: the words of a
//! classic database or a PGN file are those of the same moves, and one
//! stream format serves all three.
//!
//! Each record carries a CRC-32 of itself and its tail, checked whenever it
//! is read: a replay checks the few hundred bytes it reads and nothing else,
//! and a build computes each CRC while the record is in its hands, then
//! writes the file from start to end. Each block of slots has a CRC-32 of
//! its number and its slots in the table, which a scan of every slot (#148)
//! checks the first time it reads the block, once while the stream is open.
//! That scan also finds which games start from the standard position and
//! which from a set-up one, which the stream keeps until the search budget
//! runs short (#142).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use chesscore::{Bitboard, Board, BoardBuilder, CastleSide, Color, Move, Piece, Replayer, Square};

use cbformat::movetable::FIRST_PIECE_WORD;
use cbformat::replay;

use crate::indexdir::{crc32, crc32_update, u32_at, u64_at};
use crate::search::memory::{Evict, Hold, Refused, register};
use crate::search::{Adding, Members, SearchError};

use super::file::{Bad, read_at, write_at};
use super::format::{NO_MOVE, Outcome, pack_move};
use super::map::Map;
use super::runs::io;
use super::source::Line;

pub const MAGIC: [u8; 8] = *b"OSCBMOV\0";
pub const VERSION: u32 = 3;
pub const HEADER_LEN: usize = 128;
/// The words of a line kept in its record's slot: the tree's depth, 20 plies,
/// and one more, the move from its last position.
pub const PREFIX_WORDS: usize = 21;
const PREFIX_BYTES: usize = 2 * PREFIX_WORDS;
/// A record's slot: its directory entry, its prefix words, two zero bytes
/// and its CRC, a cache line that no page boundary cuts.
pub const SLOT_BYTES: usize = 64;
/// Where a slot's prefix words start, and where its CRC is.
const PREFIX_AT: usize = 16;
const CRC_AT: usize = SLOT_BYTES - 4;
/// A set-up start in a tail: 18 words.
pub const SETUP_BYTES: usize = 36;
/// The most plies of a line the stream keeps; the line ends there.
pub const MAX_PLIES: usize = u16::MAX as usize;
/// Everything appended to the file starts at a multiple of this.
const ALIGN: u64 = SLOT_BYTES as u64;
/// Records in a block: a worker writes the slots of one block at a time, as
/// the build reads them, and the file's table gives where each block is.
pub const BATCH: usize = 4096;
/// A block's entry in the table: where its slots start, 8 bytes, and their
/// CRC, 4.
pub const TABLE_ENTRY: usize = 12;
/// The tails a worker gathers before it appends them.
pub const TAIL_BUFFER: usize = 1 << 20;
/// A worker's part of a build: its tail buffer, a block's slots and pending
/// tails, and a line's words, which the walk keeps
/// ([`super::source::Workspace::keep_words`]).
pub const WORKER_BYTES: usize = TAIL_BUFFER + BATCH * (SLOT_BYTES + 4) + MAX_PLIES * 2;

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
    /// Records `first_record..=last_record` have a slot each.
    pub first_record: u32,
    pub last_record: u32,
    /// The database's generation when built.
    pub generation: u64,
    /// The build's id, which the index built with it carries too.
    pub build_id: u64,
    pub games: u64,
    pub plies: u64,
    /// Where the table of blocks starts: the tails and the blocks of slots
    /// lie between the header and it.
    pub table_offset: u64,
    pub blocks: u32,
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
        b[64..68].copy_from_slice(&(BATCH as u32).to_le_bytes());
        b[72..80].copy_from_slice(&self.table_offset.to_le_bytes());
        b[80..84].copy_from_slice(&self.blocks.to_le_bytes());
        b[84..88].copy_from_slice(&self.table_crc.to_le_bytes());
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
            || u32_at(b, 64) as usize != BATCH
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
            table_offset: u64_at(b, 72),
            blocks: u32_at(b, 80),
            table_crc: u32_at(b, 84),
        })
    }

    /// The records with a slot.
    pub fn records(&self) -> u64 {
        records(self.first_record, self.last_record)
    }
}

fn records(first: u32, last: u32) -> u64 {
    (u64::from(last) + 1).saturating_sub(u64::from(first))
}

/// The CRC of record `number` whose slot's first 60 bytes are `slot` and
/// whose tail is `tail`: the number binds the record to its place.
fn record_crc(number: u32, slot: &[u8], tail: &[u8]) -> u32 {
    let c = crc32_update(!0, &number.to_le_bytes());
    !crc32_update(crc32_update(c, slot), tail)
}

/// The CRC of block `block` whose slots are `slots`: the number binds the
/// block to its place, and the slots' order within it holds each in its own.
fn block_crc(block: u32, slots: &[u8]) -> u32 {
    !crc32_update(crc32_update(!0, &block.to_le_bytes()), slots)
}

/// A record's directory entry, the first 16 bytes of its slot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// Where its tail starts, in words from the file's start: the set-up
    /// start, then the words past its prefix.
    pub tail: u32,
    pub plies: u16,
    pub flags: u16,
    pub departures: Departures,
}

impl Entry {
    fn encode(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
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

    /// The bytes of its tail: its set-up start's, then its words past the
    /// prefix's.
    fn tail_bytes(&self) -> (usize, usize) {
        let setup = if self.setup() { SETUP_BYTES } else { 0 };
        (setup, setup + 2 * usize::from(self.plies).saturating_sub(PREFIX_WORDS))
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
    home_of(board.colored(Piece::Pawn, Color::White), board.colored(Piece::Pawn, Color::Black))
}

/// The home pawns among the `white` and `black` pawns.
pub(super) fn home_of(white: Bitboard, black: Bitboard) -> u16 {
    (white >> 8 & 0xff | (black >> 48 & 0xff) << 8) as u16
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

pub(super) fn standard() -> &'static Board {
    static START: OnceLock<Board> = OnceLock::new();
    START.get_or_init(Board::startpos)
}

/// The move each word below the set-up piece words names in standard chess;
/// `None` for the words a stream never holds.
pub(super) fn moves() -> &'static [Option<Move>] {
    static MOVES: OnceLock<Vec<Option<Move>>> = OnceLock::new();
    MOVES.get_or_init(|| (0..FIRST_PIECE_WORD).map(replay::standard_move).collect())
}

/// The stream being written, `<id>.moves.partial`, from its start to its
/// end: each worker appends its tails as they fill its buffer and the slots
/// of each block it ends, so that a game's tail is found only by its offset
/// and a block only by the table.
pub struct Writer {
    file: File,
    path: PathBuf,
    first: u32,
    records: u64,
    /// Where the next append goes: the end of what is appended.
    end: Mutex<u64>,
    /// Each block's offset once appended, 0 until then, and its slots' CRC.
    blocks: Vec<(AtomicU64, AtomicU32)>,
    games: AtomicU64,
    plies: AtomicU64,
}

impl Writer {
    /// A new stream at `path` for records `first..=last`.
    pub fn create(path: &Path, first: u32, last: u32) -> Result<Writer, SearchError> {
        let records = records(first, last);
        let count = usize::try_from(records.div_ceil(BATCH as u64)).map_err(|_| SearchError::TooLarge)?;
        let mut blocks = Vec::new();
        blocks.try_reserve_exact(count).map_err(|_| SearchError::TooLarge)?;
        blocks.extend((0..count).map(|_| (AtomicU64::new(0), AtomicU32::new(0))));
        let file =
            File::options().read(true).write(true).create(true).truncate(true).open(path).map_err(|e| io(path, e))?;
        Ok(Writer {
            file,
            path: path.to_path_buf(),
            first,
            records,
            end: Mutex::new(HEADER_LEN as u64),
            blocks,
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
            block: 0,
            first: 0,
            count: 0,
            slots: buf(BATCH * SLOT_BYTES)?,
            tail: buf(TAIL_BUFFER)?,
            pending: buf(BATCH)?,
            games: 0,
            plies: 0,
        })
    }

    /// Appends `bytes`, a multiple of [`ALIGN`] long, after what is
    /// appended; where they start. Each append takes its place in turn and
    /// is written at once, so the file grows from its start to its end.
    fn append(&self, bytes: &[u8]) -> Result<u64, SearchError> {
        debug_assert!((bytes.len() as u64).is_multiple_of(ALIGN));
        let at = {
            let mut end = self.end.lock().unwrap_or_else(|e| e.into_inner());
            let at = *end;
            *end += bytes.len() as u64;
            at
        };
        write_at(&self.file, at, bytes).map_err(|e| io(&self.path, e))?;
        Ok(at)
    }

    /// Ends the stream of the database at `generation`, built with
    /// `build_id`: the table of blocks after them, then the header. The file
    /// is not synced: a torn one fails its CRCs and is rebuilt.
    pub fn finish(self, generation: u64, build_id: u64) -> Result<Header, SearchError> {
        let table_offset = *self.end.lock().unwrap_or_else(|e| e.into_inner());
        let mut table = Vec::new();
        table.try_reserve_exact(TABLE_ENTRY * self.blocks.len()).map_err(|_| SearchError::TooLarge)?;
        for (at, crc) in &self.blocks {
            let at = at.load(Ordering::Relaxed);
            if at == 0 {
                return Err(io(&self.path, std::io::Error::other("a block of the move stream was not written")));
            }
            table.extend(at.to_le_bytes());
            table.extend(crc.load(Ordering::Relaxed).to_le_bytes());
        }
        write_at(&self.file, table_offset, &table).map_err(|e| io(&self.path, e))?;
        let header = Header {
            first_record: self.first,
            last_record: (u64::from(self.first) + self.records).saturating_sub(1) as u32,
            generation,
            build_id,
            games: self.games.load(Ordering::Relaxed),
            plies: self.plies.load(Ordering::Relaxed),
            table_offset,
            blocks: u32::try_from(self.blocks.len()).map_err(|_| SearchError::TooLarge)?,
            table_crc: crc32(&table),
        };
        write_at(&self.file, 0, &header.encode()).map_err(|e| io(&self.path, e))?;
        Ok(header)
    }
}

/// A worker's part of the stream: one block of records at a time.
pub struct Part<'a> {
    writer: &'a Writer,
    /// The block, its first record, and how many records it has.
    block: usize,
    first: u32,
    count: usize,
    slots: Vec<u8>,
    /// Tails not yet appended, and the block's records whose tails they hold,
    /// by their place in the block: their slots count their tail offsets from
    /// the buffer's start, and lack their CRCs, until it is appended.
    tail: Vec<u8>,
    pending: Vec<u32>,
    games: u64,
    plies: u64,
}

impl Part<'_> {
    /// Starts the block of records `first..=last`, at most [`BATCH`] of them
    /// from the start of a block: none is indexed until added.
    pub fn begin(&mut self, first: u32, last: u32) {
        let from = first.saturating_sub(self.writer.first) as usize;
        debug_assert_eq!(from % BATCH, 0, "a block starts at a multiple of BATCH");
        self.block = from / BATCH;
        self.first = first;
        self.count = (last.saturating_sub(first) as usize + 1).min(BATCH);
        self.slots.clear();
        self.slots.resize(self.count * SLOT_BYTES, 0);
        for slot in self.slots.as_chunks_mut::<SLOT_BYTES>().0 {
            slot[PREFIX_AT..PREFIX_AT + PREFIX_BYTES].fill(0xff);
        }
    }

    /// Adds `line`, a game of the block that the index holds.
    pub fn add(&mut self, line: &Line) -> Result<(), SearchError> {
        let Some(i) = line.number.checked_sub(self.first).map(|i| i as usize).filter(|&i| i < self.count) else {
            return Ok(());
        };
        let words = &line.words[..line.words.len().min(MAX_PLIES)];
        let past = &words[words.len().min(PREFIX_WORDS)..];
        let setup = line.setup.as_ref();
        let bytes = setup.map_or(0, |_| SETUP_BYTES) + 2 * past.len();
        let mut tail = 0;
        if bytes > 0 {
            // A tail is at most 131,064 bytes: it always fits an empty buffer.
            if self.tail.len() + bytes > TAIL_BUFFER {
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
        let slot = &mut self.slots[i * SLOT_BYTES..(i + 1) * SLOT_BYTES];
        slot[..PREFIX_AT].copy_from_slice(&entry.encode());
        for (to, w) in slot[PREFIX_AT..PREFIX_AT + PREFIX_BYTES].as_chunks_mut::<2>().0.iter_mut().zip(words) {
            *to = w.to_le_bytes();
        }
        self.games += 1;
        self.plies += words.len() as u64;
        Ok(())
    }

    /// Appends the tails gathered, and ends the slots of the records whose
    /// tails they are: their offsets and their CRCs.
    fn flush(&mut self) -> Result<(), SearchError> {
        if self.tail.is_empty() {
            return Ok(());
        }
        let len = self.tail.len();
        self.tail.resize(len.next_multiple_of(SLOT_BYTES), 0);
        let base = self.writer.append(&self.tail)? / 2;
        for &i in &self.pending {
            let slot = &mut self.slots[i as usize * SLOT_BYTES..(i as usize + 1) * SLOT_BYTES];
            let entry = Entry::decode(slot);
            let at = 2 * entry.tail as usize;
            let tail = u32::try_from(base + u64::from(entry.tail)).map_err(|_| SearchError::TooLarge)?;
            slot[0..4].copy_from_slice(&tail.to_le_bytes());
            let bytes = self.tail.get(at..at + entry.tail_bytes().1).unwrap_or_default();
            let crc = record_crc(self.first + i, &slot[..CRC_AT], bytes);
            slot[CRC_AT..].copy_from_slice(&crc.to_le_bytes());
        }
        self.pending.clear();
        self.tail.clear();
        Ok(())
    }

    /// Ends the block: its tails appended, the CRCs of its other records,
    /// then its slots appended and placed in the table with their CRC.
    pub fn end(&mut self) -> Result<(), SearchError> {
        self.flush()?;
        for (i, slot) in self.slots.as_chunks_mut::<SLOT_BYTES>().0.iter_mut().enumerate() {
            if Entry::decode(slot).tail_bytes().1 == 0 {
                let crc = record_crc(self.first + i as u32, &slot[..CRC_AT], &[]);
                slot[CRC_AT..].copy_from_slice(&crc.to_le_bytes());
            }
        }
        let w = self.writer;
        let crc = block_crc(self.block as u32, &self.slots);
        let at = w.append(&self.slots)?;
        if let Some((block, sum)) = w.blocks.get(self.block) {
            block.store(at, Ordering::Relaxed);
            sum.store(crc, Ordering::Relaxed);
        }
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
    /// The first ply a line's first visit counts at: a line that reaches the
    /// position before it is counted elsewhere, by the tree (#146).
    from: u32,
}

impl Target {
    pub fn of(board: &Board) -> Target {
        let (white, black, pawns) = (board.colors(Color::White), board.colors(Color::Black), board.pieces(Piece::Pawn));
        Target {
            key: board.hash(),
            counts: counts(white, black, pawns),
            home: home_of(pawns & white, pawns & black),
            from: 0,
        }
    }

    /// The same position, found only in the lines that reach it first beyond
    /// ply `ply`: the tree holds it, with every game that reaches it within.
    pub fn beyond(self, ply: u8) -> Target {
        Target { from: u32::from(ply) + 1, ..self }
    }

    /// Whether a line at `board` can no longer reach the position.
    fn passed(&self, board: &Replayer) -> bool {
        let (white, black, pawns) = (board.colors(Color::White), board.colors(Color::Black), board.pieces(Piece::Pawn));
        counts(white, black, pawns).iter().zip(self.counts).any(|(&have, need)| have < need)
            || home_of(pawns & white, pawns & black) & self.home != self.home
    }
}

/// Each side's men, then each side's pawns.
fn counts(white: Bitboard, black: Bitboard, pawns: Bitboard) -> [u32; 4] {
    [white.count_ones(), black.count_ones(), (pawns & white).count_ones(), (pawns & black).count_ones()]
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

/// A stream mapped read-only. The header and the table of blocks are checked
/// when it opens, each record against its CRC whenever it is read, and each
/// block against its own the first time a scan reads it. The table and the
/// starts are held in the search budget, the starts until the budget runs
/// short; the mapped file is the operating system's file cache, outside the
/// budget.
pub struct Stream {
    pub path: PathBuf,
    pub header: Header,
    map: Map,
    blocks: Vec<Block>,
    starts: Arc<KeptStarts>,
    _memory: Hold,
}

/// The starts a stream keeps, which it gives up when the search budget runs
/// short, as any cache does: a scan then reads every slot again, and finds
/// them again when it has room for them.
#[derive(Default)]
struct KeptStarts(Mutex<Option<Arc<Starts>>>);

impl Evict for KeptStarts {
    fn evict(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// The starts of the games the index holds, as the first scan of every slot
/// while the stream is open finds them (#142): the games from the standard
/// start, and those from a set-up one. Every game from the standard start
/// reaches it at its first ply, so that a scan for it reads nothing more of
/// them, and replays the set-up ones alone: 3 MB for the Mega Database,
/// whose slots are 768 MB.
pub(super) struct Starts {
    pub standard: Members,
    pub set_up: Members,
}

impl Starts {
    /// Where a scan notes the starts of a block's games.
    pub fn noting(&self) -> Noting<'_> {
        Noting { standard: Adding::to(&self.standard), set_up: Adding::to(&self.set_up) }
    }
}

/// The starts of a block's games, noted a word of numbers at a time as a
/// scan reads their slots in order.
pub(super) struct Noting<'a> {
    standard: Adding<'a>,
    set_up: Adding<'a>,
}

impl Noting<'_> {
    /// Notes the start of game `number`, whose entry is `entry`.
    pub fn note(&mut self, number: u32, entry: &Entry) {
        match (entry.indexed(), entry.setup()) {
            (true, false) => self.standard.add(number),
            (true, true) => self.set_up.add(number),
            (false, _) => {}
        }
    }
}

/// A block of slots as the table gives it: where they start, their CRC, and
/// whether a scan found them to match it.
struct Block {
    at: u64,
    crc: u32,
    sound: AtomicBool,
}

impl Stream {
    pub fn open(path: &Path) -> Result<Stream, Bad> {
        let file = File::open(path).map_err(Bad::Io)?;
        let len = file.metadata().map_err(Bad::Io)?.len();
        let mut head = [0u8; HEADER_LEN];
        read_at(&file, 0, &mut head).map_err(Bad::Io)?;
        let header = Header::decode(&head).ok_or(Bad::Corrupt("stream header"))?;
        let (records, blocks) = (header.records(), u64::from(header.blocks));
        // The table must end the file and have a block for every BATCH
        // records, each of whose slots the file holds, before anything is
        // allocated from the counts.
        if blocks != records.div_ceil(BATCH as u64)
            || header.table_offset.checked_add(TABLE_ENTRY as u64 * blocks) != Some(len)
            || header.table_offset < HEADER_LEN as u64 + records * SLOT_BYTES as u64
            || !header.table_offset.is_multiple_of(ALIGN)
            || header.games > records
        {
            return Err(Bad::Corrupt("stream layout"));
        }
        let table_len = TABLE_ENTRY * blocks as usize;
        let memory = Hold::reserve_quietly(table_len + blocks as usize * size_of::<Block>()).map_err(|r| {
            if r == Refused::TooLarge { Bad::Corrupt("stream table larger than memory") } else { Bad::Busy }
        })?;
        let mut table = Vec::new();
        table.try_reserve_exact(table_len).map_err(|_| Bad::Busy)?;
        table.resize(table_len, 0);
        read_at(&file, header.table_offset, &mut table).map_err(Bad::Io)?;
        if crc32(&table) != header.table_crc {
            return Err(Bad::Corrupt("stream table"));
        }
        let mut placed = Vec::new();
        placed.try_reserve_exact(blocks as usize).map_err(|_| Bad::Busy)?;
        for (b, e) in table.as_chunks::<TABLE_ENTRY>().0.iter().enumerate() {
            let (at, crc) = (u64_at(e, 0), u32_at(e, 8));
            let slots = (records - b as u64 * BATCH as u64).min(BATCH as u64);
            if at < HEADER_LEN as u64
                || !at.is_multiple_of(ALIGN)
                || at.checked_add(slots * SLOT_BYTES as u64).is_none_or(|end| end > header.table_offset)
            {
                return Err(Bad::Corrupt("stream layout"));
            }
            placed.push(Block { at, crc, sound: AtomicBool::new(false) });
        }
        drop(table);
        let size = usize::try_from(len).map_err(|_| Bad::Corrupt("stream larger than memory"))?;
        let map = Map::new(&file, size).map_err(Bad::Io)?;
        let starts = Arc::new(KeptStarts::default());
        let weak: Weak<dyn Evict> = Arc::downgrade(&(Arc::clone(&starts) as Arc<dyn Evict>));
        register(weak);
        Ok(Stream { path: path.to_path_buf(), header, map, blocks: placed, starts, _memory: memory })
    }

    /// Record `number`, checked against its CRC.
    pub(super) fn record(&self, number: u32) -> Result<Record<'_>, Bad> {
        let (slot, tail) = self.place(number)?;
        if record_crc(number, &slot[..CRC_AT], tail) != u32_at(slot, CRC_AT) {
            return Err(Bad::Corrupt("stream record"));
        }
        Ok(Record::of(slot, tail))
    }

    /// Record `number` as a build reads back the stream it has just written
    /// (#147): placed within the file, but not checked against its CRC, which
    /// every answer checks. A pass of the build reads every record, and the
    /// tree's passes only the first bytes of each.
    pub(super) fn written(&self, number: u32) -> Result<Record<'_>, Bad> {
        let (slot, tail) = self.place(number)?;
        Ok(Record::of(slot, tail))
    }

    /// The number of block `block`'s first record, and the slots of its
    /// records in order, as a scan of every game reads them (#148): checked
    /// against the block's CRC the first time a scan reads them while the
    /// stream is open, not against their records' CRCs, which cover their
    /// tails too, and which a scan of every slot would read whole.
    pub(super) fn slots(&self, block: usize) -> Result<(u32, impl Iterator<Item = Slot<'_>>), Bad> {
        let placed = self.blocks.get(block).ok_or(Bad::Corrupt("stream block"))?;
        let at = placed.at as usize;
        // Every block but the last holds BATCH records; each block's slots lie
        // before the table, checked when it opened.
        let first = block as u64 * BATCH as u64;
        let count = self.header.records().saturating_sub(first).min(BATCH as u64) as usize;
        let bytes = self.map.bytes().get(at..at + count * SLOT_BYTES).ok_or(Bad::Corrupt("stream block"))?;
        // A race of two scans checks the block twice, which is harmless.
        if !placed.sound.load(Ordering::Relaxed) {
            if block_crc(block as u32, bytes) != placed.crc {
                return Err(Bad::Corrupt("stream block"));
            }
            placed.sound.store(true, Ordering::Relaxed);
        }
        let number =
            u32::try_from(u64::from(self.header.first_record) + first).map_err(|_| Bad::Corrupt("stream block"))?;
        Ok((number, bytes.as_chunks::<SLOT_BYTES>().0.iter().map(Slot::of)))
    }

    /// The starts of the games, once a scan of every slot has found them,
    /// and while the stream keeps them.
    pub(super) fn starts(&self) -> Option<Arc<Starts>> {
        self.starts.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Room for the starts of the games, which a scan of every slot finds
    /// block by block, the stream then keeping them ([`Stream::keep`]):
    /// `None` while it keeps them, or when the search budget has no room for
    /// them now, which they never take from what searches retained.
    pub(super) fn find_starts(&self) -> Option<Starts> {
        if self.starts().is_some() {
            return None;
        }
        let len = self.header.last_record as usize + 1;
        Some(Starts { standard: Members::new_quietly(len).ok()?, set_up: Members::new_quietly(len).ok()? })
    }

    /// Keeps the starts a scan of every slot found, each block's from slots
    /// found to match the block's CRC; of two scans that found them at once,
    /// the first's.
    pub(super) fn keep(&self, starts: Starts) {
        self.starts.0.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(|| Arc::new(starts));
    }

    /// The slot and the tail of record `number`, within the file.
    fn place(&self, number: u32) -> Result<(&[u8], &[u8]), Bad> {
        let i = number
            .checked_sub(self.header.first_record)
            .map(u64::from)
            .filter(|&i| i < self.header.records())
            .ok_or(Bad::Corrupt("stream record"))?;
        let bytes = self.map.bytes();
        // Every block's slots lie before the table, checked when it opened.
        let at = self.blocks.get((i / BATCH as u64) as usize).map(|b| b.at + i % BATCH as u64 * SLOT_BYTES as u64);
        let slot =
            at.and_then(|at| bytes.get(at as usize..at as usize + SLOT_BYTES)).ok_or(Bad::Corrupt("stream record"))?;
        let entry = Entry::decode(slot);
        let len = entry.tail_bytes().1;
        let tail = if len == 0 {
            &[][..]
        } else {
            let at = 2 * u64::from(entry.tail);
            at.checked_add(len as u64)
                .filter(|&end| at >= HEADER_LEN as u64 && end <= self.header.table_offset)
                .and_then(|end| bytes.get(at as usize..end as usize))
                .ok_or(Bad::Corrupt("stream tail"))?
        };
        Ok((slot, tail))
    }

    /// Record `number`'s directory entry.
    pub fn entry(&self, number: u32) -> Result<Entry, Bad> {
        Ok(self.record(number)?.entry)
    }

    /// Game `number`'s line; its words empty when the index does not hold it.
    pub fn game(&self, number: u32) -> Result<Game, Bad> {
        let record = self.record(number)?;
        Ok(Game { entry: record.entry, start: record.start()?, words: record.words().collect() })
    }

    /// Replays game `number`'s line to the first position that is `target`:
    /// the game, with the move played from there; `None` when the index does
    /// not hold the game, or its line never reaches the position, or first
    /// reaches it before the target's first ply ([`Target::beyond`]). The
    /// words were checked when the stream was built, so they are played
    /// unchecked. A line stops as soon as it can no longer reach the position
    /// (see [`Target`]), and one whose home pawns left in an order the
    /// position does not allow, or that ends before the first ply, is not
    /// played.
    pub fn find(&self, number: u32, target: &Target) -> Result<Option<Hit>, Bad> {
        let record = self.record(number)?;
        let entry = record.entry;
        if !entry.indexed()
            || u32::from(entry.plies) < target.from
            || (!entry.setup() && !entry.departures.allows(target.home))
        {
            return Ok(None);
        }
        let mut board = Replayer::new(record.start()?.unwrap_or_else(|| standard().clone()));
        if target.passed(&board) {
            return Ok(None);
        }
        let moves = moves();
        let mut words = record.words();
        let mut ply = 0u32;
        loop {
            let mv = match words.next() {
                Some(w) => Some(moves.get(usize::from(w)).copied().flatten().ok_or(Bad::Corrupt("stream word"))?),
                None => None,
            };
            if board.hash() == target.key {
                if ply < target.from {
                    return Ok(None);
                }
                return Ok(Some(Hit { mv: mv.map_or(NO_MOVE, pack_move), outcome: entry.outcome(), elo: entry.elo() }));
            }
            let Some(mv) = mv else { return Ok(None) };
            // Only a capture or a pawn's move changes what `passed` counts:
            // the men of the side not moving, and the pawns.
            let them = !board.side_to_move();
            let before = (board.colors(them), board.pieces(Piece::Pawn));
            board.play(mv);
            ply += 1;
            if (board.colors(them), board.pieces(Piece::Pawn)) != before && target.passed(&board) {
                return Ok(None);
            }
        }
    }
}

/// A record's slot alone: its entry, and the words of its prefix.
pub(super) struct Slot<'a> {
    pub entry: Entry,
    prefix: &'a [u8],
}

impl<'a> Slot<'a> {
    fn of(slot: &'a [u8; SLOT_BYTES]) -> Slot<'a> {
        let entry = Entry::decode(slot);
        let words = usize::from(entry.plies).min(PREFIX_WORDS);
        Slot { entry, prefix: &slot[PREFIX_AT..PREFIX_AT + 2 * words] }
    }

    /// The line's first words, up to [`PREFIX_WORDS`]: a set-up game's follow
    /// its start, which its tail holds.
    pub fn words(&self) -> impl Iterator<Item = u16> + use<'a> {
        self.prefix.as_chunks::<2>().0.iter().map(|w| u16::from_le_bytes(*w))
    }
}

/// A record of a stream: its entry, the words of its prefix, the words past
/// it, and its set-up start.
pub(super) struct Record<'a> {
    pub entry: Entry,
    prefix: &'a [u8],
    past: &'a [u8],
    setup: Option<&'a [u8]>,
}

impl<'a> Record<'a> {
    /// The record whose slot and tail, as long as its entry says, are these.
    fn of(slot: &'a [u8], tail: &'a [u8]) -> Record<'a> {
        let entry = Entry::decode(slot);
        let setup = entry.tail_bytes().0.min(tail.len());
        let plies = usize::from(entry.plies);
        let (start, past) = tail.split_at(setup);
        Record {
            entry,
            prefix: &slot[PREFIX_AT..PREFIX_AT + 2 * plies.min(PREFIX_WORDS)],
            past,
            setup: (setup > 0).then_some(start),
        }
    }

    pub fn words(&self) -> impl Iterator<Item = u16> + use<'a> {
        self.prefix.as_chunks::<2>().0.iter().chain(self.past.as_chunks::<2>().0).map(|w| u16::from_le_bytes(*w))
    }

    /// The words of its prefix, then those past it, as stored: a replay that
    /// takes them a slice at a time needs no test a word of which part it is
    /// in.
    pub fn word_parts(&self) -> (&'a [[u8; 2]], &'a [[u8; 2]]) {
        (self.prefix.as_chunks::<2>().0, self.past.as_chunks::<2>().0)
    }

    /// The set-up start; `None` for the standard one.
    pub fn start(&self) -> Result<Option<Board>, Bad> {
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
            table_offset: 128 + 99 * 64 + 4032,
            blocks: 1,
            table_crc: 7,
        };
        let e = h.encode();
        assert_eq!(Header::decode(&e), Some(h));
        let mut bad = e;
        bad[44] ^= 1;
        assert_eq!(Header::decode(&bad), None, "the header's CRC covers the build id");
        let mut v1 = e;
        v1[8] = 1;
        let crc = crc32(&v1[..124]);
        v1[124..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(Header::decode(&v1), None, "a stream of another version");
        let entry =
            Entry { tail: 70_000, plies: 65_535, flags: INDEXED | SETUP | 2400 << 2 | 1, departures: Departures(5) };
        assert_eq!(Entry::decode(&entry.encode()), entry);
        assert!(entry.indexed() && entry.setup());
        assert_eq!((entry.elo(), entry.outcome()), (2400, Outcome::Draw));
        assert_eq!(entry.tail_bytes(), (SETUP_BYTES, SETUP_BYTES + 2 * (65_535 - PREFIX_WORDS)));
    }

    /// Record `n`'s line in [`written`]: none for every seventh, which the
    /// index does not hold; else up to 399 words, some from a set-up start.
    fn line_of(n: u32) -> Option<(Vec<u16>, Option<[u8; SETUP_BYTES]>)> {
        if n.is_multiple_of(7) {
            return None;
        }
        let words = (0..n * 37 % 400).map(|k| (n.wrapping_mul(31) + k) as u16).collect();
        let fen = "4k3/8/8/8/8/8/P7/4K3 w - - 0 1";
        Some((words, n.is_multiple_of(11).then(|| setup_of(&Board::from_fen(fen).unwrap()))))
    }

    /// A stream of records `1..=records`, more than a block, written by two
    /// workers whose appends interleave and whose blocks end in reverse
    /// order; each block's tails fill the buffer more than once.
    fn written(name: &str, records: u32) -> PathBuf {
        let path = std::env::temp_dir().join(format!("bridge-stream-{name}-{}", std::process::id()));
        let writer = Writer::create(&path, 1, records).unwrap();
        let (mut a, mut b) = (writer.part().unwrap(), writer.part().unwrap());
        let split = BATCH as u32;
        a.begin(1, split);
        b.begin(split + 1, records);
        for n in 1..=split {
            for (part, number) in [(&mut a, n), (&mut b, n + split)] {
                if let Some((words, setup)) = line_of(number).filter(|_| number <= records) {
                    part.add(&Line::of(number, words, setup, Outcome::Draw)).unwrap();
                }
            }
        }
        b.end().unwrap();
        a.end().unwrap();
        drop((a, b));
        let header = writer.finish(3, 9).unwrap();
        assert_eq!((header.first_record, header.last_record, header.blocks), (1, records, 2));
        path
    }

    /// Every record reads back as written, wherever its block and its tail
    /// were appended, and the file ends with the table.
    #[test]
    fn a_stream_reads_back_as_written() {
        let records = BATCH as u32 + 700;
        let path = written("back", records);
        let stream = Stream::open(&path).unwrap();
        assert_eq!((stream.header.generation, stream.header.build_id), (3, 9));
        assert!(stream.blocks[1].at < stream.blocks[0].at, "the second block ended first");
        let (mut games, mut plies) = (0, 0);
        for n in 1..=records {
            let game = stream.game(n).unwrap();
            match line_of(n) {
                Some((words, setup)) => {
                    assert!(game.entry.indexed(), "{n}");
                    assert_eq!(game.words, words, "{n}");
                    assert_eq!(game.entry.setup(), setup.is_some(), "{n}");
                    assert_eq!(game.start.map(|b| setup_of(&b)), setup, "{n}");
                    assert_eq!(game.entry.outcome(), Outcome::Draw);
                    games += 1;
                    plies += words.len() as u64;
                }
                None => assert_eq!((game.entry, game.words.len()), (Entry::default(), 0), "{n}"),
            }
        }
        assert_eq!((stream.header.games, stream.header.plies), (games, plies));
        assert!(matches!(stream.game(0), Err(Bad::Corrupt(_))));
        assert!(matches!(stream.game(records + 1), Err(Bad::Corrupt(_))));
        drop(stream);
        std::fs::remove_file(&path).unwrap();
    }

    /// A byte changed anywhere a record reads, its slot, its CRC or its
    /// tail, fails that record and no other; two slots swapped fail both.
    /// A changed table, or a file cut short, does not open.
    #[test]
    fn a_damaged_record_is_never_read() {
        let records = BATCH as u32 + 20;
        let path = written("damage", records);
        let good = std::fs::read(&path).unwrap();
        let (slot_of, tail_of) = {
            let stream = Stream::open(&path).unwrap();
            let slot_of = |n: u32| {
                let i = u64::from(n - 1);
                (stream.blocks[(i / BATCH as u64) as usize].at + i % BATCH as u64 * SLOT_BYTES as u64) as usize
            };
            let slots: Vec<usize> = (1..=records).map(slot_of).collect();
            let tail = 2 * stream.entry(10).unwrap().tail as usize;
            (slots, tail)
        };
        let damaged = |at: usize, what: &str| {
            let bad = path.with_extension(what);
            let mut bytes = good.clone();
            bytes[at] ^= 0x10;
            std::fs::write(&bad, &bytes).unwrap();
            bad
        };
        // Game 10 has 370 words: its tail holds words 21 on.
        assert_eq!(line_of(10).unwrap().0.len(), 370);
        for (at, what) in [
            (slot_of[9] + 5, "plies"),
            (slot_of[9] + PREFIX_AT + 3, "prefix"),
            (slot_of[9] + 58, "zero"),
            (slot_of[9] + CRC_AT + 1, "crc"),
            (tail_of + 600, "tail"),
        ] {
            let bad = damaged(at, what);
            let stream = Stream::open(&bad).unwrap();
            assert!(matches!(stream.game(10), Err(Bad::Corrupt(_))), "{what}");
            assert!(matches!(stream.entry(10), Err(Bad::Corrupt(_))), "{what}");
            let target = Target::of(standard());
            assert!(matches!(stream.find(10, &target), Err(Bad::Corrupt(_))), "{what}");
            assert!(stream.game(9).is_ok() && stream.game(11).is_ok(), "{what}");
            drop(stream);
            std::fs::remove_file(&bad).unwrap();
        }
        // A record the index does not hold is checked as well.
        assert!(line_of(14).is_none());
        let bad = damaged(slot_of[13] + 7, "unindexed");
        assert!(matches!(Stream::open(&bad).unwrap().game(14), Err(Bad::Corrupt(_))));
        std::fs::remove_file(&bad).unwrap();
        // Two slots swapped: each is sound but not where it belongs.
        let bad = path.with_extension("swapped");
        let mut bytes = good.clone();
        let (x, y) = (slot_of[1], slot_of[BATCH + 1]);
        for k in 0..SLOT_BYTES {
            bytes.swap(x + k, y + k);
        }
        std::fs::write(&bad, &bytes).unwrap();
        let stream = Stream::open(&bad).unwrap();
        assert!(matches!(stream.game(2), Err(Bad::Corrupt(_))));
        assert!(matches!(stream.game(BATCH as u32 + 2), Err(Bad::Corrupt(_))));
        drop(stream);
        std::fs::remove_file(&bad).unwrap();
        let table = Header::decode(&good).unwrap().table_offset as usize;
        for (at, what) in [(table + 3, "table"), (40, "header")] {
            let bad = damaged(at, what);
            assert!(matches!(Stream::open(&bad), Err(Bad::Corrupt(_))), "{what}");
            std::fs::remove_file(&bad).unwrap();
        }
        let bad = path.with_extension("short");
        std::fs::write(&bad, &good[..good.len() - 8]).unwrap();
        assert!(matches!(Stream::open(&bad), Err(Bad::Corrupt(_))));
        std::fs::remove_file(&bad).unwrap();
        std::fs::remove_file(&path).unwrap();
    }

    /// A scan reads a block's slots only when they match the block's CRC: a
    /// byte changed in a slot, or two slots exchanged within a block or
    /// across two, fail the blocks they are in and no other. A block found
    /// sound is marked so, and one that fails is not.
    #[test]
    fn a_damaged_block_is_never_scanned() {
        let records = BATCH as u32 + 20;
        let path = written("blocks", records);
        let good = std::fs::read(&path).unwrap();
        let slot_of: Vec<usize> = {
            let stream = Stream::open(&path).unwrap();
            for block in 0..2 {
                let (first, slots) = stream.slots(block).unwrap();
                assert_eq!(first, 1 + (block * BATCH) as u32);
                let entries: Vec<Entry> = slots.map(|s| s.entry).collect();
                let numbers = first..(first + BATCH as u32).min(records + 1);
                assert_eq!(entries, numbers.map(|n| stream.entry(n).unwrap()).collect::<Vec<_>>(), "{block}");
                assert!(stream.blocks[block].sound.load(Ordering::Relaxed));
            }
            let slot_of = |n: u32| {
                let i = (n - 1) as usize;
                stream.blocks[i / BATCH].at as usize + i % BATCH * SLOT_BYTES
            };
            (1..=records).map(slot_of).collect()
        };
        enum Change {
            Byte(u32, usize),
            Exchange(u32, u32),
        }
        let last = BATCH as u32 + 3;
        for (what, change, sound) in [
            ("entry", Change::Byte(2, 5), [false, true]),
            ("prefix", Change::Byte(2, PREFIX_AT + 3), [false, true]),
            ("zero", Change::Byte(2, 58), [false, true]),
            ("crc", Change::Byte(2, CRC_AT + 1), [false, true]),
            ("last-block", Change::Byte(last, 0), [true, false]),
            ("within", Change::Exchange(2, 9), [false, true]),
            ("across", Change::Exchange(2, last), [false, false]),
        ] {
            let bad = path.with_extension(what);
            let mut bytes = good.clone();
            let slot = |n: u32| slot_of[n as usize - 1];
            match change {
                Change::Byte(n, k) => bytes[slot(n) + k] ^= 0x10,
                Change::Exchange(x, y) => {
                    for k in 0..SLOT_BYTES {
                        bytes.swap(slot(x) + k, slot(y) + k);
                    }
                }
            }
            std::fs::write(&bad, &bytes).unwrap();
            let stream = Stream::open(&bad).unwrap();
            for (block, sound) in sound.into_iter().enumerate() {
                assert_eq!(stream.slots(block).is_ok(), sound, "{what}, block {block}");
                assert_eq!(stream.blocks[block].sound.load(Ordering::Relaxed), sound, "{what}, block {block}");
                if !sound {
                    assert!(matches!(stream.slots(block), Err(Bad::Corrupt(_))), "{what}: fails again");
                }
            }
            drop(stream);
            std::fs::remove_file(&bad).unwrap();
        }
        std::fs::remove_file(&path).unwrap();
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
