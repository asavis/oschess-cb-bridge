//! The tree's passes of a build (#147): every position within the first
//! [`MAX_PLY`] plies of each game, replayed from the move stream the build
//! has just written, a range of the parts of the keys ([`part_of`]) at a
//! time, as many as the build's share of the budget holds.
//!
//! In a pass, each worker replays the games it takes, following their keys
//! through the move words alone ([`Keys`]), and keeps the entries of the
//! pass's parts in a buffer of its own. A full buffer is sorted by key and
//! each crowded position folded ([`fold`]): its best games kept whole, the
//! others counted by move and outcome. When that leaves too little room, the
//! pass ends at an earlier part for every worker, and the next pass starts
//! there. The positions that games from the standard start reach first
//! within [`SHALLOW_PLY`] plies, the most crowded, the stream pass has folded
//! already ([`Shallow`]): the passes leave them out. Then the workers take
//! the pass's parts in key order, each merging one part's entries from every
//! buffer and from those folded, adding each position up and making its
//! blocks, and write them once the parts before have been placed: the tree is
//! written once, in key order, and its blocks end where parts end, so any
//! number of passes gives the same file.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use crate::indexdir::crc32_update;
use crate::search::SearchError;
use crate::search::memory::{Hold, Refused};
use crate::search::workers::threads;

use super::build::{Chunks, Counted, Out, Passes, Turns, from_bad, plan_pass};
use super::follow::{Keys, standard_keys};
use super::format::{
    BLOCK_DATA, BLOCK_ENTRY, BLOCK_KEYS, Block, Counts, KEY_ENTRY, MAX_BLOCK_DATA, MAX_PLY, NO_MOVE, TOP_GAMES,
    encode_record, order_moves, part_of, rank_top,
};
use super::runs::{ENTRY_BYTES, Entry, Limits, MAX_GAME, PassTime, Progress, grow, on_workers};
use super::stream::{self, Stream};

/// A position's run of entries this long or shorter is left as it is when a
/// buffer is folded.
const FOLD_MIN: usize = 2 * TOP_GAMES;
/// The most entries a position folds into: its notable games, and a
/// weighted entry for each move, or none, and outcome.
pub(super) const FOLD_ENTRIES: usize = TOP_GAMES + 219 * 4;
/// The positions a game from the standard start reaches first within this
/// many plies: at most 9,323 whatever the database, and the most crowded of
/// all. The stream pass folds their entries as it reads the games
/// ([`Shallow`]), and the tree's passes leave them out.
pub(super) const SHALLOW_PLY: u8 = 3;
/// The most and the fewest entries of those positions a worker of the stream
/// pass holds, folded whenever they fill its room: a worker sees nearly all
/// of them, which fold into a few dozen entries each.
pub(super) const SHALLOW_ENTRIES: usize = 1 << 20;
pub(super) const MIN_SHALLOW_ENTRIES: usize = 1 << 14;
/// The least room a worker's entries take.
const MIN_WORKER_ENTRIES: usize = 64;
/// The blocks a worker hands over at once, a block's worth, and its share of
/// those kept until their turn to be written comes.
const OUT_BYTES: usize = BLOCK_KEYS * KEY_ENTRY + MAX_BLOCK_DATA;
/// What a worker holds besides its entries: a block being made, the blocks
/// made and not yet handed over, its share of those kept, and the room a
/// crowded position folds in.
pub const WORKER_BYTES: usize = BLOCK_KEYS * KEY_ENTRY + MAX_BLOCK_DATA + 2 * OUT_BYTES + FOLD_ENTRIES * ENTRY_BYTES;

/// A worker's entries of the positions [`SHALLOW_PLY`] names, folded as they
/// fill its room.
pub(super) struct Shallow {
    entries: Vec<Entry>,
    room: usize,
    scratch: Vec<Entry>,
}

impl Shallow {
    /// What a worker holds for `room` entries: them, and the room a crowded
    /// position folds in.
    pub fn bytes(room: usize) -> usize {
        (room + FOLD_ENTRIES) * ENTRY_BYTES
    }

    /// Room for `room` entries, allocated fallibly, which the worker holds in
    /// the budget first ([`Shallow::bytes`]); none for 0.
    pub fn new(room: usize) -> Option<Shallow> {
        let (mut entries, mut scratch) = (Vec::new(), Vec::new());
        if room > 0 {
            entries.try_reserve_exact(room).ok()?;
            scratch.try_reserve_exact(FOLD_ENTRIES).ok()?;
        }
        Some(Shallow { entries, room, scratch })
    }

