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
//! worker. The workers then take the pass's blocks in order, each merging one
//! block's postings from every buffer, and write them once the blocks before
//! have been written: per block of [`BLOCK_BUCKETS`] buckets, each bucket's
//! posting count and its postings by game, then print, each a varint of the
//! game's difference from the one before, shifted left by eight, the print
//! and the mark; the block covered by a CRC-32 in the section's table. A
//! pass may end inside a block, which the next one goes on with.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use chesscore::{Board, Color, Piece};

use cbformat::movetable::{self, Captured, FIRST_CASTLE_960, MoveWord};

use crate::indexdir::crc32_update;
use crate::search::SearchError;
use crate::search::memory::{Cancel, Refused};
use crate::search::workers::{self, threads};

use super::build::{Chunks, Out, Turns, corrupt, from_bad};
use super::format::{
    DEEP_BLOCK_BITS, DEEP_BLOCK_ENTRY, MAX_PLY, PRINT_BITS, PRUNE_PLY, STRUCTURE_PIECES, deep_bucket, deep_print,
    piece_shift, read_varint, structure_of, varint,
};
use super::runs::{Limits, Progress, Room};
use super::source::MAX_STRUCTURES;
use super::stream::{self, Stream};

/// Buckets per block.
pub const BLOCK_BUCKETS: usize = 1 << DEEP_BLOCK_BITS;
/// The least room a worker's postings take.
const MIN_WORKER_POSTINGS: usize = 64;
/// The bytes of blocks a worker hands over at once at most, and its share of
/// those kept until their turn to be written comes.
const OUT_BYTES: usize = 1 << 20;
/// What a worker holds besides its postings: the bytes it makes, its share
/// of those kept, and a block's buffers to merge.
pub const WORKER_BYTES: usize = 2 * OUT_BYTES + (1 << 10);

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

/// A posting's bucket, game and print, apart from its mark.
fn place(p: u64) -> u64 {
    p >> 1
}

/// What a move word does to a structure, from the move table, in 64 bits,
/// so that a word is played without a branch: the square a pawn leaves
/// (bits 0-5), the square it reaches (6-11), whether the word names a move
/// (12), a black one (13), whether a pawn leaves its square (14) and reaches
/// the other one (15), the square of a pawn it takes (16-21) and whether it
/// takes one (22), whether the structure changes (23), and the change to the
/// pieces' counts (32-63, signed): a piece taken, a pawn promoted.
#[derive(Clone, Copy, Default)]
struct Effect(u64);

const MOVE: u64 = 1 << 12;

/// The effect of each word below the Chess960 castlings.
fn effects() -> &'static [Effect] {
    static EFFECTS: OnceLock<Vec<Effect>> = OnceLock::new();
    EFFECTS.get_or_init(|| {
        let kind = |p: movetable::Piece| match p {
            movetable::Piece::Knight => 0,
            movetable::Piece::Bishop => 1,
            movetable::Piece::Rook => 2,
            _ => 3,
        };
        (0..FIRST_CASTLE_960)
            .map(|word| match movetable::decode(word) {
                Some(MoveWord::Normal { color, piece, from, to, captured, promotion }) => {
                    let (us, them) = match color {
                        movetable::Color::White => (Color::White, Color::Black),
                        movetable::Color::Black => (Color::Black, Color::White),
                    };
                    let pawn = piece == movetable::Piece::Pawn;
                    let (from, to) = (u64::from(from & 63), u64::from(to & 63));
                    // The pawn taken en passant stands beside the one that
                    // takes it: on the rank it leaves, the file it reaches.
                    let taken_pawn = match captured {
                        Captured::Pawn => Some(to),
                        Captured::EnPassant => Some(from & 56 | to & 7),
                        _ => None,
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
                    Effect(
                        from | to << 6
                            | MOVE
                            | u64::from(us == Color::Black) << 13
                            | u64::from(pawn) << 14
                            | u64::from(pawn && promotion.is_none()) << 15
                            | taken_pawn.unwrap_or(0) << 16
                            | u64::from(taken_pawn.is_some()) << 22
                            | u64::from(changes) << 23
                            | (delta as i32 as u32 as u64) << 32,
                    )
                }
                Some(MoveWord::Castle { .. }) => Effect(MOVE),
                _ => Effect::default(),
            })
            .collect()
    })
}

