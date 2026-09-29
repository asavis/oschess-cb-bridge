//! The deep section of an index (#133): for each bucket of structures
//! ([`super::format::structure`]), the games whose main line holds a
//! structure of that bucket past [`super::format::MAX_PLY`], each with the
//! structure's print ([`deep_print`]) and marked when the game holds it
//! beyond the tree's plies. The tree counts the games that reach a position
//! it holds within its plies; any other game that reaches a position is
//! looked for in the games of its structure's bucket, which are few, by
//! replaying them (#146). A game holds a position's structure wherever it
//! reaches it, so only the games with the structure's print are replayed,
//! and for a position the tree holds only those marked.
//!
//! A build writes the section in passes over the move stream it has just
//! written (#147), a range of buckets at a time, as many as the build's share
//! of the budget holds. Each worker replays the whole lines of the games it
//! takes, following only their structures through the move words
//! ([`Tracker`]), and keeps the postings ([`posting`]) of the pass's buckets in
//! a buffer of its own, sorted and freed of repeats when it fills; when that
//! leaves too little room, the pass ends at an earlier bucket for every
//! worker, or, when the pass's first bucket alone fills the room, at a game
//! of that bucket, and the next pass goes on with the bucket's later games.
//! The workers then take the pass's blocks in order, each merging one
//! block's postings from every buffer, and write them once the blocks before
//! have been placed: per block of [`BLOCK_BUCKETS`] buckets, each bucket's
//! posting count and its postings by game, then print, each a varint of the
//! game's difference from the one before, shifted left by eight, the print
//! and the mark; the block covered by a CRC-32 in the section's table. A
//! pass may end inside a block, and inside a bucket, which the next one goes
//! on with: the bucket's count is the one its first pass counted, and its
//! postings go on from the game written last, so that any number of passes
//! writes the same bytes.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use chesscore::{Board, Color, Piece};

use cbformat::movetable::{self, Captured, FIRST_CASTLE_960, MoveWord};

use crate::indexdir::crc32_update;
use crate::search::SearchError;
use crate::search::memory::{Cancel, Refused};
use crate::search::workers::{self, threads};

use super::build::{Chunks, Out, PLANNED, Turns, corrupt, from_bad};
use super::format::{
    DEEP_BLOCK_BITS, DEEP_BLOCK_ENTRY, MAX_PLY, PRINT_BITS, PRUNE_PLY, STRUCTURE_PIECES, deep_bucket, deep_print,
    piece_shift, read_varint, structure_of, varint,
};
use super::runs::{Limits, PassTime, Progress, Room};
use super::source::MAX_STRUCTURES;
use super::stream::{self, Stream};

/// Buckets per block.
pub const BLOCK_BUCKETS: usize = 1 << DEEP_BLOCK_BITS;
/// The least room a worker's postings take: twice a game's most, so that a
/// pass that ends at a game keeps some.
const MIN_WORKER_POSTINGS: usize = 2 * MAX_STRUCTURES;
/// The bytes of blocks a worker makes and hands over at once at most, and
/// its share of those kept until their turn to be written comes: a piece is
/// handed over once it holds half of it, even inside a bucket, and so never
/// grows past it.
const OUT_BYTES: usize = 1 << 20;
/// The postings of a bucket gathered to be sorted, at most.
const GATHERED: usize = 4096;
/// What a worker holds besides its postings: the bytes it makes, its share
/// of those kept, and a block's buffers to merge.
pub const WORKER_BYTES: usize = 2 * OUT_BYTES + (1 << 10) + 8 * GATHERED;
/// The bits of a game in a point of the section: `bucket << 32 | game`,
/// where a pass starts and ends. A bucket's first point is its game 0, which
/// no game is.
const GAME_BITS: u32 = 32;

/// Game `game` holds `structure`, of a bucket of `bits` bits, `beyond` the
/// tree's plies or only within them: `bucket << 40 | game << 8 | print << 1`,
/// and 1 when only within. A bucket's postings sort by game, then print, and
/// a game's posting beyond before its posting within, which is the one kept
/// of the two.
pub fn posting(structure: u64, bits: u8, game: u32, beyond: bool) -> u64 {
    let (bucket, print) = (deep_bucket(structure, bits), deep_print(structure, bits));
    u64::from(bucket) << 40 | u64::from(game) << 8 | u64::from(print) << 1 | u64::from(!beyond)
}

/// A posting's bucket.
fn bucket_of(p: u64) -> u64 {
    p >> 40
}

/// A posting's bucket and game: the point it lies at.
fn point(p: u64) -> u64 {
    p >> 8
}

/// A posting's game.
fn game_of(p: u64) -> u64 {
    p >> 8 & 0xffff_ffff
}

/// A posting's bucket, game and print, apart from its mark.
fn place(p: u64) -> u64 {
    p >> 1
}

/// The first point of block `block`.
fn block_point(block: u64) -> u64 {
    block << (u32::from(DEEP_BLOCK_BITS) + GAME_BITS)
}

/// What a move word does to a structure, from the move table, so that a
/// word is played without a branch: the pawns it takes off their squares or
/// puts on theirs, white's then black's, which a xor applies; the change to
/// the pieces' counts, a piece taken or a pawn promoted, which a wrapping add
/// applies; and whether it may change the structure ([`CHANGED`]) and names
/// a move of standard chess ([`NAMED`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
struct Effect {
    pawns: [u64; 2],
    pieces: u64,
    flags: u64,
}

/// An [`Effect`] flag: a pawn's move or a capture.
const CHANGED: u64 = 1;
/// An [`Effect`] flag: a word that names a move of standard chess.
const NAMED: u64 = 2;

/// Room for the distinct effects of the move table's words, a power of two
/// so that an index needs no check: its 45,357 words below the Chess960
/// castlings have 1,282.
const EFFECTS: usize = 2048;

/// The index of each word's effect among the distinct ones, 0 for a word
/// that names no move of standard chess, and the effects: two lookups a
/// word, into tables of 2 bytes a word and 32 an effect of which 131 KB are
/// in use, which a core's cache holds, where an effect a word would take
/// 1.4 MB.
fn effects() -> (&'static [u16; 1 << 16], &'static [Effect; EFFECTS]) {
    type Tables = (Box<[u16; 1 << 16]>, Box<[Effect; EFFECTS]>);
    static TABLES: OnceLock<Tables> = OnceLock::new();
    let (of, effects) = TABLES.get_or_init(|| {
        let (mut of, mut effects) = (vec![0u16; 1 << 16], vec![Effect::default()]);
        let mut index = std::collections::HashMap::new();
        for word in 0..FIRST_CASTLE_960 {
            if let Some(e) = effect_of(word) {
                of[usize::from(word)] = *index.entry(e).or_insert_with(|| {
                    effects.push(e);
                    effects.len() as u16 - 1
                });
            }
        }
        assert!(effects.len() <= EFFECTS, "{} effects", effects.len());
        effects.resize(EFFECTS, Effect::default());
        let boxed = |v: Vec<u16>| v.into_boxed_slice().try_into().expect("a word's index each");
        (boxed(of), effects.into_boxed_slice().try_into().expect("room for every effect"))
    });
    (of, effects)
}

