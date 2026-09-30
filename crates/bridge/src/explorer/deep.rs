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
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::indexdir::crc32_update;
use crate::search::SearchError;
use crate::search::memory::Refused;
use crate::search::workers::threads;

use super::build::{Chunks, Out, Passes, Turns, from_bad, plan_pass};
use super::follow::{CHANGES, Changes, Tracker};
use super::format::{
    BLOCK_BUCKETS, DEEP_BLOCK_BITS, DEEP_BLOCK_ENTRY, MAX_PLY, PRINT_BITS, deep_bucket, deep_print, structure_of,
    structures_in_vectors, structures_of, varint,
};
use super::runs::{Limits, PassTime, Progress, on_workers};
use super::source::MAX_STRUCTURES;
use super::stream::{self, Stream};

/// The least room a worker's postings take: twice a game's most, so that a
/// pass that ends at a game keeps some.
const MIN_WORKER_POSTINGS: usize = 2 * MAX_STRUCTURES;
/// The bytes of blocks a worker makes and hands over at once at most, and
/// its share of those kept until their turn to be written comes: a piece is
/// handed over once it holds half of it, even inside a bucket, and so never
/// grows past it.
const OUT_BYTES: usize = 1 << 20;
/// The postings of a run of a block's buckets gathered to be sorted, at
/// most ([`Gathered`]).
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
    let postings: u64 = counts.iter().sum();
    progress.start("structures", postings);
    let table_bytes = counts.len() * DEEP_BLOCK_ENTRY;
    // A pass ends inside a bucket that alone fills the room ([`make_room`]),
    // so a worker needs its least room, whatever the largest block.
    let passes = Passes {
        table: table_bytes,
        worker: WORKER_BYTES,
        item: 8,
        items: postings,
        least: MIN_WORKER_POSTINGS,
        largest: 0,
    };
    let room = passes.room(share, threads(), stream.header.records(), limits.pass_bytes)?;
    let (_memory, want, room) = room.reserve(progress)?;
    let (capacity, want) = passes.capacity(room, want);
    let mut table = Vec::new();
    table.try_reserve_exact(table_bytes).map_err(|_| Refused::Busy)?;
    let mut sink = Sink { at: out.offset, table, start: 0, crc: !0, postings: 0 };
    let (mut lo, mut split) = (0, Split::default());
    while lo < block_point(counts.len() as u64) {
        // From the block the last pass ended in, counted whole.
        let first = (lo >> (u32::from(DEEP_BLOCK_BITS) + GAME_BITS)) as usize;
        let (end, planned) = plan_pass(first, counts.len(), capacity, |block| counts[block]);
        let hi = AtomicU64::new(block_point(end as u64));
        progress.deep_passes.fetch_add(1, Ordering::Relaxed);
        let rest = AtomicU64::new(0);
        let pass = Pass { stream, bits, lo, hi: &hi, split, rest, capacity, planned, progress, limits };
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
    limits: &'a Limits,
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
/// line notes as it goes, the changes of a run of words, their structures,
/// and the postings that lie in the pass.
struct Kept {
    buf: Vec<u64>,
    cap: usize,
    rest: u64,
    changes: Changes,
    structures: [u64; CHANGES],
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
        let spare = chunks.spare(self.planned);
        on_workers(want, self.progress, self.limits, |w| {
            let cap = self.capacity / w.count;
            let mut kept = Kept {
                buf: Vec::new(),
                cap,
                rest: 0,
                changes: Changes::default(),
                structures: [0; CHANGES],
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
        if !entry.indexed() || entry.plies <= u16::from(MAX_PLY) {
            return Ok(());
        }
        let start = record.start().map_err(|e| from_bad(path, e))?;
        let mut line = start.as_ref().map_or_else(Tracker::standard, Tracker::of);
        let word = || SearchError::Bug("stream word");
        // The tree's plies hold no structure of the section: they are the
        // prefix's words, and the words past it follow ply 21, beyond the
        // tree's plies.
        const { assert!(stream::PREFIX_WORDS == MAX_PLY as usize + 1) };
        let (prefix, past) = record.word_parts();
        line.play_all(prefix).ok_or_else(word)?;
        let within = Within::of(self.lo, self.hi.load(Ordering::Relaxed), self.bits, game);
        let mut held = line.structure();
        let mut replayed = Replayed::default();
        if structures_in_vectors() {
            for words in past.chunks(CHANGES) {
                let noted = line.play_noting_apart(words, &mut kept.changes).ok_or_else(word)?;
                let (c, structures) = (&kept.changes, &mut kept.structures);
                structures_of(&c.white, &c.black, &c.pieces, noted, structures);
                held = find_changed(&structures[..noted], held, &within, &mut replayed, &mut kept.found);
            }
        } else {
            for words in past.chunks(CHANGES) {
                let noted = line.play_noting(words, &mut kept.changes).ok_or_else(word)?;
                let c = &kept.changes;
                for ((&w, &b), &p) in c.white.iter().zip(&c.black).zip(&c.pieces).take(noted) {
                    let s = structure_of(w, b, p);
                    if s != held {
                        find(held, &within, &mut replayed, &mut kept.found);
                        held = s;
                    }
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

/// Notes a line's postings of the structures it held before each of
/// `structures`, the ones the changes of a run of its words lead to, from
/// `held` on, as [`find`] does: the structure held after them. Not inlined,
/// so that the registers it finds in are its own.
#[inline(never)]
fn find_changed(
    structures: &[u64],
    mut held: u64,
    within: &Within,
    replayed: &mut Replayed,
    found: &mut [u64; MAX_STRUCTURES],
) -> u64 {
    for &s in structures {
        if s != held {
            find(held, within, replayed, found);
            held = s;
        }
    }
    held
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
    let miscounted = || SearchError::Bug("the move stream does not replay to the postings it counted");
    on_workers(want, progress, pass.limits, |w| {
        let stopped = || w.stopped() || progress.stopped();
        let mut heads: Vec<&[u64]> = Vec::new();
        heads.try_reserve_exact(buffers.len()).map_err(|_| Refused::Busy)?;
        let mut room = Gathered::new(GATHERED)?;
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
            // The postings of one worker are put as they lie, in order; those
            // of several are gathered a run of buckets at a time.
            let one = heads.len() <= 1;
            room.count(if one { &[] } else { &heads });
            let mut gathered = if one { u64::MAX } else { low };
            for bucket in low..=high {
                if bucket >= gathered {
                    gathered = room.gather(&mut heads, bucket, high);
                }
                let (starts, ends) = (from <= bucket << GAME_BITS, to >= (bucket + 1) << GAME_BITS);
                // Only the pass's first bucket ends inside it ([`make_room`]).
                let count = match (starts, ends) {
                    (true, true) => Count::Whole,
                    (true, false) => Count::First(rest),
                    (false, _) => Count::After(pass.split.last),
                };
                let (n, last) = put_bucket(&mut heads, bucket, count, &mut bytes, room.bucket(bucket), &mut hand)?;
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

/// A worker's room for putting a block's buckets: the block's postings
/// counted by bucket, and those of a run of its buckets gathered from the
/// heads, `room` at most, each bucket's together, where each starts, and the
/// run's buckets.
struct Gathered {
    counts: [usize; BLOCK_BUCKETS],
    starts: [usize; BLOCK_BUCKETS],
    postings: Vec<u64>,
    room: usize,
    first: u64,
    end: u64,
}

/// A bucket's place among its block's.
fn local(bucket: u64) -> usize {
    bucket as usize & (BLOCK_BUCKETS - 1)
}

impl Gathered {
    /// Room for `room` postings.
    fn new(room: usize) -> Result<Gathered, SearchError> {
        let mut postings = Vec::new();
        postings.try_reserve_exact(room).map_err(|_| Refused::Busy)?;
        Ok(Gathered { counts: [0; BLOCK_BUCKETS], starts: [0; BLOCK_BUCKETS], postings, room, first: 0, end: 0 })
    }

    /// Counts the postings of one block in `heads` by bucket, and gathers
    /// none yet.
    fn count(&mut self, heads: &[&[u64]]) {
        self.counts.fill(0);
        for &p in heads.iter().flat_map(|h| h.iter()) {
            self.counts[local(bucket_of(p))] += 1;
        }
        (self.first, self.end) = (0, 0);
    }

    /// Gathers the postings of the buckets from `bucket` on, to `high` at
    /// most, that the room holds together, which lie first in the sorted
    /// `heads`, and takes them off the heads: each head's once, by bucket.
    /// A bucket that alone does not fit is left in the heads, to be merged.
    /// The bucket after them.
    fn gather(&mut self, heads: &mut [&[u64]], bucket: u64, high: u64) -> u64 {
        let (mut end, mut total) = (bucket, 0);
        while end <= high && total + self.counts[local(end)] <= self.room {
            self.starts[local(end)] = total;
            total += self.counts[local(end)];
            end += 1;
        }
        (self.first, self.end) = (bucket, end);
        if end == bucket {
            return bucket + 1;
        }
        self.postings.clear();
        self.postings.resize(total, 0);
        let mut at = self.starts;
        for h in heads.iter_mut() {
            let k = h.iter().take_while(|&&p| bucket_of(p) < end).count();
            for &p in &h[..k] {
                let i = &mut at[local(bucket_of(p))];
                if let Some(to) = self.postings.get_mut(*i) {
                    *to = p;
                }
                *i += 1;
            }
            *h = &h[k..];
        }
        end
    }

    /// Bucket `bucket`'s postings, sorted, when they were gathered.
    fn bucket(&mut self, bucket: u64) -> Option<&[u64]> {
        if !(self.first..self.end).contains(&bucket) {
            return None;
        }
        let at = self.starts[local(bucket)];
        let postings = self.postings.get_mut(at..at + self.counts[local(bucket)])?;
        postings.sort_unstable();
        Some(postings)
    }
}

/// Puts bucket `bucket`'s postings to `out`: those `gathered`, sorted, or
/// else those which lie first in the sorted `heads`, taken off the heads.
/// Writes what `count` says, then each by game from the one before, handing
/// `out` over whenever it holds half of [`OUT_BYTES`] before another.
/// Returns the postings put and the last game. A game's postings all come
/// from one worker's buffer, so none repeats another's, and a bucket's
/// postings gathered from every head and sorted are in the order of the
/// heads merged: a bucket that fits a worker's room is gathered and sorted
/// there ([`Gathered`]), a larger one merged.
fn put_bucket(
    heads: &mut [&[u64]],
    bucket: u64,
    count: Count,
    out: &mut Vec<u8>,
    gathered: Option<&[u64]>,
    hand: &mut dyn FnMut(&mut Vec<u8>) -> Result<(), SearchError>,
) -> Result<(u64, u64), SearchError> {
    let n: usize = match gathered {
        Some(postings) => postings.len(),
        None => heads.iter().map(|h| h.iter().take_while(|&&p| bucket_of(p) == bucket).count()).sum(),
    };
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
    if let Some(postings) = gathered {
        for &x in postings {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::format::bucket_games;

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
        // Each bucket sorted where it is gathered, with others or alone, or
        // merged from the heads, as a block's buckets are put.
        let mut outs = Vec::new();
        for room in [8, 2, 0] {
            let mut room = Gathered::new(room).unwrap();
            let mut heads: Vec<&[u64]> = vec![&a, &b];
            let (mut out, mut kept, mut gathered) = (Vec::new(), 0, 0);
            room.count(&heads);
            for bucket in 0..BLOCK_BUCKETS as u64 {
                if bucket >= gathered {
                    gathered = room.gather(&mut heads, bucket, BLOCK_BUCKETS as u64 - 1);
                }
                let hand = &mut |_: &mut Vec<u8>| Ok(());
                kept += put_bucket(&mut heads, bucket, Count::Whole, &mut out, room.bucket(bucket), hand).unwrap().0;
            }
            assert_eq!(kept, 5);
            assert!(heads.iter().all(|h| h.is_empty()));
            outs.push(out);
        }
        assert!(outs[0] == outs[1] && outs[1] == outs[2]);
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
        let mut room = Gathered::new(GATHERED).unwrap();
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
            room.count(&heads);
            assert_eq!(room.gather(&mut heads, bucket, bucket), bucket + 1);
            let (n, last) = put_bucket(&mut heads, bucket, count, &mut out, room.bucket(bucket), &mut hand).unwrap();
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
}