/// A line's structure followed through its move words alone: each side's
/// pawns and its pieces counted by kind, as [`super::format::structure`]
/// hashes them. A word names the piece it moves, what it takes and what a
/// pawn becomes, and the stream's words were checked when it was written, so
/// no board is needed.
#[derive(Clone, Copy, Debug)]
pub struct Tracker {
    pawns: [u64; 2],
    pieces: u64,
    effects: &'static [Effect],
}

impl PartialEq for Tracker {
    fn eq(&self, other: &Tracker) -> bool {
        (self.pawns, self.pieces) == (other.pawns, other.pieces)
    }
}

impl std::fmt::Debug for Effect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Effect({:#x})", self.0)
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
        Tracker { pawns: [pawns(Color::White), pawns(Color::Black)], pieces, effects: effects() }
    }

    /// Plays `word`: whether the structure may have changed, which only a
    /// pawn's move or a capture does; `None` for a word that names no move
    /// of standard chess.
    #[inline]
    pub fn play(&mut self, word: u16) -> Option<bool> {
        let e = self.effects.get(usize::from(word))?.0;
        if e & MOVE == 0 {
            return None;
        }
        let us = (e >> 13 & 1) as usize;
        self.pawns[us ^ 1] &= !((e >> 22 & 1) << (e >> 16 & 63));
        self.pawns[us] &= !((e >> 14 & 1) << (e & 63));
        self.pawns[us] |= (e >> 15 & 1) << (e >> 6 & 63);
        self.pieces = self.pieces.wrapping_add((e >> 32) as u32 as i32 as i64 as u64);
        Some(e >> 23 & 1 != 0)
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
    let room = room.min(limits.pass_bytes.unwrap_or(usize::MAX));
    let least = MIN_WORKER_POSTINGS * 8;
    if room < least {
        return Err(SearchError::TooLarge);
    }
    let (_memory, want, room) =
        Room { fixed: table_bytes, each: WORKER_BYTES, workers: want, least, room }.reserve(progress)?;
    let capacity = room / 8;
    let want = want.min(capacity / MIN_WORKER_POSTINGS);
    let mut table = Vec::new();
    table.try_reserve_exact(table_bytes).map_err(|_| Refused::Busy)?;
    let mut sink = Sink { out, table, start: 0, crc: !0, postings: 0 };
    let buckets = (counts.len() as u64) << DEEP_BLOCK_BITS;
    let mut lo = 0;
    while lo < buckets {
        // As many blocks as three quarters of the room hold, one at least,
        // the one the last pass ended in counted whole.
        let mut end = (lo >> DEEP_BLOCK_BITS) as usize;
        let first = end;
        let mut planned = 0;
        while end < counts.len() && (end == first || planned + counts[end] <= (capacity / 4 * 3) as u64) {
            planned += counts[end];
            end += 1;
        }
        let hi = AtomicU64::new((end as u64) << DEEP_BLOCK_BITS);
        progress.deep_passes.fetch_add(1, Ordering::Relaxed);
        let pass = Pass { stream, bits, lo, hi: &hi, capacity, progress };
        let buffers = pass.collect(want)?;
        let hi = hi.load(Ordering::Relaxed);
        write_blocks(&buffers, &pass, hi, counts, &mut sink, want)?;
        lo = hi;
    }
    let table_offset = sink.out.offset;
    sink.out.put(&sink.table)?;
    Ok(Section { postings: sink.postings, table_offset, table_crc: !crc32_update(!0, &sink.table) })
}

/// One pass: buckets `lo..hi`, `hi` lowered by a worker whose postings do not
/// fit.
struct Pass<'a> {
    stream: &'a Stream,
    bits: u8,
    lo: u64,
    hi: &'a AtomicU64,
    /// The postings all workers' buffers hold together.
    capacity: usize,
    progress: &'a Progress,
}