    /// Adds `entry`; `false` once the entries no longer fold into half the
    /// room, which a database of games from the standard start comes near
    /// only with a small share of the budget, and then the build does
    /// without them.
    pub fn add(&mut self, entry: Entry) -> bool {
        if self.entries.len() >= self.room {
            self.fold();
            if self.entries.len() > self.room / 2 {
                return false;
            }
        }
        self.entries.push(entry);
        true
    }

    /// Sorts the entries and folds them.
    fn fold(&mut self) {
        self.entries.sort_unstable_by_key(|e| e.key);
        fold(&mut self.entries, &mut self.scratch);
    }

    /// Every worker's entries, each with the hold of its room, sorted and
    /// folded together: each worker's into the first one's room, which holds
    /// two, whose entries then fold into half of it again, as nearly always,
    /// the one merged giving its room back at once. The entries and their
    /// hold; `None` when they do not fold into the room, and the build does
    /// without them.
    pub fn merge(all: Vec<(Shallow, Hold)>, progress: &Progress) -> Result<Option<(Vec<Entry>, Hold)>, SearchError> {
        let mut all = all.into_iter();
        let Some((mut into, mut hold)) = all.next() else { return Ok(Some((Vec::new(), Hold::default()))) };
        into.fold();
        for (mut other, _room) in all {
            other.fold();
            if into.entries.len() + other.entries.len() > into.room {
                return Ok(None);
            }
            into.entries.extend_from_slice(&other.entries);
            into.fold();
        }
        // What the entries take, held before the room is given back.
        let len = into.entries.len();
        grow(&mut hold, len * ENTRY_BYTES, progress)?;
        let mut merged = Vec::new();
        merged.try_reserve_exact(len).map_err(|_| Refused::Busy)?;
        merged.extend_from_slice(&into.entries);
        drop(into);
        hold.shrink(len * ENTRY_BYTES);
        Ok(Some((merged, hold)))
    }
}

/// The tree as written: its positions and blocks, and where its table is.
pub(super) struct Tree {
    pub keys: u64,
    pub blocks: u32,
    pub table_offset: u64,
    pub table_crc: u32,
}

/// Writes the tree of the games in `stream` to `out`, then its table, within
/// `share` bytes of the budget: `counted` holds the entries of each part of
/// the keys, of `part_bits` bits, as the stream's pass counted them, which the
/// tree must hold as many of, those it folded, and those entries folded.
pub(super) fn write(
    stream: &Stream,
    counted: &Counted,
    part_bits: u8,
    out: &mut Out,
    progress: &Progress,
    share: usize,
    limits: &Limits,
) -> Result<Tree, SearchError> {
    let counts = &counted.entries;
    let total: u64 = counts.iter().sum();
    progress.start("positions", total);
    // The entries the passes collect: all but those the stream pass folded.
    let collected = |part: usize| counts[part] - counted.shallow[part];
    let folded = &counted.folded[..];
    // The table: a block a part, and one for every 1,024 entries more, which
    // grows as it must.
    let table_bytes = BLOCK_ENTRY * (counts.len() + (total / 1024) as usize + 16);
    // The room of a worker whose pass starts at the largest part (`needed`).
    let largest = (0..counts.len()).map(|part| needed(collected(part))).max().unwrap_or(MIN_WORKER_ENTRIES);
    let passes = Passes {
        table: table_bytes,
        worker: WORKER_BYTES,
        item: ENTRY_BYTES,
        items: (0..counts.len()).map(collected).sum(),
        least: MIN_WORKER_ENTRIES,
        largest: largest.saturating_mul(ENTRY_BYTES),
    };
    let room = passes.room(share, threads(), stream.header.records(), limits.pass_bytes)?;
    let (_memory, want, room) = room.reserve(progress)?;
    let (capacity, want) = passes.capacity(room, want);
    let mut table = Vec::new();
    table.try_reserve_exact(table_bytes).map_err(|_| Refused::Busy)?;
    let table_memory = Hold::default();
    let mut sink = Sink { at: out.offset, table, table_memory, keys: 0, blocks: 0, games: 0, progress };
    let mut first = 0;
    while first < counts.len() {
        let (end, planned) = plan_pass(first, counts.len(), capacity, collected);
        let hi = AtomicUsize::new(end);
        progress.tree_passes.fetch_add(1, Ordering::Relaxed);
        let pass = Pass { stream, part_bits, first, hi: &hi, capacity, planned, folded, progress, limits };
        let started = Instant::now();
        // As many workers as each hold the pass's first part in its share
        // of the room, one at least.
        let buffers = pass.collect(want.min(capacity / needed(collected(first))).max(1))?;
        let replayed = Instant::now();
        let end = hi.load(Ordering::Relaxed);
        write_parts(&buffers, &pass, end, counts, &mut sink, out, want)?;
        out.sync_behind();
        let time = PassTime { replay: replayed - started, write: replayed.elapsed() };
        progress.time(|t| t.tree.push(time));
        first = end;
    }
    if sink.games != total {
        return Err(SearchError::Bug("the move stream does not replay to the positions it was read with"));
    }
    out.offset = sink.at;
    let table_offset = out.offset;
    out.put(&sink.table)?;
    let blocks = u32::try_from(sink.blocks).map_err(|_| SearchError::TooLarge)?;
    Ok(Tree { keys: sink.keys, blocks, table_offset, table_crc: !crc32_update(!0, &sink.table) })
}