/// What `word` does to a structure; `None` when it names no move of standard
/// chess.
fn effect_of(word: u16) -> Option<Effect> {
    let kind = |p: movetable::Piece| match p {
        movetable::Piece::Knight => 0,
        movetable::Piece::Bishop => 1,
        movetable::Piece::Rook => 2,
        _ => 3,
    };
    match movetable::decode(word)? {
        MoveWord::Normal { color, piece, from, to, captured, promotion } => {
            let (us, them) = match color {
                movetable::Color::White => (Color::White, Color::Black),
                movetable::Color::Black => (Color::Black, Color::White),
            };
            let pawn = piece == movetable::Piece::Pawn;
            let (from, to) = (u64::from(from & 63), u64::from(to & 63));
            let mut pawns = [0; 2];
            // A pawn leaves its square, and stands on the other one unless it
            // becomes a piece there.
            if pawn {
                pawns[us.index()] = 1 << from | u64::from(promotion.is_none()) << to;
            }
            // The pawn taken en passant stands beside the one that takes it:
            // on the rank it leaves, the file it reaches.
            pawns[them.index()] = match captured {
                Captured::Pawn => 1 << to,
                Captured::EnPassant => 1 << (from & 56 | to & 7),
                _ => 0,
            };
            let taken_piece = match captured {
                Captured::Knight => Some(movetable::Piece::Knight),
                Captured::Bishop => Some(movetable::Piece::Bishop),
                Captured::Rook => Some(movetable::Piece::Rook),
                Captured::Queen => Some(movetable::Piece::Queen),
                _ => None,
            };
            let mut delta = 0i64;
            if let Some(p) = taken_piece {
                delta -= 1 << piece_shift(kind(p), them);
            }
            if let Some(p) = promotion.filter(|_| pawn) {
                delta += 1 << piece_shift(kind(p), us);
            }
            let changes = pawn || captured != Captured::Nothing;
            Some(Effect { pawns, pieces: delta as u64, flags: NAMED | if changes { CHANGED } else { 0 } })
        }
        MoveWord::Castle { .. } => Some(Effect { flags: NAMED, ..Effect::default() }),
        _ => None,
    }
}

/// The words a line's replay plays at a time past the tree's plies, noting
/// the changes of its structure ([`Tracker::play_noting`]).
const CHANGES: usize = 64;

/// The parts of a line's structure after a word that may have changed it.
#[derive(Clone, Copy, Default)]
struct Change {
    pawns: [u64; 2],
    pieces: u64,
}

/// A line's structure followed through its move words alone: each side's
/// pawns and its pieces counted by kind, as [`super::format::structure`]
/// hashes them. A word names the piece it moves, what it takes and what a
/// pawn becomes, and the stream's words were checked when it was written, so
/// no board is needed.
#[derive(Clone, Copy)]
pub struct Tracker {
    pawns: [u64; 2],
    pieces: u64,
    of: &'static [u16; 1 << 16],
    effects: &'static [Effect; EFFECTS],
}

impl PartialEq for Tracker {
    fn eq(&self, other: &Tracker) -> bool {
        (self.pawns, self.pieces) == (other.pawns, other.pieces)
    }
}

impl std::fmt::Debug for Tracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tracker").field("pawns", &self.pawns).field("pieces", &self.pieces).finish()
    }
}

impl Tracker {
    pub fn of(board: &Board) -> Tracker {
        let mut pieces = 0u64;
        for (i, piece) in STRUCTURE_PIECES.into_iter().enumerate() {
            for color in [Color::White, Color::Black] {
                pieces += u64::from(board.colored(piece, color).count_ones()) << piece_shift(i, color);
            }
        }
        let pawns = |color| board.colored(Piece::Pawn, color);
        let (of, effects) = effects();
        Tracker { pawns: [pawns(Color::White), pawns(Color::Black)], pieces, of, effects }
    }

    /// The standard start's, counted once.
    pub fn standard() -> Tracker {
        static STANDARD: OnceLock<Tracker> = OnceLock::new();
        *STANDARD.get_or_init(|| Tracker::of(stream::standard()))
    }

    /// The effect of `word`.
    #[inline(always)]
    fn effect(&self, word: u16) -> &'static Effect {
        &self.effects[usize::from(self.of[usize::from(word)]) & (EFFECTS - 1)]
    }

    /// Plays `word`: whether the structure may have changed, which only a
    /// pawn's move or a capture does; `None` for a word that names no move
    /// of standard chess.
    #[inline]
    pub fn play(&mut self, word: u16) -> Option<bool> {
        let e = self.effect(word);
        if e.flags & NAMED == 0 {
            return None;
        }
        self.apply(e);
        Some(e.flags & CHANGED != 0)
    }

    /// Plays `words`, as [`Tracker::play`] plays each, without a branch a
    /// word: `None` when one names no move of standard chess.
    #[inline]
    fn play_all(&mut self, words: &[[u8; 2]]) -> Option<()> {
        let mut named = NAMED;
        for w in words {
            let e = self.effect(u16::from_le_bytes(*w));
            named &= e.flags;
            self.apply(e);
        }
        (named & NAMED != 0).then_some(())
    }

    /// Plays `words`, [`CHANGES`] at most, and notes in `changes` the parts
    /// of the structure after each word that may have changed it, a pawn's
    /// move or a capture: without a branch a word, since which words do is
    /// unpredictable. The changes noted; `None` when a word names no move of
    /// standard chess.
    #[inline]
    fn play_noting(&mut self, words: &[[u8; 2]], changes: &mut [Change; CHANGES]) -> Option<usize> {
        let (mut noted, mut named) = (0, NAMED);
        for w in words {
            let e = self.effect(u16::from_le_bytes(*w));
            named &= e.flags;
            self.apply(e);
            // No more noted than words played, so within `changes`.
            changes[noted & (CHANGES - 1)] = Change { pawns: self.pawns, pieces: self.pieces };
            noted += (e.flags & CHANGED) as usize;
        }
        (named & NAMED != 0).then_some(noted)
    }

    /// Plays effect `e`, as [`Tracker::play`] does: a pawn leaves a square it
    /// stands on and reaches one no pawn of its side stands on, and one taken
    /// stood where it is taken, as the words were checked.
    #[inline(always)]
    fn apply(&mut self, e: &Effect) {
        self.pawns[0] ^= e.pawns[0];
        self.pawns[1] ^= e.pawns[1];
        self.pieces = self.pieces.wrapping_add(e.pieces);
    }

    pub fn structure(&self) -> u64 {
        structure_of(self.pawns[0], self.pawns[1], self.pieces)
    }
}

/// The deep section as written: its postings, and where its table is.
pub(super) struct Section {
    pub postings: u64,
    pub table_offset: u64,
    pub table_crc: u32,
}