impl Pass<'_> {
    /// Each worker's postings of the pass's buckets, sorted and freed of
    /// repeats, of up to `want` workers.
    fn collect(&self, want: usize) -> Result<Vec<Vec<u64>>, SearchError> {
        let chunks = Chunks::new(self.stream.header.first_record, self.stream.header.last_record, want);
        workers::run(want, 0, &Cancel::never(), |w| {
            let cap = self.capacity / w.count;
            let mut buf: Vec<u64> = Vec::new();
            buf.try_reserve_exact(cap).map_err(|_| Refused::Busy)?;
            while let Some((lo, hi)) = chunks.take() {
                if w.stopped() || self.progress.stop.load(Ordering::Relaxed) {
                    return Err(SearchError::Superseded);
                }
                for game in lo..=hi {
                    self.replay(game, &mut buf, cap)?;
                }
            }
            buf.sort_unstable();
            buf.dedup_by_key(|p| place(*p));
            Ok(buf)
        })
    }

    /// Adds the postings of game `game`'s structures past the tree's plies
    /// that lie in the pass to `buf`: each once, in the order its line holds
    /// them, as the walk that wrote the stream met them, and marked when the
    /// line holds it beyond [`MAX_PLY`].
    fn replay(&self, game: u32, buf: &mut Vec<u64>, cap: usize) -> Result<(), SearchError> {
        let path = &self.stream.path;
        let record = self.stream.written(game).map_err(|e| from_bad(path, e))?;
        let entry = record.entry;
        if !entry.indexed() || entry.plies <= u16::from(PRUNE_PLY) {
            return Ok(());
        }
        let start = record.start().map_err(|e| from_bad(path, e))?;
        let mut line = Tracker::of(start.as_ref().unwrap_or_else(|| stream::standard()));
        let word = || corrupt(path, "stream word");
        let mut words = record.words();
        // The tree's plies hold no structure of the section.
        for w in words.by_ref().take(usize::from(PRUNE_PLY) + 1) {
            line.play(w).ok_or_else(word)?;
        }
        // The structure held from ply 21, and the last ply it was held at.
        let mut held = (line.structure(), u32::from(PRUNE_PLY) + 1);
        let mut structures = 0;
        for (ply, w) in (u32::from(PRUNE_PLY) + 2..).zip(words) {
            if line.play(w).ok_or_else(word)? {
                let s = line.structure();
                if s != held.0 {
                    self.add(held.0, held.1, game, &mut structures, buf, cap)?;
                    held.0 = s;
                }
            }
            held.1 = ply;
        }
        self.add(held.0, held.1, game, &mut structures, buf, cap)
    }

    /// Adds game `game`'s posting of `structure`, held last at ply `last`, when
    /// its bucket lies in the pass: the line's first [`MAX_STRUCTURES`]
    /// structures, as the walk kept them.
    fn add(
        &self,
        structure: u64,
        last: u32,
        game: u32,
        structures: &mut usize,
        buf: &mut Vec<u64>,
        cap: usize,
    ) -> Result<(), SearchError> {
        if *structures >= MAX_STRUCTURES {
            return Ok(());
        }
        *structures += 1;
        let bucket = u64::from(deep_bucket(structure, self.bits));
        if bucket < self.lo || bucket >= self.hi.load(Ordering::Relaxed) {
            return Ok(());
        }
        if buf.len() >= cap {
            make_room(buf, cap, self.lo, self.hi)?;
        }
        if bucket < self.hi.load(Ordering::Relaxed) {
            buf.push(posting(structure, self.bits, game, last > u32::from(MAX_PLY)));
        }
        Ok(())
    }
}