/// One pass: the parts of the keys from `first` to `hi`, which a worker lowers
/// when its entries do not fit.
struct Pass<'a> {
    stream: &'a Stream,
    part_bits: u8,
    first: usize,
    hi: &'a AtomicUsize,
    /// The entries all workers' buffers hold together, and those planned.
    capacity: usize,
    planned: u64,
    /// The entries of the positions that [`SHALLOW_PLY`] names, sorted and
    /// folded by the stream pass, which the pass then leaves out; none when
    /// it did without.
    folded: &'a [Entry],
    progress: &'a Progress,
    limits: &'a Limits,
}

impl Pass<'_> {
    fn part(&self, key: u64) -> usize {
        part_of(key, self.part_bits)
    }

    /// Each worker's entries of the pass's parts, sorted by key, of up to
    /// `want` workers.
    fn collect(&self, want: usize) -> Result<Vec<Vec<Entry>>, SearchError> {
        let (first, last) = (self.stream.header.first_record, self.stream.header.last_record);
        let chunks = Chunks::new(first, last, want);
        let spare = chunks.spare(self.planned);
        on_workers(want, self.progress, self.limits, |w| {
            let cap = self.capacity / w.count;
            let mut buf: Vec<Entry> = Vec::new();
            buf.try_reserve_exact(cap).map_err(|_| Refused::Busy)?;
            let mut scratch: Vec<Entry> = Vec::new();
            scratch.try_reserve_exact(FOLD_ENTRIES).map_err(|_| Refused::Busy)?;
            let mut taker = chunks.taker();
            loop {
                // A background build gives way to foreground work before it
                // takes its next chunk, so that none waits for it (#149).
                self.progress.give_way();
                let Some((lo, hi)) = taker.take(buf.len() + spare > cap) else { break };
                if w.stopped() || self.progress.stopped() {
                    return Err(SearchError::Superseded);
                }
                for game in lo..=hi {
                    self.replay(game, &mut buf, cap, &mut scratch)?;
                }
            }
            buf.sort_unstable_by_key(|e| e.key);
            fold(&mut buf, &mut scratch);
            Ok(buf)
        })
    }

    /// Adds the entries of game `game`'s first positions that lie in the
    /// pass to `buf`: each position once, at its first visit, with the move
    /// played from there, as the walk that wrote the stream met them; but
    /// for those the stream pass folded. The line's keys are followed first,
    /// a lookup a ply, and a pass's parts hold few of them, so only those it
    /// holds are looked for among the ones before.
    fn replay(&self, game: u32, buf: &mut Vec<Entry>, cap: usize, scratch: &mut Vec<Entry>) -> Result<(), SearchError> {
        let path = &self.stream.path;
        let record = self.stream.written(game).map_err(|e| from_bad(path, e))?;
        let entry = record.entry;
        if !entry.indexed() {
            return Ok(());
        }
        let start = record.start().map_err(|e| from_bad(path, e))?;
        let mut line = match &start {
            Some(board) => Keys::of(board).ok_or(SearchError::Bug("stream start"))?,
            None => standard_keys(),
        };
        // The positions to ply 20 and the moves played from them are the
        // prefix's words, the last one's move its 21st.
        const { assert!(stream::PREFIX_WORDS == MAX_PLY as usize + 1) };
        let (words, _) = record.word_parts();
        let mut keys = [0u64; MAX_PLY as usize + 1];
        let mut moves = [NO_MOVE; MAX_PLY as usize + 1];
        let mut reached = 0;
        for (ply, (key, mv)) in keys.iter_mut().zip(&mut moves).enumerate() {
            *key = line.hash();
            reached = ply + 1;
            let Some(w) = words.get(ply) else { break };
            let s = line.step(u16::from_le_bytes(*w)).ok_or(SearchError::Bug("stream word"))?;
            *mv = s.mv;
            if ply < usize::from(MAX_PLY) {
                line.apply(s);
            }
        }
        let folded = if !self.folded.is_empty() && start.is_none() { usize::from(SHALLOW_PLY) + 1 } else { 0 };
        // The plies whose positions lie in the pass's parts, without a branch
        // a ply; the pass may end earlier as they are added, never later.
        let hi = self.hi.load(Ordering::Relaxed);
        let mut held = 0u32;
        for (ply, &key) in keys.iter().enumerate().take(reached).skip(folded) {
            let part = self.part(key);
            held |= u32::from(part >= self.first && part < hi) << ply;
        }
        while held != 0 {
            let ply = held.trailing_zeros() as usize;
            held &= held - 1;
            let key = keys[ply];
            let part = self.part(key);
            if part >= self.hi.load(Ordering::Relaxed) || keys[..ply].contains(&key) {
                continue;
            }
            if buf.len() >= cap {
                make_room(buf, cap, scratch, self.first, self.hi, self.part_bits)?;
            }
            if part < self.hi.load(Ordering::Relaxed) {
                buf.push(Entry::new(key, game, entry.outcome(), moves[ply], entry.elo()));
            }
        }
        Ok(())
    }
}