/// Writes the deep section of the games in `stream` to `out`, of buckets of
/// `bits` bits, then its table, within `share` bytes of the budget: `counts`
/// holds the postings of each block as the stream's pass counted them.
/// Returns the postings kept, a game once per structure print in a bucket,
/// marked when any of its postings there is.
pub(super) fn write(
    stream: &Stream,
    counts: &[u64],
    bits: u8,
    out: &mut Out,
    progress: &Progress,
    share: usize,
    limits: &Limits,
) -> Result<Section, SearchError> {
    progress.start("structures", counts.iter().sum());
    let table_bytes = counts.len() * DEEP_BLOCK_ENTRY;
    // Half the workers at most, as many as the share holds beside a quarter
    // of it for the postings, one at least.
    let games = stream.header.records();
    let fit = (share.saturating_sub(table_bytes) / 4 * 3 / WORKER_BYTES).max(1);
    let want = threads().div_ceil(2).min(games.div_ceil(64) as usize).min(fit).max(1);
    let room = share.checked_sub(table_bytes + want * WORKER_BYTES).ok_or(SearchError::TooLarge)?;
    let least = MIN_WORKER_POSTINGS * 8;
    // No more than the postings take, twice over: a small database's build
    // holds a little of the budget, however large its share, and takes one
    // pass.
    let postings: u64 = counts.iter().sum();
    let usable = usize::try_from(postings).unwrap_or(usize::MAX).saturating_mul(2 * 8).saturating_add(least);
    let room = room.min(usable).min(limits.pass_bytes.unwrap_or(usize::MAX));
    if room < least {
        return Err(SearchError::TooLarge);
    }
    let (_memory, want, room) =
        Room { fixed: table_bytes, each: WORKER_BYTES, workers: want, least, room }.reserve(progress)?;
    let capacity = room / 8;
    let want = want.min(capacity / MIN_WORKER_POSTINGS);
    let mut table = Vec::new();
    table.try_reserve_exact(table_bytes).map_err(|_| Refused::Busy)?;
    let mut sink = Sink { at: out.offset, table, start: 0, crc: !0, postings: 0 };
    let (mut lo, mut split) = (0, Split::default());
    while lo < block_point(counts.len() as u64) {
        // As many blocks as the room holds, but for a little, one at least,
        // the one the last pass ended in counted whole.
        let mut end = (lo >> (u32::from(DEEP_BLOCK_BITS) + GAME_BITS)) as usize;
        let first = end;
        let mut planned = 0;
        while end < counts.len() && (end == first || planned + counts[end] <= (capacity * PLANNED / 100) as u64) {
            planned += counts[end];
            end += 1;
        }
        let hi = AtomicU64::new(block_point(end as u64));
        progress.deep_passes.fetch_add(1, Ordering::Relaxed);
        let rest = AtomicU64::new(0);
        let pass = Pass { stream, bits, lo, hi: &hi, split, rest, capacity, planned, progress };
        let started = Instant::now();
        let buffers = pass.collect(want)?;
        let replayed = Instant::now();
        let hi = hi.load(Ordering::Relaxed);
        split = write_blocks(&buffers, &pass, hi, counts, &mut sink, out, want)?.unwrap_or_default();
        out.sync_behind();
        let time = PassTime { replay: replayed - started, write: replayed.elapsed() };
        progress.time(|t| t.deep.push(time));
        lo = hi;
    }
    out.offset = sink.at;
    let table_offset = out.offset;
    out.put(&sink.table)?;
    Ok(Section { postings: sink.postings, table_offset, table_crc: !crc32_update(!0, &sink.table) })
}

/// One pass: the points ([`GAME_BITS`]) from `lo` to `hi`, `hi` lowered by a
/// worker whose postings do not fit.
struct Pass<'a> {
    stream: &'a Stream,
    bits: u8,
    lo: u64,
    hi: &'a AtomicU64,
    /// Where the pass before ended, when inside the bucket that `lo` lies
    /// in.
    split: Split,
    /// The postings of the bucket that `lo` lies in from `lo` on, each once,
    /// whatever the pass ends at: the bucket's count when the pass starts it.
    rest: AtomicU64,
    /// The postings all workers' buffers hold together, and those planned.
    capacity: usize,
    planned: u64,
    progress: &'a Progress,
}

/// A bucket that a pass ended inside: the game it put last, and the postings
/// of the bucket's count left for the passes after it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Split {
    last: u64,
    left: u64,
}

/// A worker's postings in a pass: its buffer of `cap`, and those of the
/// pass's first bucket it counted ([`Pass::rest`]); and what the replay of a
/// line notes as it goes, the changes of a run of words and the postings
/// that lie in the pass.
struct Kept {
    buf: Vec<u64>,
    cap: usize,
    rest: u64,
    changes: [Change; CHANGES],
    found: [u64; MAX_STRUCTURES],
}

/// What the replay of a line has found: its structures, those of them that
/// lie in the pass, and the prints of those in the pass's first bucket.
#[derive(Default)]
struct Replayed {
    structures: usize,
    found: usize,
    prints: u128,
}

impl Pass<'_> {
    /// Each worker's postings of the pass, sorted and freed of repeats, of
    /// up to `want` workers.
    fn collect(&self, want: usize) -> Result<Vec<Vec<u64>>, SearchError> {
        let (first, last) = (self.stream.header.first_record, self.stream.header.last_record);
        let chunks = Chunks::new(first, last, want);
        // Room for twice what a chunk adds, about.
        let spare = (2 * self.planned * chunks.size()).div_ceil(self.stream.header.records().max(1)) as usize;
        workers::run(want, 0, &Cancel::never(), |w| {
            let cap = self.capacity / w.count;
            let mut kept = Kept {
                buf: Vec::new(),
                cap,
                rest: 0,
                changes: [Change::default(); CHANGES],
                found: [0; MAX_STRUCTURES],
            };
            kept.buf.try_reserve_exact(cap).map_err(|_| Refused::Busy)?;
            let mut taker = chunks.taker();
            loop {
                // A background build gives way to foreground work before it
                // takes its next chunk, so that none waits for it (#149).
                self.progress.give_way();
                let Some((lo, hi)) = taker.take(kept.buf.len() + spare > cap) else { break };
                if w.stopped() || self.progress.stopped() {
                    return Err(SearchError::Superseded);
                }
                for game in lo..=hi {
                    self.replay(game, &mut kept)?;
                }
            }
            self.rest.fetch_add(kept.rest, Ordering::Relaxed);
            let mut buf = kept.buf;
            buf.sort_unstable();
            buf.dedup_by_key(|p| place(*p));
            Ok(buf)
        })
    }

    /// Adds the postings of game `game`'s structures past the tree's plies
    /// that lie in the pass to `kept`: each once, in the order its line holds
    /// them, as the walk that wrote the stream met them, and marked when the
    /// line holds it beyond [`MAX_PLY`], as it holds every one past the
    /// tree's plies.
    fn replay(&self, game: u32, kept: &mut Kept) -> Result<(), SearchError> {
        let path = &self.stream.path;
        let record = self.stream.written(game).map_err(|e| from_bad(path, e))?;
        let entry = record.entry;
        if !entry.indexed() || entry.plies <= u16::from(PRUNE_PLY) {
            return Ok(());
        }
        let start = record.start().map_err(|e| from_bad(path, e))?;
        let mut line = start.as_ref().map_or_else(Tracker::standard, Tracker::of);
        let word = || corrupt(path, "stream word");
        // The tree's plies hold no structure of the section: they are the
        // prefix's words, and the words past it follow ply 21, beyond the
        // tree's plies.
        const { assert!(stream::PREFIX_WORDS == PRUNE_PLY as usize + 1 && PRUNE_PLY >= MAX_PLY) };
        let (prefix, past) = record.word_parts();
        line.play_all(prefix).ok_or_else(word)?;
        let within = Within::of(self.lo, self.hi.load(Ordering::Relaxed), self.bits, game);
        let mut held = line.structure();
        let mut replayed = Replayed::default();
        for words in past.chunks(CHANGES) {
            let noted = line.play_noting(words, &mut kept.changes).ok_or_else(word)?;
            for c in &kept.changes[..noted] {
                let s = structure_of(c.pawns[0], c.pawns[1], c.pieces);
                if s != held {
                    find(held, &within, &mut replayed, &mut kept.found);
                    held = s;
                }
            }
        }
        find(held, &within, &mut replayed, &mut kept.found);
        if replayed.prints != 0 {
            kept.rest += u64::from(replayed.prints.count_ones());
        }
        for i in 0..replayed.found {
            let p = kept.found[i];
            if point(p) >= self.hi.load(Ordering::Relaxed) {
                continue;
            }
            if kept.buf.len() >= kept.cap {
                make_room(&mut kept.buf, kept.cap, self.lo, self.hi)?;
            }
            if point(p) < self.hi.load(Ordering::Relaxed) {
                kept.buf.push(p);
            }
        }
        Ok(())
    }
}