/// Makes room in `buf`, a worker's full buffer of `cap` postings in a pass of
/// the buckets from `lo` to `hi`: its postings sorted and freed of repeats,
/// and when they still take three quarters of it, the pass ended for every
/// worker at the bucket that keeps about half, which may lie inside a block.
/// A first bucket that alone leaves no room is too large for the share.
fn make_room(buf: &mut Vec<u64>, cap: usize, lo: u64, hi: &AtomicU64) -> Result<(), SearchError> {
    buf.sort_unstable();
    let end = hi.load(Ordering::Relaxed);
    buf.truncate(buf.partition_point(|&p| bucket_of(p) < end));
    buf.dedup_by_key(|p| place(*p));
    if buf.len() > cap / 4 * 3 {
        let cut = bucket_of(buf[cap / 2]).max(lo + 1);
        hi.fetch_min(cut, Ordering::Relaxed);
        buf.truncate(buf.partition_point(|&p| bucket_of(p) < cut));
        if buf.len() > cap - cap / 8 {
            return Err(SearchError::TooLarge);
        }
    }
    Ok(())
}

/// What the section's blocks are written to, one after another; a block
/// begun in one pass may end in the next.
struct Sink<'a> {
    out: &'a mut Out,
    table: Vec<u8>,
    /// Where the block being written starts, and its CRC so far.
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

impl Sink<'_> {
    /// Writes the bytes `made`, and ends their block in the table when they
    /// end it.
    fn put(&mut self, made: Made, progress: &Progress) -> Result<(), SearchError> {
        if made.begins {
            self.start = self.out.offset;
            self.crc = !0;
        }
        if made.whole.is_none() {
            self.crc = crc32_update(self.crc, &made.bytes);
        }
        self.out.put(&made.bytes)?;
        self.postings += made.kept;
        if made.ends {
            let len = u32::try_from(self.out.offset - self.start).map_err(|_| SearchError::TooLarge)?;
            self.table.extend(self.start.to_le_bytes());
            self.table.extend(len.to_le_bytes());
            self.table.extend(made.whole.unwrap_or(!self.crc).to_le_bytes());
            progress.done.fetch_add(made.done, Ordering::Relaxed);
        }
        Ok(())
    }
}

/// Writes buckets `pass.lo..hi` of the sorted `buffers` on up to `want`
/// workers, block by block, in order.
fn write_blocks(
    buffers: &[Vec<u64>],
    pass: &Pass<'_>,
    hi: u64,
    counts: &[u64],
    sink: &mut Sink<'_>,
    want: usize,
) -> Result<(), SearchError> {
    let first = pass.lo >> DEEP_BLOCK_BITS;
    let units = ((hi - 1) >> DEEP_BLOCK_BITS) - first + 1;
    let progress = pass.progress;
    let turns = Turns::new(units as usize, want * OUT_BYTES, sink);
    let write = |sink: &mut &mut Sink<'_>, made: Made| sink.put(made, progress);
    workers::run(want, 0, &Cancel::never(), |w| {
        let stopped = || w.stopped() || progress.stop.load(Ordering::Relaxed);
        let mut heads: Vec<&[u64]> = Vec::new();
        heads.try_reserve_exact(buffers.len()).map_err(|_| Refused::Busy)?;
        while let Some(unit) = turns.take() {
            if stopped() {
                return Err(SearchError::Superseded);
            }
            let block = first + unit as u64;
            let (block_lo, block_hi) = (block << DEEP_BLOCK_BITS, (block + 1) << DEEP_BLOCK_BITS);
            let (from, to) = (pass.lo.max(block_lo), hi.min(block_hi));
            heads.clear();
            for b in buffers {
                let at = b.partition_point(|&p| bucket_of(p) < from);
                let end = at + b[at..].partition_point(|&p| bucket_of(p) < to);
                if end > at {
                    heads.push(&b[at..end]);
                }
            }
            let (mut out, mut begins, mut kept) = (Vec::new(), from == block_lo, 0);
            for bucket in from..to {
                kept += put_bucket(&mut heads, bucket, &mut out);
                if out.len() >= OUT_BYTES / 2 && bucket + 1 < to {
                    let bytes = std::mem::take(&mut out);
                    let (len, made) = (bytes.len(), Made { bytes, begins, ends: false, whole: None, kept, done: 0 });
                    turns.put(unit, made, len, false, &stopped, &write)?;
                    (begins, kept) = (false, 0);
                }
            }
            let ends = to == block_hi;
            let whole = (begins && ends).then(|| !crc32_update(!0, &out));
            let done = if ends { counts.get(block as usize).copied().unwrap_or(0) } else { 0 };
            let len = out.len();
            turns.put(unit, Made { bytes: out, begins, ends, whole, kept, done }, len, true, &stopped, &write)?;
        }
        Ok(())
    })?;
    Ok(())
}