/// The entries a worker's buffer holds so that a pass whose first part has
/// `entries` entries, as the stream pass counted them, never finds that
/// part too large for it ([`make_room`]), however little they fold: an
/// eighth more, and [`MIN_WORKER_ENTRIES`] at least.
fn needed(entries: u64) -> usize {
    usize::try_from(entries + entries / 7 + 1).unwrap_or(usize::MAX).max(MIN_WORKER_ENTRIES)
}

/// Makes room in `buf`, a worker's full buffer of `cap` entries in a pass of
/// the parts from `first` to `hi`, of `part_bits` bits: its entries sorted
/// and folded, and when they still take three quarters of it, the pass ended
/// for every worker at the part that keeps about half. A first part that
/// alone leaves no room is too large for the share: a worker holds what the
/// part [`needed`] whenever the share holds that, so only a share too small
/// for one worker and the part's folded entries fails so.
fn make_room(
    buf: &mut Vec<Entry>,
    cap: usize,
    scratch: &mut Vec<Entry>,
    first: usize,
    hi: &AtomicUsize,
    part_bits: u8,
) -> Result<(), SearchError> {
    let part = |e: &Entry| part_of(e.key, part_bits);
    buf.sort_unstable_by_key(|e| e.key);
    let end = hi.load(Ordering::Relaxed);
    buf.truncate(buf.partition_point(|e| part(e) < end));
    fold(buf, scratch);
    if buf.len() > cap / 4 * 3 {
        let cut = part(&buf[cap / 2]).max(first + 1);
        hi.fetch_min(cut, Ordering::Relaxed);
        buf.truncate(buf.partition_point(|e| part(e) < cut));
        if buf.len() > cap - cap / 8 {
            return Err(SearchError::TooLarge);
        }
    }
    Ok(())
}

/// Folds each position whose run of entries in the sorted `buf` is longer
/// than [`FOLD_MIN`]: its [`TOP_GAMES`] best games are kept whole, and every
/// other entry is counted into a weighted entry of its move and outcome, so
/// that a position however crowded takes a few hundred entries at most. The
/// position adds up to what it did. `scratch` holds [`FOLD_ENTRIES`].
fn fold(buf: &mut Vec<Entry>, scratch: &mut Vec<Entry>) {
    let n = buf.len();
    let (mut to, mut i) = (0, 0);
    while i < n {
        let key = buf[i].key;
        let mut j = i + 1;
        while j < n && buf[j].key == key {
            j += 1;
        }
        if j - i <= FOLD_MIN {
            buf.copy_within(i..j, to);
            to += j - i;
            i = j;
            continue;
        }
        // The best games whole, best first, then the weighted entries.
        scratch.clear();
        let mut best = 0;
        let rank = |e: &Entry| (e.elo(), e.game());
        for &e in &buf[i..j] {
            let folded = if e.is_weighted() {
                Some(e)
            } else if best < TOP_GAMES {
                let at = scratch[..best].partition_point(|b| rank(b) > rank(&e));
                scratch.insert(at, e);
                best += 1;
                None
            } else if rank(&e) > rank(&scratch[best - 1]) {
                let worst = scratch.remove(best - 1);
                let at = scratch[..best - 1].partition_point(|b| rank(b) > rank(&e));
                scratch.insert(at, e);
                Some(worst)
            } else {
                Some(e)
            };
            if let Some(f) = folded {
                match scratch[best..].iter_mut().find(|w| w.mv() == f.mv() && w.outcome() == f.outcome()) {
                    Some(w) => {
                        let games = (w.games() + f.games()).min(u64::from(MAX_GAME)) as u32;
                        *w = Entry::weighted(key, games, f.outcome(), f.mv());
                    }
                    None => scratch.push(Entry::weighted(key, f.games() as u32, f.outcome(), f.mv())),
                }
            }
        }
        // Never more than the run: each weighted entry folds one at least.
        buf[to..to + scratch.len()].copy_from_slice(scratch);
        to += scratch.len();
        i = j;
    }
    buf.truncate(to);
}