/// The buckets whose point of a game lies in a pass: those from `first` on,
/// `span` of them; the pass's first bucket, when the game's point there
/// counts ([`Pass::rest`]), else none; the game as a posting holds it; and
/// the shift that takes a structure's bucket and print.
struct Within {
    first: u64,
    span: u64,
    counted: u64,
    game: u64,
    shift: u32,
}

impl Within {
    /// The buckets of `bits` bits whose point of game `game` lies from `lo`
    /// on and before `hi`: the pass as far as it reached when the game's line
    /// began, as `hi` only comes down, and each posting found is looked at
    /// again as it is kept.
    fn of(lo: u64, hi: u64, bits: u8, game: u32) -> Within {
        let g = u64::from(game);
        let (lo_game, hi_game) = (lo & ((1 << GAME_BITS) - 1), hi & ((1 << GAME_BITS) - 1));
        // A point `bucket << GAME_BITS | g` is at `lo` or past it when the
        // bucket is past `lo`'s, or `lo`'s with `g` at `lo`'s game or past
        // it; likewise before `hi`.
        let first = (lo >> GAME_BITS) + u64::from(g < lo_game);
        let end = (hi >> GAME_BITS) + u64::from(g < hi_game);
        let counted = if g >= lo_game { lo >> GAME_BITS } else { u64::MAX };
        let shift = 64 - u32::from(bits) - u32::from(PRINT_BITS);
        Within { first, span: end.saturating_sub(first), counted, game: g << 8, shift }
    }
}

/// Notes a line's posting of `structure` in `found` when it lies `within`
/// the pass: the line's first [`MAX_STRUCTURES`] structures, as the walk kept
/// them, without a branch on where the structure's bucket lies, which is
/// unpredictable. One of the pass's first bucket is counted, once per print,
/// wherever the pass ends.
#[inline(always)]
fn find(structure: u64, within: &Within, replayed: &mut Replayed, found: &mut [u64; MAX_STRUCTURES]) {
    if replayed.structures >= MAX_STRUCTURES {
        return;
    }
    replayed.structures += 1;
    // The structure's bucket, then its print, as [`posting`] takes them.
    let x = structure >> within.shift;
    let (bucket, print) = (x >> PRINT_BITS, x & ((1 << PRINT_BITS) - 1));
    if bucket == within.counted {
        count_print(replayed, print);
    }
    // Fewer found than structures, so within `found`; beyond the tree's
    // plies, as [`posting`] marks it.
    if let Some(f) = found.get_mut(replayed.found) {
        *f = bucket << 40 | within.game | print << 1;
    }
    replayed.found += usize::from(bucket.wrapping_sub(within.first) < within.span);
}

/// Counts `print` among the prints of a line's structures in the pass's
/// first bucket, which few lines hold.
#[cold]
fn count_print(replayed: &mut Replayed, print: u64) {
    replayed.prints |= 1 << print;
}

/// Makes room in `buf`, a worker's full buffer of `cap` postings in a pass of
/// the points from `lo` to `hi`: its postings sorted and freed of repeats,
/// and when they still take three quarters of it, the pass ended for every
/// worker where about half are kept: at a bucket, which may lie inside a
/// block, or, when the pass's first bucket holds that half, at a game of
/// that bucket. A game holds half a worker's least room of postings at most
/// ([`MIN_WORKER_POSTINGS`]), so that a pass always keeps some; one that
/// alone left no room would be too large for the share.
fn make_room(buf: &mut Vec<u64>, cap: usize, lo: u64, hi: &AtomicU64) -> Result<(), SearchError> {
    buf.sort_unstable();
    let end = hi.load(Ordering::Relaxed);
    buf.truncate(buf.partition_point(|&p| point(p) < end));
    buf.dedup_by_key(|p| place(*p));
    if buf.len() > cap / 4 * 3 {
        let middle = buf[cap / 2];
        let cut = if bucket_of(middle) > lo >> GAME_BITS {
            bucket_of(middle) << GAME_BITS
        } else {
            point(middle).max(lo + 1)
        };
        hi.fetch_min(cut, Ordering::Relaxed);
        buf.truncate(buf.partition_point(|&p| point(p) < cut));
        if buf.len() > cap - cap / 8 {
            return Err(SearchError::TooLarge);
        }
    }
    Ok(())
}

/// Where the section's blocks go, one after another; a block begun in one
/// pass may end in the next.
struct Sink {
    /// Where the next bytes go.
    at: u64,
    table: Vec<u8>,
    /// Where the block being placed starts, and its CRC so far.
    start: u64,
    crc: u32,
    postings: u64,
}

/// Bytes of a block, made and handed over: its first when `begins`, its last
/// when `ends`, `whole` the CRC of a whole block made at once; the postings
/// they hold, and the postings the stream's pass counted in the block when
/// they end it.
struct Made {
    bytes: Vec<u8>,
    begins: bool,
    ends: bool,
    whole: Option<u32>,
    kept: u64,
    done: u64,
}