/// Puts bucket `bucket`'s postings, which lie first in the sorted `heads`,
/// to `out`: their count, then each by game, taking them off the heads.
/// Returns the count. A game's postings all come from one worker's buffer, so
/// none repeats another's.
fn put_bucket(heads: &mut [&[u64]], bucket: u64, out: &mut Vec<u8>) -> u64 {
    let n: usize = heads.iter().map(|h| h.iter().take_while(|&&p| bucket_of(p) == bucket).count()).sum();
    varint(out, n as u64);
    let mut last = 0u64;
    for _ in 0..n {
        // The least head: few, so looked through in turn.
        let Some(h) =
            heads.iter_mut().filter(|h| h.first().is_some_and(|&p| bucket_of(p) == bucket)).min_by_key(|h| h[0])
        else {
            break;
        };
        let x = h[0];
        *h = &h[1..];
        let game = x >> 8 & 0xffff_ffff;
        varint(out, (game - last) << 8 | x & 0xfe | !x & 1);
        last = game;
    }
    n as u64
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
        let mut heads: Vec<&[u64]> = vec![&a, &b];
        let mut out = Vec::new();
        let kept: u64 = (0..BLOCK_BUCKETS as u64).map(|bucket| put_bucket(&mut heads, bucket, &mut out)).sum();
        assert_eq!(kept, 5);
        assert!(heads.iter().all(|h| h.is_empty()));
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
    /// a block; a first bucket that alone fills it is too large.
    #[test]
    fn a_full_buffer_ends_the_pass_earlier() {
        let bits = 12;
        let s = |bucket: u64| structure_in(bits, bucket, 3);
        let hi = AtomicU64::new(1 << bits);
        let mut buf: Vec<u64> = (0..512).flat_map(|g| [posting(s(300), bits, g + 1, true); 2]).collect();
        make_room(&mut buf, 1_024, 256, &hi).unwrap();
        assert_eq!((buf.len(), hi.load(Ordering::Relaxed)), (512, 1 << bits), "repeats dropped, the pass as it was");
        let mut buf: Vec<u64> = (0..1_024).map(|i| posting(s(256 + i / 64), bits, i as u32 + 1, true)).collect();
        make_room(&mut buf, 1_024, 256, &hi).unwrap();
        assert_eq!(hi.load(Ordering::Relaxed), 264, "inside the block of buckets 256 to 511");
        assert_eq!(buf.len(), 512);
        let mut full: Vec<u64> = (0..1_024).map(|i| posting(s(256), bits, i + 1, true)).collect();
        assert!(matches!(make_room(&mut full, 1_024, 256, &hi), Err(SearchError::TooLarge)));
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
        assert_eq!(Tracker::of(&Board::startpos()).play(0), None, "word 0 names no move");
        assert_eq!(Tracker::of(&Board::startpos()).play(movetable::NULL_MOVE), None);
        assert_eq!(Tracker::of(&Board::startpos()).play(FIRST_CASTLE_960), None);
    }
}