/// Where the tree's parts go, one part after another.
struct Sink<'a> {
    /// Where the next part's bytes go.
    at: u64,
    table: Vec<u8>,
    /// What the table holds beyond its first room, which the passes' hold
    /// holds.
    table_memory: Hold,
    keys: u64,
    blocks: u64,
    /// The games of the positions placed, which add up to the entries.
    games: u64,
    progress: &'a Progress,
}

impl Sink<'_> {
    /// Places the blocks `made` after those placed, and adds them to the
    /// table: their bytes, and where they go.
    fn place(&mut self, made: Made) -> Result<(u64, Vec<u8>), SearchError> {
        let base = self.at;
        self.at += made.out.len() as u64;
        let more = made.table.len() * BLOCK_ENTRY;
        if self.table.len() + more > self.table.capacity() {
            let step = more.max(64 << 10);
            grow(&mut self.table_memory, step, self.progress)?;
            self.table.try_reserve_exact(step).map_err(|_| Refused::Busy)?;
        }
        for b in made.table {
            Block { offset: base + b.offset, ..b }.encode(&mut self.table);
        }
        self.blocks += (more / BLOCK_ENTRY) as u64;
        self.keys += made.keys;
        self.games += made.games;
        self.progress.positions.fetch_add(made.keys, Ordering::Relaxed);
        self.progress.done.fetch_add(made.done, Ordering::Relaxed);
        Ok((base, made.out))
    }
}

/// Blocks of a part, made and handed over: their bytes and their table at
/// offsets from the first of them, the positions and the games they count,
/// and the entries of the part when they end it.
struct Made {
    out: Vec<u8>,
    table: Vec<Block>,
    keys: u64,
    games: u64,
    done: u64,
}

/// Writes parts `pass.first..end` of the sorted `buffers` and of the
/// entries the stream pass folded on up to `want` workers, in key order.
fn write_parts(
    buffers: &[Vec<Entry>],
    pass: &Pass<'_>,
    end: usize,
    counts: &[u64],
    sink: &mut Sink<'_>,
    out: &Out,
    want: usize,
) -> Result<(), SearchError> {
    let first = pass.first;
    let place = |sink: &mut &mut Sink<'_>, made: Made| sink.place(made);
    let write = |(at, bytes): (u64, Vec<u8>)| out.write(at, &bytes);
    let turns = Turns::new(end - first, want * OUT_BYTES, sink, &place, &write);
    let progress = pass.progress;
    on_workers(want, progress, pass.limits, |w| {
        let stopped = || w.stopped() || progress.stopped();
        let mut made = Blocks::new().ok_or(Refused::Busy)?;
        let mut agg = Aggregate::new().ok_or(Refused::Busy)?;
        let mut heads: Vec<&[Entry]> = Vec::new();
        heads.try_reserve_exact(buffers.len() + 1).map_err(|_| Refused::Busy)?;
        loop {
            // A background build gives way to foreground work before it
            // takes its next part, so that none waits for it (#149).
            progress.give_way();
            let Some(unit) = turns.take() else { break };
            if stopped() {
                return Err(SearchError::Superseded);
            }
            let part = first + unit;
            heads.clear();
            for b in buffers.iter().map(|b| &b[..]).chain([pass.folded]) {
                let from = b.partition_point(|e| pass.part(e.key) < part);
                let to = from + b[from..].partition_point(|e| pass.part(e.key) == part);
                if to > from {
                    heads.push(&b[from..to]);
                }
            }
            // The least key of the heads, and all its entries from each.
            while let Some(key) = heads.iter().filter_map(|h| h.first()).map(|e| e.key).min() {
                for h in heads.iter_mut() {
                    let n = h.iter().take_while(|e| e.key == key).count();
                    for e in &h[..n] {
                        agg.add(e);
                    }
                    *h = &h[n..];
                }
                agg.emit(key, &mut made)?;
                if made.out.len() >= OUT_BYTES / 2 {
                    let bytes = made.out.len();
                    turns.put(unit, made.hand(0), bytes, false, &stopped)?;
                }
            }
            made.end_block();
            let bytes = made.out.len();
            turns.put(unit, made.hand(counts[part]), bytes, true, &stopped)?;
        }
        Ok(())
    })?;
    Ok(())
}