impl Sink {
    /// Places the bytes `made` after those placed, and ends their block in
    /// the table when they end it: the bytes, and where they go.
    fn place(&mut self, made: Made, progress: &Progress) -> Result<(u64, Vec<u8>), SearchError> {
        if made.begins {
            self.start = self.at;
            self.crc = !0;
        }
        if made.whole.is_none() {
            self.crc = crc32_update(self.crc, &made.bytes);
        }
        let at = self.at;
        self.at += made.bytes.len() as u64;
        self.postings += made.kept;
        if made.ends {
            let len = u32::try_from(self.at - self.start).map_err(|_| SearchError::TooLarge)?;
            self.table.extend(self.start.to_le_bytes());
            self.table.extend(len.to_le_bytes());
            self.table.extend(made.whole.unwrap_or(!self.crc).to_le_bytes());
            progress.done.fetch_add(made.done, Ordering::Relaxed);
        }
        Ok((at, made.bytes))
    }
}

/// Writes the postings of the sorted `buffers` from `pass.lo` to `hi` on up
/// to `want` workers, block by block, in order. A bucket that `hi` lies
/// inside is the pass's first ([`make_room`]): returns it, split.
fn write_blocks(
    buffers: &[Vec<u64>],
    pass: &Pass<'_>,
    hi: u64,
    counts: &[u64],
    sink: &mut Sink,
    out: &Out,
    want: usize,
) -> Result<Option<Split>, SearchError> {
    let block_bits = u32::from(DEEP_BLOCK_BITS) + GAME_BITS;
    let first = pass.lo >> block_bits;
    let units = ((hi - 1) >> block_bits) - first + 1;
    let progress = pass.progress;
    let place = |sink: &mut &mut Sink, made: Made| sink.place(made, progress);
    let write = |(at, bytes): (u64, Vec<u8>)| out.write(at, &bytes);
    let turns = Turns::new(units as usize, want * OUT_BYTES, sink, &place, &write);
    let (rest, split) = (pass.rest.load(Ordering::Relaxed), Mutex::new(None));
    let miscounted = || corrupt(&pass.stream.path, "the move stream does not replay to the postings it counted");
    workers::run(want, 0, &Cancel::never(), |w| {
        let stopped = || w.stopped() || progress.stopped();
        let mut heads: Vec<&[u64]> = Vec::new();
        heads.try_reserve_exact(buffers.len()).map_err(|_| Refused::Busy)?;
        let mut gathered: Vec<u64> = Vec::new();
        gathered.try_reserve_exact(GATHERED).map_err(|_| Refused::Busy)?;
        loop {
            // A background build gives way to foreground work before it
            // takes its next block, so that none waits for it (#149).
            progress.give_way();
            let Some(unit) = turns.take() else { break };
            if stopped() {
                return Err(SearchError::Superseded);
            }
            let block = first + unit as u64;
            let (block_lo, block_hi) = (block_point(block), block_point(block + 1));
            let (from, to) = (pass.lo.max(block_lo), hi.min(block_hi));
            heads.clear();
            for b in buffers {
                let at = b.partition_point(|&p| point(p) < from);
                let end = at + b[at..].partition_point(|&p| point(p) < to);
                if end > at {
                    heads.push(&b[at..end]);
                }
            }
            // The bytes made, handed over in pieces, each ended by a bucket
            // or, inside a crowded one, by half of `OUT_BYTES`: its first of
            // the block when `begins`, and the postings of the buckets it
            // ends. What a piece holds in the budget is its allocation.
            let (begins, kept) = (Cell::new(from == block_lo), Cell::new(0));
            let mut hand = |bytes: &mut Vec<u8>| {
                let bytes = std::mem::take(bytes);
                let held = bytes.capacity();
                let made =
                    Made { bytes, begins: begins.replace(false), ends: false, whole: None, kept: kept.take(), done: 0 };
                turns.put(unit, made, held, false, &stopped)
            };
            let mut bytes = Vec::new();
            let (low, high) = (from >> GAME_BITS, (to - 1) >> GAME_BITS);
            for bucket in low..=high {
                let (starts, ends) = (from <= bucket << GAME_BITS, to >= (bucket + 1) << GAME_BITS);
                // Only the pass's first bucket ends inside it ([`make_room`]).
                let count = match (starts, ends) {
                    (true, true) => Count::Whole,
                    (true, false) => Count::First(rest),
                    (false, _) => Count::After(pass.split.last),
                };
                let (n, last) = put_bucket(&mut heads, bucket, count, &mut bytes, &mut gathered, &mut hand)?;
                kept.set(kept.get() + n);
                // A bucket's count is its postings in every part.
                let left = match count {
                    Count::Whole => 0,
                    Count::First(all) => all.checked_sub(n).ok_or_else(miscounted)?,
                    Count::After(_) => pass.split.left.checked_sub(n).ok_or_else(miscounted)?,
                };
                if !ends {
                    *split.lock().unwrap_or_else(|e| e.into_inner()) = Some(Split { last, left });
                } else if left != 0 {
                    return Err(miscounted());
                }
                if bytes.len() >= OUT_BYTES / 2 && bucket < high {
                    hand(&mut bytes)?;
                }
            }
            let (begins, ends) = (begins.get(), to == block_hi);
            let whole = (begins && ends).then(|| !crc32_update(!0, &bytes));
            let done = if ends { counts.get(block as usize).copied().unwrap_or(0) } else { 0 };
            let held = bytes.capacity();
            turns.put(unit, Made { bytes, begins, ends, whole, kept: kept.get(), done }, held, true, &stopped)?;
        }
        Ok(())
    })?;
    Ok(split.into_inner().unwrap_or_else(|e| e.into_inner()))
}

/// What a pass puts before a bucket's postings.
#[derive(Clone, Copy, Debug)]
enum Count {
    /// Their count: the pass holds the bucket whole.
    Whole,
    /// The count of all the bucket's postings, of which the pass holds the
    /// first.
    First(u64),
    /// Nothing: the bucket goes on from a pass before, whose last game was
    /// this one.
    After(u64),
}

/// Puts bucket `bucket`'s postings, which lie first in the sorted `heads`,
/// to `out`, taking them off the heads: what `count` says, then each by game
/// from the one before, handing `out` over whenever it holds half of
/// [`OUT_BYTES`] before another. Returns the postings put and the last game.
/// A game's postings all come from one worker's buffer, so none repeats
/// another's, and a bucket's postings gathered from every head and sorted
/// are in the order of the heads merged: a bucket that fits `gathered`'s
/// capacity is sorted there, a larger one merged.
fn put_bucket(
    heads: &mut [&[u64]],
    bucket: u64,
    count: Count,
    out: &mut Vec<u8>,
    gathered: &mut Vec<u64>,
    hand: &mut dyn FnMut(&mut Vec<u8>) -> Result<(), SearchError>,
) -> Result<(u64, u64), SearchError> {
    let n: usize = heads.iter().map(|h| h.iter().take_while(|&&p| bucket_of(p) == bucket).count()).sum();
    let mut last = match count {
        Count::Whole => {
            varint(out, n as u64);
            0
        }
        Count::First(all) => {
            varint(out, all);
            0
        }
        Count::After(last) => last,
    };
    let mut put = |x: u64, out: &mut Vec<u8>| {
        if out.len() >= OUT_BYTES / 2 {
            hand(out)?;
        }
        let game = game_of(x);
        varint(out, (game - last) << 8 | x & 0xfe | !x & 1);
        last = game;
        Ok::<(), SearchError>(())
    };
    if n <= gathered.capacity() {
        gathered.clear();
        for h in heads.iter_mut() {
            let k = h.iter().take_while(|&&p| bucket_of(p) == bucket).count();
            gathered.extend_from_slice(&h[..k]);
            *h = &h[k..];
        }
        gathered.sort_unstable();
        for &x in gathered.iter() {
            put(x, out)?;
        }
    } else {
        for _ in 0..n {
            // The least head: few, so looked through in turn.
            let Some(h) =
                heads.iter_mut().filter(|h| h.first().is_some_and(|&p| bucket_of(p) == bucket)).min_by_key(|h| h[0])
            else {
                break;
            };
            let x = h[0];
            *h = &h[1..];
            put(x, out)?;
        }
    }
    Ok((n as u64, last))
}