/// One position's entries, added up as a part's merge passes them.
struct Aggregate {
    count: Counts,
    moves: Vec<(u16, Counts)>,
    /// The best games so far, best first: (rating, game).
    top: Vec<(u16, u32)>,
}

impl Aggregate {
    fn new() -> Option<Aggregate> {
        let mut moves = Vec::new();
        moves.try_reserve_exact(256).ok()?;
        let mut top = Vec::new();
        top.try_reserve_exact(TOP_GAMES + 1).ok()?;
        Some(Aggregate { count: Counts::default(), moves, top })
    }

    fn add(&mut self, e: &Entry) {
        let (outcome, games) = (e.outcome(), e.games());
        self.count.add_games(outcome, games);
        if e.mv() != NO_MOVE {
            match self.moves.iter_mut().find(|m| m.0 == e.mv()) {
                Some(m) => m.1.add_games(outcome, games),
                None => {
                    let mut c = Counts::default();
                    c.add_games(outcome, games);
                    self.moves.push((e.mv(), c));
                }
            }
        }
        if e.is_weighted() {
            return;
        }
        rank_top(&mut self.top, (e.elo(), e.game()));
    }

    /// Writes the position `key` to `made`, its moves in a record's order,
    /// and starts afresh.
    fn emit(&mut self, key: u64, made: &mut Blocks) -> Result<(), SearchError> {
        order_moves(&mut self.moves);
        made.push(key, &self.count, &self.moves, &self.top)?;
        made.games += self.count.games;
        self.count = Counts::default();
        self.moves.clear();
        self.top.clear();
        Ok(())
    }
}

/// The blocks of a part being made: a block's keys and records, and the
/// blocks made and not yet handed over, with their table at offsets from the
/// first of them.
struct Blocks {
    keys: Vec<u8>,
    data: Vec<u8>,
    first_key: u64,
    in_block: usize,
    out: Vec<u8>,
    table: Vec<Block>,
    keys_made: u64,
    games: u64,
}

impl Blocks {
    fn new() -> Option<Blocks> {
        fn buf<T>(n: usize) -> Option<Vec<T>> {
            let mut v = Vec::new();
            v.try_reserve_exact(n).ok()?;
            Some(v)
        }
        Some(Blocks {
            keys: buf(BLOCK_KEYS * KEY_ENTRY)?,
            data: buf(MAX_BLOCK_DATA)?,
            first_key: 0,
            in_block: 0,
            out: Vec::new(),
            table: Vec::new(),
            keys_made: 0,
            games: 0,
        })
    }

    /// The blocks made, handed over, which end their part with its `done`
    /// entries, or 0.
    fn hand(&mut self, done: u64) -> Made {
        Made {
            out: std::mem::take(&mut self.out),
            table: std::mem::take(&mut self.table),
            keys: std::mem::take(&mut self.keys_made),
            games: std::mem::take(&mut self.games),
            done,
        }
    }

    fn push(
        &mut self,
        key: u64,
        counts: &Counts,
        moves: &[(u16, Counts)],
        top: &[(u16, u32)],
    ) -> Result<(), SearchError> {
        if self.in_block == 0 {
            self.first_key = key;
        }
        let at = u32::try_from(self.data.len()).map_err(|_| SearchError::TooLarge)?;
        self.keys.extend(key.to_le_bytes());
        self.keys.extend(at.to_le_bytes());
        encode_record(&mut self.data, counts, moves, top.iter().map(|t| t.1));
        self.in_block += 1;
        self.keys_made += 1;
        if self.in_block == BLOCK_KEYS || self.data.len() >= BLOCK_DATA {
            self.end_block();
        }
        Ok(())
    }

    /// Ends the block being made, if any: a block ends with its part.
    fn end_block(&mut self) {
        if self.in_block == 0 {
            return;
        }
        let crc = !crc32_update(crc32_update(!0, &self.keys), &self.data);
        self.table.push(Block {
            first_key: self.first_key,
            offset: self.out.len() as u64,
            keys: self.in_block as u32,
            data_len: self.data.len() as u32,
            crc,
        });
        self.out.extend_from_slice(&self.keys);
        self.out.extend_from_slice(&self.data);
        self.keys.clear();
        self.data.clear();
        self.in_block = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::format::Outcome;

    /// A position's counts, its moves' counts, by move, and its best games.
    type Added = (Counts, Vec<(u16, Counts)>, Vec<(u16, u32)>);

    /// What `entries` of one position add up to.
    fn added(entries: &[Entry]) -> Added {
        let mut a = Aggregate::new().unwrap();
        for e in entries {
            a.add(e);
        }
        a.moves.sort_unstable_by_key(|m| m.0);
        (a.count, a.moves, a.top)
    }

    #[test]
    fn a_folded_position_adds_up_to_the_same() {
        let outcomes = [Outcome::White, Outcome::Draw, Outcome::Black, Outcome::Other];
        let mut all = Vec::new();
        for g in 1..=5_000u32 {
            let mv = if g % 11 == 0 { NO_MOVE } else { (g % 7) as u16 + 1 };
            all.push(Entry::new(9, g, outcomes[g as usize % 4], mv, (g * 7919 % 3000) as u16));
        }
        // A position of few entries beside it stays as it is.
        let few: Vec<Entry> = (1..=5u32).map(|g| Entry::new(10, g, Outcome::Draw, 3, 100)).collect();
        let mut buf: Vec<Entry> = all.iter().chain(&few).copied().collect();
        let mut scratch = Vec::with_capacity(FOLD_ENTRIES);
        fold(&mut buf, &mut scratch);
        assert!(buf.len() <= TOP_GAMES + 8 * 4 + few.len(), "{} entries", buf.len());
        assert_eq!(&buf[buf.len() - few.len()..], &few[..]);
        let folded: Vec<Entry> = buf.iter().filter(|e| e.key == 9).copied().collect();
        assert_eq!(added(&folded), added(&all));
        // Folded again with more of the same position, as a buffer that
        // fills again is.
        let more: Vec<Entry> = (5_001..=6_000u32).map(|g| Entry::new(9, g, Outcome::White, 2, 4000)).collect();
        let mut again: Vec<Entry> = folded.iter().chain(&more).copied().collect();
        fold(&mut again, &mut scratch);
        let everything: Vec<Entry> = all.iter().chain(&more).copied().collect();
        assert_eq!(added(&again), added(&everything));
        assert_eq!(added(&again).2.len(), TOP_GAMES);
        assert!(added(&again).2.iter().all(|t| t.0 == 4000), "the best games are the later ones");
    }

    /// A full buffer folds a crowded position and goes on; one of many
    /// positions ends the pass for every worker at the part that keeps about
    /// half of it, below where another worker ended it already; a first part
    /// that alone fills it is too large.
    #[test]
    fn a_full_buffer_folds_then_ends_the_pass_earlier() {
        let bits = 4;
        let key = |part: u64, i: u64| part << 60 | i;
        let mut scratch = Vec::with_capacity(FOLD_ENTRIES);
        let hi = AtomicUsize::new(16);
        // Part 3's start, reached by a thousand games, and a few others.
        let mut buf: Vec<Entry> = (1..=1_000).map(|g| Entry::new(key(3, 0), g, Outcome::Draw, 5, 2000)).collect();
        buf.extend((0..24).map(|i| Entry::new(key(9, i), 1, Outcome::White, 5, 2000)));
        make_room(&mut buf, 1_024, &mut scratch, 2, &hi, bits).unwrap();
        assert_eq!((buf.len(), hi.load(Ordering::Relaxed)), (TOP_GAMES + 1 + 24, 16), "folded, the pass as it was");
        // Distinct positions of parts 2 to 9: cut at the part of the middle one.
        let mut buf: Vec<Entry> =
            (0..1_024).map(|i| Entry::new(key(2 + i / 128, i), 1, Outcome::White, 5, 2000)).collect();
        make_room(&mut buf, 1_024, &mut scratch, 2, &hi, bits).unwrap();
        assert_eq!(hi.load(Ordering::Relaxed), 6);
        assert_eq!(buf.len(), 512, "parts 2 to 5 kept");
        assert!(buf.iter().all(|e| part_of(e.key, bits) < 6));
        // Another worker, whose part 7 lies beyond where the pass ends now.
        let mut other: Vec<Entry> =
            (0..1_024).map(|i| Entry::new(key(2 + i / 200, i), 1, Outcome::White, 5, 2000)).collect();
        make_room(&mut other, 1_024, &mut scratch, 2, &hi, bits).unwrap();
        assert!(hi.load(Ordering::Relaxed) <= 6 && other.iter().all(|e| part_of(e.key, bits) < 6));
        // Part 2 alone fills the buffer with distinct positions.
        let mut full: Vec<Entry> = (0..1_024).map(|i| Entry::new(key(2, i), 1, Outcome::White, 5, 2000)).collect();
        assert!(matches!(make_room(&mut full, 1_024, &mut scratch, 2, &hi, bits), Err(SearchError::TooLarge)));
    }

    /// A buffer of the entries a part [`needed`] holds that part whole, of
    /// distinct positions that never fold, filled up with the next part's:
    /// a pass that starts at it ends past it, whatever its size.
    #[test]
    fn a_buffer_of_what_a_part_needed_holds_it() {
        let bits = 4;
        let entry = |part: u64, i: u64| Entry::new(part << 60 | i, 1, Outcome::White, 5, 2000);
        let mut scratch = Vec::with_capacity(FOLD_ENTRIES);
        for n in (0..=700u64).chain([4_095, 65_535]) {
            let cap = needed(n);
            assert!(cap >= MIN_WORKER_ENTRIES && cap as u64 > n, "{n}");
            let mut buf: Vec<Entry> = (0..n).map(|i| entry(2, i)).collect();
            buf.extend((0..cap as u64 - n).map(|i| entry(3, i)));
            let hi = AtomicUsize::new(16);
            make_room(&mut buf, cap, &mut scratch, 2, &hi, bits).unwrap_or_else(|e| panic!("{n}: {e:?}"));
            assert!(hi.load(Ordering::Relaxed) >= 3, "{n}: the pass ends past part 2");
            assert_eq!(buf.iter().filter(|e| part_of(e.key, bits) == 2).count() as u64, n);
        }
    }

    /// A worker's entries of the shallow positions fold into its room as
    /// they come, then together with the other workers', sorted, and add up
    /// to what they did. A worker whose entries no longer fold into half its
    /// room gives them up, and so do workers whose entries together do not
    /// fit one room.
    #[test]
    fn shallow_entries_fold_as_they_come() {
        let outcomes = [Outcome::White, Outcome::Draw, Outcome::Black, Outcome::Other];
        let (room, progress) = (1_024, Progress::default());
        let (mut workers, mut all) = (Vec::new(), Vec::new());
        for w in 0..3u32 {
            let mut shallow = Shallow::new(room).unwrap();
            for g in 1..=3_000u32 {
                let game = w * 3_000 + g;
                let e = Entry::new(u64::from(game % 5), game, outcomes[game as usize % 4], (game % 7) as u16 + 1, 2000);
                assert!(shallow.add(e));
                all.push(e);
            }
            workers.push((shallow, Hold::default()));
        }
        let (merged, hold) = Shallow::merge(workers, &progress).unwrap().unwrap();
        assert!(merged.windows(2).all(|w| w[0].key <= w[1].key));
        assert_eq!(hold.bytes(), merged.len() * ENTRY_BYTES);
        for key in 0..5 {
            let of = |entries: &[Entry]| added(&entries.iter().filter(|e| e.key == key).copied().collect::<Vec<_>>());
            assert_eq!(of(&merged), of(&all), "position {key}");
        }
        // Distinct positions fill half the room.
        let mut shallow = Shallow::new(room).unwrap();
        assert!((0..2 * room as u64).any(|k| !shallow.add(Entry::new(k, 1, Outcome::Draw, 3, 100))));
        // Two workers' distinct positions, half a room each, do not fit one.
        let workers = (0..2u64)
            .map(|w| {
                let mut shallow = Shallow::new(room).unwrap();
                for k in 0..room as u64 / 2 + 1 {
                    assert!(shallow.add(Entry::new(w << 32 | k, 1, Outcome::Draw, 3, 100)));
                }
                (shallow, Hold::default())
            })
            .collect();
        assert!(Shallow::merge(workers, &progress).unwrap().is_none());
        assert!(Shallow::merge(Vec::new(), &progress).unwrap().unwrap().0.is_empty());
    }

    #[test]
    fn a_position_keeps_its_best_games_and_counts_each_move() {
        let mut a = Aggregate::new().unwrap();
        for g in 1..=20u32 {
            let outcome = [Outcome::White, Outcome::Draw, Outcome::Black, Outcome::Other][g as usize % 4];
            a.add(&Entry::new(9, g, outcome, if g % 2 == 0 { 70 } else { 71 }, (g * 100) as u16));
        }
        a.add(&Entry::weighted(9, 1_000, Outcome::White, 70));
        assert_eq!(a.count, Counts { games: 1_020, white: 1_005, draws: 5, black: 5 });
        assert_eq!(a.top.len(), TOP_GAMES);
        assert_eq!(a.top[0], (2000, 20));
        assert_eq!(a.moves.iter().map(|m| m.1.games).sum::<u64>(), 1_020);
    }
}