/// The games of bucket `local` in a block's bytes that hold a structure of
/// print `print`, at most `max_game` each, only those that hold it beyond the
/// tree's plies when `beyond`; `None` when the block does not hold them as
/// written.
pub fn bucket_games(block: &[u8], local: usize, max_game: u32, print: u8, beyond: bool) -> Option<Vec<u32>> {
    let mut at = 0;
    for _ in 0..local {
        let n = read_varint(block, &mut at)?;
        for _ in 0..n {
            read_varint(block, &mut at)?;
        }
    }
    let n = read_varint(block, &mut at)?;
    // A game comes once per print at most, and each posting takes a byte at
    // least, so a damaged count never reserves more than the block's size.
    if n > u64::from(max_game) << PRINT_BITS || n > (block.len() - at) as u64 {
        return None;
    }
    let mut games = Vec::new();
    games.try_reserve_exact(n as usize).ok()?;
    let (mut game, mut last_print) = (0u64, 0u64);
    for _ in 0..n {
        let v = read_varint(block, &mut at)?;
        let (delta, p) = (v >> 8, v >> 1 & 0x7f);
        // Games ascend, from 1, and so do the prints of one game.
        if delta == 0 && (game == 0 || p <= last_print) {
            return None;
        }
        game += delta;
        if game > u64::from(max_game) {
            return None;
        }
        last_print = p;
        if p == u64::from(print) && (!beyond || v & 1 == 1) {
            games.push(game as u32);
        }
    }
    Some(games)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::format::structure;
    use chesscore::Move;

    /// A structure of bucket `bucket` among `1 << bits` with print `print`.
    fn structure_in(bits: u8, bucket: u64, print: u64) -> u64 {
        bucket << (64 - bits) | print << (64 - bits - PRINT_BITS)
    }

    /// Two workers' postings, out of order within neither, with a game twice
    /// in one bucket with one structure, once beyond the tree's plies, and a
    /// third time with another structure of the bucket: a block's buckets
    /// read back by game and print, the marked ones alone when asked.
    #[test]
    fn postings_come_back_per_bucket_in_game_order() {
        let bits = 14;
        let s = |bucket: u64, print: u64| structure_in(bits, bucket, print);
        let mut a =
            vec![posting(s(5, 1), bits, 9, false), posting(s(5, 1), bits, 3, true), posting(s(0, 0), bits, 1, true)];
        let mut b =
            vec![posting(s(5, 1), bits, 9, true), posting(s(5, 127), bits, 9, false), posting(s(6, 0), bits, 4, false)];
        for buf in [&mut a, &mut b] {
            buf.sort_unstable();
            buf.dedup_by_key(|p| place(*p));
        }
        // Game 9's two postings of one print lie in different buffers here
        // only for the test: a build replays each game on one worker.
        b.retain(|&p| !(bucket_of(p) == 5 && p >> 8 & 0xffff_ffff == 9 && p >> 1 & 0x7f == 1));
        // Each bucket sorted where it is gathered, or merged from the heads.
        let mut outs = Vec::new();
        for gathered in [Vec::with_capacity(8), Vec::new()] {
            let mut gathered = gathered;
            let mut heads: Vec<&[u64]> = vec![&a, &b];
            let mut out = Vec::new();
            let mut kept = 0;
            for bucket in 0..BLOCK_BUCKETS as u64 {
                let hand = &mut |_: &mut Vec<u8>| Ok(());
                kept += put_bucket(&mut heads, bucket, Count::Whole, &mut out, &mut gathered, hand).unwrap().0;
            }
            assert_eq!(kept, 5);
            assert!(heads.iter().all(|h| h.is_empty()));
            outs.push(out);
        }
        assert_eq!(outs[0], outs[1]);
        let out = outs.swap_remove(0);
        assert_eq!(bucket_games(&out, 5, 100, 1, false), Some(vec![3, 9]));
        assert_eq!(bucket_games(&out, 5, 100, 1, true), Some(vec![3]));
        assert_eq!(bucket_games(&out, 5, 100, 127, false), Some(vec![9]));
        assert_eq!(bucket_games(&out, 5, 100, 127, true), Some(vec![]));
        assert_eq!(bucket_games(&out, 0, 100, 0, true), Some(vec![1]));
        assert_eq!(bucket_games(&out, 6, 100, 0, false), Some(vec![4]));
        assert_eq!(bucket_games(&out, 6, 100, 0, true), Some(vec![]));
        assert_eq!(bucket_games(&out, BLOCK_BUCKETS - 1, 100, 0, false), Some(vec![]));
        // A game past the database's last record is damage.
        assert_eq!(bucket_games(&out, 5, 8, 1, false), None);
    }

    /// A damaged bucket is refused: a delta that wraps around to a game
    /// already listed, a game listed again with the same print or a lower
    /// one, a first game of 0, and a count or a delta written past 64 bits,
    /// which would read as 0 or 5 were the bits beyond cut off.
    #[test]
    fn a_damaged_bucket_is_refused() {
        let bucket = |values: &[u64]| {
            let mut b = Vec::new();
            for &v in values {
                varint(&mut b, v);
            }
            b
        };
        assert_eq!(bucket_games(&bucket(&[2, 5 << 8, u64::MAX - 2]), 0, 100, 0, false), None);
        assert_eq!(bucket_games(&bucket(&[2, 5 << 8 | 2, 2]), 0, 100, 1, false), None);
        assert_eq!(bucket_games(&bucket(&[2, 5 << 8 | 4, 2]), 0, 100, 1, false), None);
        assert_eq!(bucket_games(&bucket(&[2, 5 << 8 | 2, 4]), 0, 100, 1, false), Some(vec![5]));
        // A database of one game lists it twice in a bucket where it holds
        // two structures of different prints.
        assert_eq!(bucket_games(&bucket(&[2, 1 << 8 | 2, 4]), 0, 1, 2, false), Some(vec![1]));
        assert_eq!(bucket_games(&bucket(&[1, 2]), 0, 100, 1, false), None);
        let long = [0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02];
        assert_eq!(bucket_games(&long, 0, 100, 0, false), None);
        let mut delta = vec![1, 0x85];
        delta.extend(&long[1..]);
        assert_eq!(bucket_games(&delta, 0, 100, 0, false), None);
    }

    /// A full buffer drops its repeats and goes on; one of many buckets ends
    /// the pass for every worker at the bucket that keeps about half, inside
    /// a block; a first bucket that alone fills it ends the pass at the game
    /// that keeps about half, and so does the next pass, which starts there; a
    /// game that alone fills it is too large.
    #[test]
    fn a_full_buffer_ends_the_pass_earlier() {
        let bits = 12;
        let s = |bucket: u64| structure_in(bits, bucket, 3);
        let at = |bucket: u64, game: u64| bucket << GAME_BITS | game;
        let hi = AtomicU64::new(at(1 << bits, 0));
        let mut buf: Vec<u64> = (0..512).flat_map(|g| [posting(s(300), bits, g + 1, true); 2]).collect();
        make_room(&mut buf, 1_024, at(256, 0), &hi).unwrap();
        assert_eq!(
            (buf.len(), hi.load(Ordering::Relaxed)),
            (512, at(1 << bits, 0)),
            "repeats dropped, the pass as it was"
        );
        let mut buf: Vec<u64> = (0..1_024).map(|i| posting(s(256 + i / 64), bits, i as u32 + 1, true)).collect();
        make_room(&mut buf, 1_024, at(256, 0), &hi).unwrap();
        assert_eq!(hi.load(Ordering::Relaxed), at(264, 0), "inside the block of buckets 256 to 511");
        assert_eq!(buf.len(), 512);
        let mut crowded: Vec<u64> = (0..1_024).map(|i| posting(s(256), bits, i + 1, true)).collect();
        make_room(&mut crowded, 1_024, at(256, 0), &hi).unwrap();
        assert_eq!((crowded.len(), hi.load(Ordering::Relaxed)), (512, at(256, 513)), "inside bucket 256");
        let hi = AtomicU64::new(at(1 << bits, 0));
        let mut next: Vec<u64> = (512..1_536).map(|i| posting(s(256), bits, i + 1, true)).collect();
        make_room(&mut next, 1_024, at(256, 513), &hi).unwrap();
        assert_eq!((next.len(), hi.load(Ordering::Relaxed)), (512, at(256, 1_025)), "from game 513 to 1,024");
        // One game of 64 prints, in a buffer smaller than a worker's least.
        let mut one: Vec<u64> = (0..64).map(|print| posting(structure_in(bits, 256, print), bits, 7, true)).collect();
        assert!(matches!(make_room(&mut one, 64, at(256, 7), &hi), Err(SearchError::TooLarge)));
    }

    /// A line's structures are found in a pass as their points say: from
    /// `lo` on and before `hi`, either of which may lie inside a bucket, each
    /// as [`posting`] makes it, and those of the pass's first bucket from
    /// `lo` on counted once per print.
    #[test]
    fn a_line_finds_the_postings_that_lie_in_the_pass() {
        let bits = 10;
        let at = |bucket: u64, game: u64| bucket << GAME_BITS | game;
        let passes = [
            (at(0, 0), at(1 << bits, 0)),
            (at(300, 0), at(301, 0)),
            (at(300, 50), at(300, 70)),
            (at(300, 50), at(302, 60)),
            (at(299, 1), at(1 << bits, 0)),
        ];
        let mut found = [0; MAX_STRUCTURES];
        let (mut kept, mut counted) = (0, 0);
        for (lo, hi) in passes {
            for bucket in [0, 1, 298, 299, 300, 301, 302, 303, (1 << bits) - 1] {
                for game in [1, 49, 50, 51, 59, 60, 61, 69, 70, 71, 1_000_000] {
                    for print in [0, 1, 126, 127] {
                        let structure = structure_in(bits, bucket, print) | 0x1234_5678 >> bits;
                        let mut replayed = Replayed::default();
                        find(structure, &Within::of(lo, hi, bits, game as u32), &mut replayed, &mut found);
                        let point = at(bucket, game);
                        let first = point >> GAME_BITS == lo >> GAME_BITS && point >= lo;
                        let what = format!("bucket {bucket} game {game} print {print} in {lo:#x}..{hi:#x}");
                        assert_eq!(replayed.found, usize::from((lo..hi).contains(&point)), "{what}");
                        assert_eq!(found[0], posting(structure, bits, game as u32, true), "{what}");
                        assert_eq!(replayed.prints, u128::from(first) << print, "{what}");
                        kept += replayed.found;
                        counted += usize::from(first);
                    }
                }
            }
        }
        assert!(kept > 100 && counted > 20, "{kept} {counted}");
        // A line's structures past the most the walk kept are not found.
        let mut replayed = Replayed { structures: MAX_STRUCTURES, ..Replayed::default() };
        find(structure_in(bits, 5, 0), &Within::of(0, at(1 << bits, 0), bits, 1), &mut replayed, &mut found);
        assert_eq!((replayed.found, replayed.structures), (0, MAX_STRUCTURES));
    }

    /// A crowded bucket is handed over in pieces as it is put, none of which
    /// grows past [`OUT_BYTES`], and which together read back as the bucket's
    /// postings; a bucket split among three passes, each going on from the
    /// game put last, puts the same bytes.
    #[test]
    fn a_crowded_bucket_is_put_in_pieces() {
        let (bits, bucket, games) = (12, 9u64, 600_000u32);
        let at = |game: u64| (bucket << GAME_BITS) + game;
        // Two workers' buffers, each game in one; some games hold a
        // structure of another print too.
        let mut buffers = [Vec::new(), Vec::new()];
        for g in 1..=games {
            let buf = &mut buffers[(g / 1_000 % 2) as usize];
            buf.push(posting(structure_in(bits, bucket, 5), bits, g, g % 3 == 0));
            if g % 7 == 0 {
                buf.push(posting(structure_in(bits, bucket, 6), bits, g, true));
            }
        }
        let all: u64 = buffers.iter().map(|b| b.len() as u64).sum();
        let mut gathered = Vec::with_capacity(GATHERED);
        // Puts the bucket's postings from point `from` to `to` after
        // `bytes`, as `count` says: the postings put, the last game, and the
        // pieces handed over.
        let mut put = |from: u64, to: u64, count: Count, bytes: &mut Vec<u8>| {
            let mut heads: Vec<&[u64]> = buffers
                .iter()
                .map(|b| {
                    let lo = b.partition_point(|&p| point(p) < from);
                    &b[lo..lo + b[lo..].partition_point(|&p| point(p) < to)]
                })
                .collect();
            let (mut out, mut pieces) = (Vec::new(), 0);
            let mut hand = |piece: &mut Vec<u8>| {
                let piece = std::mem::take(piece);
                assert!(piece.len() >= OUT_BYTES / 2 && piece.capacity() <= OUT_BYTES, "{}", piece.capacity());
                bytes.extend(piece);
                pieces += 1;
                Ok(())
            };
            let (n, last) = put_bucket(&mut heads, bucket, count, &mut out, &mut gathered, &mut hand).unwrap();
            assert!(out.capacity() <= OUT_BYTES, "{} bytes held at once", out.capacity());
            bytes.extend(out);
            (n, last, pieces)
        };
        let mut whole = Vec::new();
        let (n, last, pieces) = put(at(0), at(1 << GAME_BITS), Count::Whole, &mut whole);
        assert!((n, last) == (all, u64::from(games)) && pieces >= 2, "{n} {last} {pieces}");
        let mut split = Vec::new();
        let (a, last) = (put(at(0), at(200_001), Count::First(all), &mut split).0, 200_000);
        let (b, last) = (put(at(200_001), at(400_001), Count::After(last), &mut split).0, 400_000);
        let c = put(at(400_001), at(1 << GAME_BITS), Count::After(last), &mut split).0;
        assert_eq!(a + b + c, all);
        assert!(split == whole, "the split bucket is put as the whole one");
        let every = |step: u32| Some((1..=games).filter(|g| g % step == 0).collect::<Vec<_>>());
        assert_eq!(bucket_games(&whole, 0, games, 5, false), every(1));
        assert_eq!(bucket_games(&whole, 0, games, 5, true), every(3));
        assert_eq!(bucket_games(&whole, 0, games, 6, true), every(7));
    }

    /// Following a line's words alone gives the structure a board gives, at
    /// every ply, through captures of every kind, en passant, castling and
    /// promotions to every piece, with and without a capture, from a set-up
    /// start too; and it says so whenever the structure changes.
    #[test]
    fn a_tracked_structure_is_the_boards() {
        let lines: [(Option<&str>, &str); 4] = [
            (
                None,
                "e2e4 d7d5 e4d5 d8d5 b1c3 d5a5 d2d4 c7c6 g1f3 c8f5 f1c4 e7e6 e1h1 g8f6 c1d2 f8b4 c3e4 a5b6 \
                 e4f6 g7f6 c4b3 b8d7 d2b4 b6b4 c2c3",
            ),
            (None, "e2e4 a7a6 e4e5 d7d5 e5d6 c7d6 d1g4 c8g4 f1a6 b8a6 g1f3 d8b6 e1h1 e8c8"),
            (Some("r6r/1P4P1/8/8/2k5/8/1p4p1/R3K2R w KQ - 0 1"), "b7a8n g2h1q e1d2 b2a1r g7h8b c4b3 a8b6"),
            (Some("4k3/1P6/8/8/8/8/6p1/R3K3 b Q - 0 1"), "g2g1q e1d2 e8d7 b7b8n d7c7 a1a7"),
        ];
        for (fen, ucis) in lines {
            let mut board = fen.map_or_else(Board::startpos, |f| Board::from_fen(f).unwrap());
            let mut tracked = Tracker::of(&board);
            for uci in ucis.split_whitespace() {
                let mut mv: Move = uci.parse().unwrap();
                // Castling is the king onto its rook.
                if board.piece_at(mv.from).is_some_and(|p| p.0 == Piece::King)
                    && mv.from.file().abs_diff(mv.to.file()) == 2
                {
                    mv = Move::new(
                        mv.from,
                        chesscore::Square::new(if mv.to.file() > 4 { 7 } else { 0 }, mv.from.rank()),
                        None,
                    );
                }
                let word = cbformat::replay::word_of(&board, mv).unwrap();
                let before = structure(&board);
                board.play_checked(mv).unwrap();
                let changed = tracked.play(word).unwrap();
                assert_eq!(tracked.structure(), structure(&board), "{uci} in {ucis}");
                if structure(&board) != before {
                    assert!(changed, "{uci} changed the structure");
                }
            }
        }
        assert_eq!(Tracker::standard(), Tracker::of(&Board::startpos()));
        assert_eq!(Tracker::of(&Board::startpos()).play(0), None, "word 0 names no move");
        assert_eq!(Tracker::of(&Board::startpos()).play(movetable::NULL_MOVE), None);
        assert_eq!(Tracker::of(&Board::startpos()).play(FIRST_CASTLE_960), None);
    }

    /// Each word's effect is looked up as it is made from the word, and a
    /// word that names no move of standard chess, a Chess960 castling among
    /// them, looks up none.
    #[test]
    fn each_word_looks_up_its_effect() {
        let tracker = Tracker::of(&Board::startpos());
        let mut named = 0;
        for word in 0..=u16::MAX {
            let made = if word < FIRST_CASTLE_960 { effect_of(word) } else { None };
            named += usize::from(made.is_some());
            assert_eq!(made.unwrap_or_default(), *tracker.effect(word), "{word:#x}");
        }
        assert!(named > 40_000, "{named}");
    }

    /// Words played a run at a time note the parts after each word that may
    /// change the structure, as the words played one at a time say, and
    /// lead to the same structure played all at once, over random games long
    /// enough to promote; a run with a word that names no move fails.
    #[test]
    fn a_run_of_words_notes_each_change() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut changed = 0;
        for _ in 0..60 {
            let mut board = Board::startpos();
            let mut words = Vec::new();
            for _ in 0..300 {
                let moves = board.legal_moves();
                if moves.is_empty() {
                    break;
                }
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let mv = moves[(x % moves.len() as u64) as usize];
                words.push(cbformat::replay::word_of(&board, mv).unwrap());
                board.play_unchecked(mv);
            }
            let mut one = Tracker::of(&Board::startpos());
            let mut expected = Vec::new();
            for &w in &words {
                if one.play(w).unwrap() {
                    expected.push((one.pawns, one.pieces));
                }
            }
            let bytes: Vec<[u8; 2]> = words.iter().map(|w| w.to_le_bytes()).collect();
            let (mut run, mut noted) = (Tracker::of(&Board::startpos()), Vec::new());
            let mut changes = [Change::default(); CHANGES];
            for words in bytes.chunks(CHANGES) {
                let n = run.play_noting(words, &mut changes).unwrap();
                noted.extend(changes[..n].iter().map(|c| (c.pawns, c.pieces)));
            }
            assert_eq!(noted, expected);
            assert_eq!(run, one);
            let mut all = Tracker::of(&Board::startpos());
            assert_eq!(all.play_all(&bytes), Some(()));
            assert_eq!(all, one);
            changed += noted.len();
        }
        assert!(changed > 1_000, "{changed}");
        let mut run = Tracker::of(&Board::startpos());
        let mut changes = [Change::default(); CHANGES];
        let e2e4 = cbformat::replay::word_of(&Board::startpos(), "e2e4".parse().unwrap()).unwrap();
        assert_eq!(run.play_noting(&[e2e4.to_le_bytes(), [0, 0]], &mut changes), None);
        assert_eq!(run.play_all(&[e2e4.to_le_bytes(), FIRST_CASTLE_960.to_le_bytes()]), None);
    }
}
