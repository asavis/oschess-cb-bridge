//! A whole build (#147), in three kinds of pass, which write nothing but the
//! two files they build, `<id>.moves.partial` and `<id>.idx.partial`:
//!
//! 1. The stream pass reads the database's games once, writes their main
//!    lines as the move stream, counts the tree's entries in each part of
//!    the keys and the deep section's postings in each of its blocks, and
//!    folds the entries of the few, crowded positions that games reach first
//!    within [`tree::SHALLOW_PLY`] plies.
//! 2. The tree's passes replay each game's first positions from the stream,
//!    for as many parts of the keys at a time as the build's share of the
//!    budget holds, and write the tree in key order ([`super::tree`]).
//! 3. The deep section's passes replay each whole line, for as many buckets
//!    at a time, and write its blocks in order ([`super::deep`]).
//!
//! A pass plans nearly all its room ([`PLANNED`]), and its workers share it
//! whatever their pace ([`Taker`]); they write what they make at its place
//! in the file, in order ([`Turns`]), while a thread syncs what each pass
//! wrote behind them ([`Out`]). Both files carry one build id and are renamed
//! into place at the end: the stream first, then the index. Where the time
//! went, phase by phase and pass by pass, is noted in the build's
//! [`Progress`] ([`super::runs::Timings`]).

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::indexdir;
use crate::search::SearchError;
use crate::search::memory::{Hold, Refused};
use crate::search::workers::{Worker, threads};
use crate::sync::{lock, unpoisoned};

use super::deep;
use super::file::{Bad, write_at};
use super::format::{DEEP_BLOCK_BITS, HEADER_LEN, Header, MAX_PLY, deep_bits, deep_bucket, part_bits, part_of};
use super::runs::{ENTRY_BYTES, Entry, Limits, MAX_GAME, Progress, Room, io, on_workers, opened, reserve};
use super::source::{Line, Source, Workspace};
use super::stream::{self, BATCH, Stream};
use super::tree::{self, Shallow};

/// What to build: records `first..=last` of the database at `generation`.
pub struct Plan {
    pub first: u32,
    pub last: u32,
    pub generation: u64,
}

/// Builds the index of `plan` into `target`, and its move stream beside it
/// ([`stream::path_of`]), each through a file of its own renamed at the end;
/// nothing else is written. A failed build removes both, and so does one
/// that panics.
pub fn build_with(
    source: &dyn Source,
    plan: &Plan,
    target: &Path,
    progress: &Progress,
    limits: &Limits,
) -> Result<Header, SearchError> {
    let paths = [indexdir::partial(target), indexdir::partial(&stream::path_of(target))];
    let mut partials = Partials { paths, renamed: false };
    let header = build_in(source, plan, target, progress, limits)?;
    partials.renamed = true;
    Ok(header)
}

/// A build's two files, removed when it drops unless they were renamed into
/// place: a build that fails, or panics (#172), leaves neither behind. Its
/// files are closed by then, the build's frames having dropped them first.
struct Partials {
    paths: [PathBuf; 2],
    renamed: bool,
}

impl Drop for Partials {
    fn drop(&mut self) {
        if !self.renamed {
            for path in &self.paths {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

fn build_in(
    source: &dyn Source,
    plan: &Plan,
    target: &Path,
    progress: &Progress,
    limits: &Limits,
) -> Result<Header, SearchError> {
    if plan.last > MAX_GAME {
        return Err(SearchError::TooLarge);
    }
    let (partial, moves) = (indexdir::partial(target), stream::path_of(target));
    let moves_partial = indexdir::partial(&moves);
    let (part_bits, bits) = (part_bits(plan.last), deep_bits(plan.last));
    let started = Instant::now();
    let anchor = ranking_anchor(source, plan, progress)?;
    let mut writer = stream::Writer::create(&moves_partial, plan.first, plan.last)?;
    writer.anchor = anchor;
    let counted = read_games(source, plan, &writer, part_bits, bits, progress, limits)?;
    let build_id = stream::build_id();
    writer.finish(plan.generation, build_id)?;
    // The build reads back the stream it has written, mapped, from the
    // operating system's file cache, which holds it outside the budget.
    let stream = opened(progress, || Stream::open(&moves_partial)).map_err(|e| from_bad(&moves_partial, e))?;
    progress.time(|t| t.reading = started.elapsed());
    let share = limits.share.checked_sub(counted.bytes).ok_or(SearchError::TooLarge)?;
    let mut out = Out::create(&partial)?;
    let tree = tree::write(&stream, &counted, part_bits, &mut out, progress, share, limits)?;
    let deep_offset = out.offset;
    let deep = deep::write(&stream, &counted.postings, bits, &mut out, progress, share, limits)?;
    let header = Header {
        max_ply: MAX_PLY,
        prune_ply: MAX_PLY,
        first_record: plan.first,
        last_record: plan.last,
        generation: plan.generation,
        games: stream.header.games,
        keys: tree.keys,
        blocks: tree.blocks,
        table_offset: tree.table_offset,
        table_crc: tree.table_crc,
        file_len: out.offset,
        deep_bits: bits,
        deep_postings: deep.postings,
        deep_offset,
        deep_table_offset: deep.table_offset,
        deep_table_crc: deep.table_crc,
        build_id,
    };
    let started = Instant::now();
    out.finish(&header)?;
    drop(stream);
    progress.time(|t| t.closing = started.elapsed());
    // The stream, then the index: a stop between the two leaves files of
    // different builds, which are rebuilt. On Windows an old file may be
    // mapped by an answer still in flight, which each rename waits for.
    let started = Instant::now();
    indexdir::replace(&moves_partial, &moves).map_err(|e| io(&moves, e))?;
    indexdir::replace(&partial, target).map_err(|e| io(target, e))?;
    progress.time(|t| t.renaming = started.elapsed());
    Ok(header)
}

/// The ranking anchor is found once, before any shallow entries are folded.
/// Batches reuse one budgeted workspace and observe build cancellation.
fn ranking_anchor(source: &dyn Source, plan: &Plan, progress: &Progress) -> Result<u32, SearchError> {
    let _memory = reserve(Workspace::BYTES, progress)?;
    let mut work = Workspace::new().ok_or(Refused::Busy)?;
    progress.start("checking", u64::from(plan.last));
    let mut newest = 0;
    for first in (plan.first..=plan.last).step_by(super::source::RECORDS) {
        if progress.stopped() {
            return Err(SearchError::Superseded);
        }
        progress.give_way();
        let last = first.saturating_add(super::source::RECORDS as u32 - 1).min(plan.last);
        newest = newest.max(source.newest_date(first, last, &mut work)?);
        progress.done.store(u64::from(last), Ordering::Relaxed);
    }
    Ok(newest)
}

/// A failure of the file at `path`, which the build wrote, as a build's
/// failure: a file that does not hold what the build wrote there is a bug
/// (#180), not the database's fault.
pub(super) fn from_bad(path: &Path, e: Bad) -> SearchError {
    match e {
        Bad::Io(e) => io(path, e),
        Bad::Corrupt(what) => SearchError::Bug(what),
        Bad::Busy => SearchError::Busy,
    }
}

/// What the stream pass counted: the tree's entries in each part of the
/// keys, those of them it folded ([`tree::SHALLOW_PLY`]), and the deep
/// section's postings in each of its blocks; and the entries it folded,
/// sorted. All held in the budget until the build ends.
pub(super) struct Counted {
    pub entries: Vec<u64>,
    pub shallow: Vec<u64>,
    pub folded: Vec<Entry>,
    pub postings: Vec<u64>,
    bytes: usize,
    _memory: Hold,
    _folded: Hold,
}

/// Games of a batch between two looks at foreground work (#149). A batch
/// of the stream pass runs for milliseconds on every worker, which a cold
/// search starting meanwhile would otherwise share whole; within it a
/// background build gives way every so many games as well.
const GIVE_WAY_EVERY: u32 = 256;

/// The stream pass: reads records `plan.first..=plan.last` on at most half
/// the shared workers, so that searches keep the rest, a block of the stream
/// at a time, and writes each game's line to `writer`; counts what each line
/// adds to the tree, in parts of `part_bits` bits, and to the deep section,
/// of buckets of `bits` bits; and folds the entries of the positions that
/// games from the standard start reach first within [`tree::SHALLOW_PLY`]
/// plies, unless they do not fold into the room a worker has for them, when
/// the tree's passes collect them as they do the others.
fn read_games(
    source: &dyn Source,
    plan: &Plan,
    writer: &stream::Writer,
    part_bits: u8,
    bits: u8,
    progress: &Progress,
    limits: &Limits,
) -> Result<Counted, SearchError> {
    let (parts, blocks) = (1usize << part_bits, 1usize << (bits - DEEP_BLOCK_BITS));
    let bytes = 8 * (2 * parts + blocks);
    let memory = reserve(bytes, progress)?;
    let mut counted = Counted {
        entries: vec![0; parts],
        shallow: vec![0; parts],
        folded: Vec::new(),
        postings: vec![0; blocks],
        bytes,
        _memory: memory,
        _folded: Hold::default(),
    };
    let total = (u64::from(plan.last) + 1).saturating_sub(u64::from(plan.first));
    progress.start("reading", total);
    if total == 0 {
        return Ok(counted);
    }
    let batches = total.div_ceil(BATCH as u64);
    // Half the workers at most, and no more than the share holds with their
    // read buffers, their part of the stream and their counts; then each a
    // room for its folded entries from what the share has beside them, no
    // larger than the entries all the games add, of a few positions each.
    let reading = Workspace::BYTES + stream::WORKER_BYTES + bytes;
    let fit = limits.share.saturating_sub(bytes) / reading;
    if fit == 0 {
        return Err(SearchError::TooLarge);
    }
    let want = threads().div_ceil(2).min(batches as usize).min(fit).max(1);
    let spare = limits.share.saturating_sub(bytes + want * reading) / want;
    let room = (spare / ENTRY_BYTES).saturating_sub(tree::FOLD_ENTRIES).min(tree::SHALLOW_ENTRIES);
    let added = usize::try_from(total * (u64::from(tree::SHALLOW_PLY) + 1)).unwrap_or(usize::MAX);
    let room = room.min(added.max(tree::MIN_SHALLOW_ENTRIES));
    let room = room.min(limits.pass_bytes.unwrap_or(usize::MAX) / ENTRY_BYTES);
    let next = AtomicU64::new(0);
    // Set once a worker's folded entries no longer fit its room, or when it
    // has none.
    let unfolded = AtomicBool::new(false);
    let pass = StreamPass {
        source,
        plan,
        writer,
        part_bits,
        bits,
        parts,
        blocks,
        batches,
        next: &next,
        reading,
        room,
        unfolded: &unfolded,
        progress,
    };
    let found = on_workers(want, progress, limits, |w| pass.read(w))?;
    let mut folded = Vec::new();
    folded.try_reserve_exact(found.len()).map_err(|_| Refused::Busy)?;
    for (entries, shallow_entries, postings, shallow) in found {
        for (all, one) in counted.entries.iter_mut().zip(entries) {
            *all += one;
        }
        for (all, one) in counted.shallow.iter_mut().zip(shallow_entries) {
            *all += one;
        }
        for (all, one) in counted.postings.iter_mut().zip(postings) {
            *all += one;
        }
        folded.push(shallow);
    }
    let merged = if unfolded.load(Ordering::Relaxed) { None } else { Shallow::merge(folded, progress)? };
    match merged {
        Some((entries, hold)) => {
            counted.bytes += hold.bytes();
            (counted.folded, counted._folded) = (entries, hold);
        }
        None => counted.shallow.iter_mut().for_each(|n| *n = 0),
    }
    Ok(counted)
}

/// The stream pass as its workers share it ([`StreamPass::read`]): the
/// records to read and the stream their lines go to, the parts of the keys
/// and the deep section's blocks they are counted in, the batches and the
/// next one to take, what a worker holds to read them and the room for its
/// folded entries, and whether a worker's folded entries no longer fit.
struct StreamPass<'a> {
    source: &'a dyn Source,
    plan: &'a Plan,
    writer: &'a stream::Writer,
    part_bits: u8,
    bits: u8,
    parts: usize,
    blocks: usize,
    batches: u64,
    next: &'a AtomicU64,
    reading: usize,
    room: usize,
    unfolded: &'a AtomicBool,
    progress: &'a Progress,
}

/// What a worker of the stream pass counted: the tree's entries by part,
/// those of them it folded, and the deep section's postings by block; and
/// its folded entries, with the hold of their room.
type Tally = (Vec<u64>, Vec<u64>, Vec<u64>, (Shallow, Hold));

impl StreamPass<'_> {
    /// Worker `w`'s share of the stream pass: whole blocks of the stream, a
    /// batch at a time, each game's line written to its part of the stream
    /// and counted.
    fn read(&self, w: &Worker<'_>) -> Result<Tally, SearchError> {
        let StreamPass {
            source,
            plan,
            writer,
            part_bits,
            bits,
            parts,
            blocks,
            batches,
            next,
            reading,
            room,
            unfolded,
            progress,
        } = *self;
        let mut hold = reserve(reading, progress)?;
        // As much of the room as the budget has free now, by halves: a build
        // yields to searches, and does without the folded entries when a
        // worker has too little room for them.
        let mut room = room;
        while room >= tree::MIN_SHALLOW_ENTRIES && hold.grow_quietly(Shallow::bytes(room)).is_err() {
            room /= 2;
        }
        if room < tree::MIN_SHALLOW_ENTRIES {
            room = 0;
            unfolded.store(true, Ordering::Relaxed);
        }
        let mut work = Workspace::new().ok_or(Refused::Busy)?;
        work.keep_words().ok_or(Refused::Busy)?;
        let mut part = writer.part().ok_or(Refused::Busy)?;
        let zeros = |n: usize| {
            let mut v: Vec<u64> = Vec::new();
            v.try_reserve_exact(n).ok()?;
            v.resize(n, 0);
            Some(v)
        };
        let mut entries = zeros(parts).ok_or(Refused::Busy)?;
        let mut shallow_entries = zeros(parts).ok_or(Refused::Busy)?;
        let mut postings = zeros(blocks).ok_or(Refused::Busy)?;
        let mut shallow = Shallow::new(room).ok_or(Refused::Busy)?;
        let mut count = |line: &Line| {
            let standard = line.setup.is_none();
            for &(key, mv, ply) in &line.positions {
                let at = part_of(key, part_bits);
                entries[at] += 1;
                if standard && ply <= tree::SHALLOW_PLY && !unfolded.load(Ordering::Relaxed) {
                    shallow_entries[at] += 1;
                    if !shallow
                        .add(Entry::new(key, line.number, line.outcome, mv, line.elo).with_featured(writer.rank(line)))
                    {
                        unfolded.store(true, Ordering::Relaxed);
                    }
                }
            }
            for &s in &line.structures {
                postings[(deep_bucket(s, bits) >> DEEP_BLOCK_BITS) as usize] += 1;
            }
        };
        loop {
            // A background build gives way to foreground work before it
            // takes its next batch, so that none waits for it (#149).
            progress.give_way();
            // Whole blocks of the stream, so that each block is one worker's.
            let batch = next.fetch_add(1, Ordering::Relaxed);
            if batch >= batches {
                break;
            }
            if w.stopped() || progress.stopped() {
                return Err(SearchError::Superseded);
            }
            let lo = u64::from(plan.first) + batch * BATCH as u64;
            let hi = (lo + BATCH as u64 - 1).min(u64::from(plan.last));
            part.begin(lo as u32, hi as u32);
            let mut failed = None;
            let mut since = 0;
            source.lines(lo as u32, hi as u32, MAX_PLY, &mut work, &mut |line: &Line| {
                if failed.is_none() {
                    count(line);
                    failed = part.add(line).err();
                }
                since += 1;
                if since == GIVE_WAY_EVERY {
                    since = 0;
                    progress.give_way();
                }
            })?;
            match failed {
                Some(e) => return Err(e),
                None => part.end()?,
            }
            progress.done.fetch_add(hi - lo + 1, Ordering::Relaxed);
        }
        progress.skipped.fetch_add(work.skipped, Ordering::Relaxed);
        drop((work, part));
        // The worker's folded entries keep their room's hold until merged.
        hold.shrink(if room > 0 { Shallow::bytes(room) } else { 0 });
        Ok((entries, shallow_entries, postings, (shallow, hold)))
    }
}

/// The share of a pass's room, in percent, that its plan fills.
const PLANNED: usize = 97;

/// The sizes of a section's passes, the tree's or the deep section's, which
/// [`Passes::room`] plans within the build's share of the budget: what they
/// hold beside the entries or postings they collect, and those. A section's
/// passes take its units in order: the tree's parts of the keys, the deep
/// section's blocks.
pub(super) struct Passes {
    /// The section's table, which the passes hold from the start.
    pub table: usize,
    /// What a worker holds besides its buffer.
    pub worker: usize,
    /// The bytes of an entry or a posting, and those the stream pass counted.
    pub item: usize,
    pub items: u64,
    /// The fewest entries or postings a worker's buffer holds.
    pub least: usize,
    /// The room of a worker whose pass starts at the largest unit, which the
    /// passes take whenever the share holds it; a worker's least room at
    /// least, which is all that a section whose passes end inside a unit
    /// needs.
    pub largest: usize,
}

impl Passes {
    /// What the passes reserve ([`Room::reserve`]) of the build's `share` of
    /// the budget, for a database of `records` records, with `threads`
    /// shared workers and at most `pass_bytes` of entries or postings in a
    /// pass ([`Limits::pass_bytes`]). Half the workers at most, and one for
    /// every 64 records at most: as many as the share holds beside a quarter
    /// of it for the buffers and beside the largest unit's room, one at
    /// least. The buffers take the rest of the share, but no more than the
    /// entries or postings take, twice over, and that room: a small
    /// database's build holds a little of the budget, however large its
    /// share, and takes one pass. `TooLarge` when that is less than a
    /// worker's least room.
    pub fn room(
        &self,
        share: usize,
        threads: usize,
        records: u64,
        pass_bytes: Option<usize>,
    ) -> Result<Room, SearchError> {
        let least = self.least * self.item;
        let largest = self.largest.max(least);
        let rest = share.saturating_sub(self.table);
        let fit = ((rest / 4 * 3).min(rest.saturating_sub(largest)) / self.worker).max(1);
        let workers = threads.div_ceil(2).min(records.div_ceil(64) as usize).min(fit).max(1);
        let room = share.checked_sub(self.table + workers * self.worker).ok_or(SearchError::TooLarge)?;
        let usable =
            usize::try_from(self.items).unwrap_or(usize::MAX).saturating_mul(2 * self.item).saturating_add(largest);
        let room = room.min(usable).min(pass_bytes.unwrap_or(usize::MAX));
        if room < least {
            return Err(SearchError::TooLarge);
        }
        // The table's first room is reserved with the passes', so that the
        // buffers never take the room the table then waits for. While
        // searches hold the budget, the passes take less room, but never
        // less than the largest unit's when the share holds it: the build
        // waits for that rather than start a pass that a unit alone would
        // fill.
        Ok(Room { fixed: self.table, each: self.worker, workers, least: largest.clamp(least, room), room })
    }

    /// The entries or postings that the buffers hold together in the `room`
    /// reserved, and the workers of the `workers` reserved that each hold a
    /// worker's least of them.
    pub fn capacity(&self, room: usize, workers: usize) -> (usize, usize) {
        let capacity = room / self.item;
        (capacity, workers.min(capacity / self.least))
    }
}

/// The units of a pass from `first` on, of a section's `units`, each of
/// `count(unit)` entries or postings as the stream pass counted them: as many
/// as `capacity` of them hold, but for a little ([`PLANNED`]), one at least.
/// The unit after them, and the entries or postings planned.
pub(super) fn plan_pass(first: usize, units: usize, capacity: usize, count: impl Fn(usize) -> u64) -> (usize, u64) {
    let (mut end, mut planned) = (first, 0);
    while end < units && (end == first || planned + count(end) <= (capacity * PLANNED / 100) as u64) {
        planned += count(end);
        end += 1;
    }
    (end, planned)
}

/// The games of a pass, handed to its workers a few at a time as each comes
/// free.
pub(super) struct Chunks {
    next: AtomicU64,
    last: u64,
    size: u64,
    /// The records of the pass.
    records: u64,
    /// The workers still taking them: all of the pass's workers from the
    /// first one's start, so that one that starts late counts from the
    /// beginning (#237).
    taking: AtomicU32,
}

impl Chunks {
    /// Records `first..=last` for `workers` workers: about sixteen chunks a
    /// worker, of 16 to 1,024 records.
    pub fn new(first: u32, last: u32, workers: usize) -> Chunks {
        let records = (u64::from(last) + 1).saturating_sub(u64::from(first));
        let size = records.div_ceil(16 * workers.max(1) as u64).clamp(16, 1024);
        let next = AtomicU64::new(u64::from(first));
        Chunks { next, last: u64::from(last), size, records, taking: AtomicU32::new(0) }
    }

    /// The next records to take, first and last; `None` once all are taken.
    pub fn take(&self) -> Option<(u32, u32)> {
        let lo = self.next.fetch_add(self.size, Ordering::Relaxed);
        (lo <= self.last).then(|| (lo as u32, (lo + self.size - 1).min(self.last) as u32))
    }

    /// The room a worker's buffer keeps free, in a pass that plans `planned`
    /// entries or postings: twice what a chunk adds, about. A worker whose
    /// buffer has less left takes no more chunks while another one does
    /// ([`Taker`]).
    pub fn spare(&self, planned: u64) -> usize {
        (2 * planned * self.size).div_ceil(self.records.max(1)) as usize
    }

    /// One of the pass's `workers` workers, taking chunks. The first one to
    /// start counts them all: a worker whose buffer is full then leaves the
    /// chunks to the ones that have not started yet, rather than finding
    /// itself the only one taking them and going past full, which ended the
    /// pass early for all and made more passes the later the others started
    /// (#237). None of them is left out: a worker that fails before it takes
    /// a chunk stops the whole pass.
    pub fn taker(&self, workers: usize) -> Taker<'_> {
        let workers = u32::try_from(workers.max(1)).unwrap_or(u32::MAX);
        let _ = self.taking.compare_exchange(0, workers, Ordering::AcqRel, Ordering::Relaxed);
        Taker { chunks: self, last: false }
    }
}

/// A worker taking a pass's chunks. A pass plans nearly all its room, and a
/// worker whose buffer is full stops taking chunks, unless it is the last one
/// still taking them: every chunk is taken, and the buffers fill evenly
/// whatever the workers' pace, the last worker's making room for what does
/// not fit.
pub(super) struct Taker<'a> {
    chunks: &'a Chunks,
    /// Whether it found itself the last one taking them.
    last: bool,
}

impl Taker<'_> {
    /// The next records to take; `None` once all are taken, or once the
    /// worker's buffer is `full` while another worker still takes them.
    pub fn take(&mut self, full: bool) -> Option<(u32, u32)> {
        if full && !self.last {
            // Leaves them to the others while another takes them; the count
            // never drops to 0, so no taker starts the count again.
            let others =
                self.chunks.taking.fetch_update(Ordering::AcqRel, Ordering::Acquire, |t| (t > 1).then(|| t - 1));
            if others.is_ok() {
                return None;
            }
            self.last = true;
        }
        self.chunks.take()
    }
}

/// The index file being written: each part's bytes at their place, which
/// the parts are given in order ([`Turns`]), the header's room first, filled
/// in last. Bytes are written positionally, so that workers write what they
/// made at the same time, each at its place. What each pass wrote is synced
/// behind the build, on a thread of its own, so that the sync that ends the
/// build waits for the last bytes alone.
pub(super) struct Out {
    file: File,
    path: PathBuf,
    /// Where the next bytes go: the end of what is placed.
    pub offset: u64,
    behind: Option<Behind>,
}

/// The thread that syncs the index file behind the build, and how it is
/// asked to.
struct Behind {
    ask: mpsc::SyncSender<()>,
    thread: std::thread::JoinHandle<()>,
}

impl Out {
    fn create(path: &Path) -> Result<Out, SearchError> {
        let file = File::create(path).map_err(|e| io(path, e))?;
        // Without a second handle or a thread, the file is synced at the end
        // alone. The thread runs at the build's priority.
        let priority = crate::machine::current();
        let behind = file.try_clone().ok().and_then(|synced| {
            let (ask, asked) = mpsc::sync_channel::<()>(1);
            let thread = std::thread::Builder::new()
                .name("bridge-index-sync".into())
                .stack_size(crate::THREAD_STACK)
                .spawn(move || {
                    crate::machine::follow(priority);
                    for () in asked {
                        // The sync that ends the build reports a failure.
                        let _ = synced.sync_data();
                    }
                })
                .ok()?;
            Some(Behind { ask, thread })
        });
        let mut out = Out { file, path: path.to_path_buf(), offset: 0, behind };
        out.put(&[0u8; HEADER_LEN])?;
        Ok(out)
    }

    /// Has what is written so far synced behind the build: at once when no
    /// sync is running, else once the running one ends, which a request made
    /// meanwhile joins.
    pub fn sync_behind(&self) {
        if let Some(b) = &self.behind {
            let _ = b.ask.try_send(());
        }
    }

    /// Waits for the syncs behind the build to end.
    fn catch_up(&mut self) {
        if let Some(Behind { ask, thread }) = self.behind.take() {
            drop(ask);
            let _ = thread.join();
        }
    }

    /// Writes `bytes` at `at`, where they were placed.
    pub fn write(&self, at: u64, bytes: &[u8]) -> Result<(), SearchError> {
        write_at(&self.file, at, bytes).map_err(|e| io(&self.path, e))
    }

    /// Writes `bytes` after what is placed.
    pub fn put(&mut self, bytes: &[u8]) -> Result<(), SearchError> {
        self.write(self.offset, bytes)?;
        self.offset += bytes.len() as u64;
        Ok(())
    }

    /// Writes `header` in its room and syncs the file.
    fn finish(mut self, header: &Header) -> Result<(), SearchError> {
        self.catch_up();
        self.write(0, &header.encode())?;
        self.file.sync_all().map_err(|e| io(&self.path, e))
    }
}

/// A failed build's file is removed once no thread holds it.
impl Drop for Out {
    fn drop(&mut self) {
        self.catch_up();
    }
}

/// Units of work made on the workers in any order and placed in theirs, so
/// that a file holds them in order whatever the workers' progress. Each
/// worker takes the next unit as it comes free, makes its bytes and hands
/// them over: they are placed at once when every unit before theirs is, else
/// kept until then, and the worker goes on with its next unit, so that no
/// worker waits for another; whoever places a unit places the kept ones that
/// follow it. A piece is placed under the lock, then written at its place by
/// the worker that placed it, the lock released: workers hand over pieces
/// and write at the same time, one piece each at a time, held in the room
/// it keeps for what it makes. What is kept stays within `room` bytes: a
/// worker whose bytes do not fit waits until they do, or until its unit's
/// turn has come. A unit too large for one hand-over is handed over in
/// pieces, in order, the last one ending it.
pub(super) struct Turns<'a, T, M, W> {
    taken: AtomicU32,
    units: u32,
    room: usize,
    state: Mutex<Queue<T, M>>,
    written: Condvar,
    place: Place<'a, T, M, W>,
    write: WritePlaced<'a, W>,
}

/// The units placed and kept.
struct Queue<T, M> {
    /// The next unit to place.
    next: u32,
    sink: T,
    /// The pieces made ahead of their turn, by unit, in order: each with its
    /// bytes and whether it ends its unit.
    kept: BTreeMap<u32, VecDeque<(M, usize, bool)>>,
    /// The bytes kept.
    bytes: usize,
}

/// Places a piece of a unit in the sink, in the units' order: what to write
/// where.
pub(super) type Place<'a, T, M, W> = &'a (dyn Fn(&mut T, M) -> Result<W, SearchError> + Sync);

/// Writes a piece placed.
pub(super) type WritePlaced<'a, W> = &'a (dyn Fn(W) -> Result<(), SearchError> + Sync);

impl<'a, T, M, W> Turns<'a, T, M, W> {
    /// `units` units, kept within `room` bytes, each piece placed in `sink`
    /// with `place` and written with `write`.
    pub fn new(
        units: usize,
        room: usize,
        sink: T,
        place: Place<'a, T, M, W>,
        write: WritePlaced<'a, W>,
    ) -> Turns<'a, T, M, W> {
        let units = u32::try_from(units).unwrap_or(u32::MAX);
        let queue = Queue { next: 0, sink, kept: BTreeMap::new(), bytes: 0 };
        let state = Mutex::new(queue);
        Turns { taken: AtomicU32::new(0), units, room, state, written: Condvar::new(), place, write }
    }

    /// The next unit to make, `None` once all are taken.
    pub fn take(&self) -> Option<usize> {
        let unit = self.taken.fetch_add(1, Ordering::Relaxed);
        (unit < self.units).then_some(unit as usize)
    }

    /// Hands over `made`, which holds `bytes` of memory, a piece of `unit`,
    /// the last one when `last`, to be placed and written. A worker waiting
    /// for room stops, `Superseded`, once `stopped`, as every worker does
    /// after another one failed.
    pub fn put(
        &self,
        unit: usize,
        made: M,
        bytes: usize,
        last: bool,
        stopped: &dyn Fn() -> bool,
    ) -> Result<(), SearchError> {
        let unit = unit as u32;
        let mut q = lock(&self.state);
        // A piece fits when nothing is kept, however large.
        while q.next != unit && q.bytes > 0 && q.bytes + bytes > self.room {
            if stopped() {
                return Err(SearchError::Superseded);
            }
            q = unpoisoned(self.written.wait_timeout(q, Duration::from_millis(50))).0;
        }
        q.bytes += bytes;
        q.kept.entry(unit).or_default().push_back((made, bytes, last));
        // Every piece whose turn has come, in order: this one and the ones
        // kept after it, as far as they are made; another worker may place
        // the next ones while this one writes.
        loop {
            let next = q.next;
            let Some(pieces) = q.kept.get_mut(&next) else { break };
            let Some((made, bytes, last)) = pieces.pop_front() else { break };
            if pieces.is_empty() {
                q.kept.remove(&next);
            }
            if last {
                q.next += 1;
            }
            q.bytes -= bytes;
            let placed = (self.place)(&mut q.sink, made)?;
            drop(q);
            self.written.notify_all();
            (self.write)(placed)?;
            q = lock(&self.state);
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::search::memory::Cancel;
    use crate::search::workers;

    /// Units made on many workers, each taking its time, are placed in
    /// their order, some in pieces among them, and each piece written once,
    /// and no more than the room is ever kept; a worker waiting for room
    /// stops once told to.
    #[test]
    fn units_are_written_in_order_whatever_the_workers_progress() {
        let (most, written) = (std::sync::atomic::AtomicUsize::new(0), Mutex::new(Vec::new()));
        let place = |out: &mut (Vec<usize>, usize), (unit, _): (usize, usize)| {
            out.0.push(unit);
            Ok(unit)
        };
        // Writes that take their time, on several workers at once.
        let write = |unit: usize| {
            std::thread::sleep(Duration::from_micros(unit as u64 % 3 * 50));
            written.lock().unwrap().push(unit);
            Ok(())
        };
        let turns = Turns::new(500, 30, (Vec::new(), 0), &place, &write);
        std::thread::scope(|s| {
            for w in 0..8u64 {
                let (turns, most) = (&turns, &most);
                s.spawn(move || {
                    while let Some(unit) = turns.take() {
                        std::thread::sleep(Duration::from_micros((unit as u64 * 7919 + w) % 200));
                        // Every tenth unit in three pieces.
                        let pieces = if unit % 10 == 0 { 3 } else { 1 };
                        for p in 0..pieces {
                            turns.put(unit, (unit, p), 10, p + 1 == pieces, &|| false).unwrap();
                            let kept = turns.state.lock().unwrap().bytes;
                            most.fetch_max(kept, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        let q = turns.state.into_inner().unwrap();
        let want: Vec<usize> = (0..500).flat_map(|u| std::iter::repeat_n(u, if u % 10 == 0 { 3 } else { 1 })).collect();
        assert_eq!(q.sink.0, want);
        let mut written = written.into_inner().unwrap();
        written.sort_unstable();
        assert_eq!(written, want, "each piece written once");
        assert_eq!((q.next, q.bytes, q.kept.len()), (500, 0, 0));
        assert!(most.into_inner() <= 30, "kept within the room");
        // Unit 1 kept, unit 2 waits for room that unit 0, never handed over,
        // would free: it stops once told to.
        let place = |out: &mut Vec<usize>, unit: usize| {
            out.push(unit);
            Ok(())
        };
        let stuck = Turns::new(3, 10, Vec::new(), &place, &|()| Ok(()));
        stuck.put(1, 1, 10, true, &|| false).unwrap();
        assert!(matches!(stuck.put(2, 2, 10, true, &|| true), Err(SearchError::Superseded)));
        stuck.put(0, 0, 10, true, &|| false).unwrap();
        assert_eq!(stuck.state.into_inner().unwrap().sink, [0, 1]);
    }

    /// Whether this is the child that runs the test's body. The parent runs
    /// the test `name` of this binary in a child process with `workers`
    /// workers, which [`threads`] reads once a process, whatever the
    /// computer's processors, and checks it passed within `limit`: a child
    /// still running then is killed, and the test fails. Each test's child
    /// writes a log of its own, as the tests run beside each other.
    pub(crate) fn in_child(name: &str, workers: usize, limit: Duration) -> bool {
        const CHILD: &str = "BRIDGE_BUILD_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            return true;
        }
        let test = name.rsplit("::").next().unwrap_or(name);
        let log = std::env::temp_dir().join(format!("bridge-build-child-{}-{test}.log", std::process::id()));
        let file = File::create(&log).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([name, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env("OSCHESS_BRIDGE_THREADS", workers.to_string())
            .stdout(file.try_clone().unwrap())
            .stderr(file)
            .spawn()
            .unwrap();
        let until = Instant::now() + limit;
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= until {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(&log);
        let Some(status) = status else { panic!("{name} did not end within {limit:?}:\n{text}") };
        assert!(status.success() && text.contains("1 passed"), "{text}");
        false
    }

    /// A worker that panics before it hands over its unit stops the others
    /// (#172): one waiting for the room that unit would free stops, rather
    /// than waiting for ever, and the panic reaches the pass's caller. The
    /// two workers it takes run in a child process, which has them however
    /// many the computer or `OSCHESS_BRIDGE_THREADS` gives this one.
    #[test]
    fn a_worker_that_panics_stops_one_waiting_for_its_turn() {
        let name = "explorer::build::tests::a_worker_that_panics_stops_one_waiting_for_its_turn";
        if !in_child(name, 2, Duration::from_secs(60)) {
            return;
        }
        assert_eq!(threads(), 2);
        let place = |out: &mut Vec<usize>, unit: usize| {
            out.push(unit);
            Ok(())
        };
        // Room for one byte: unit 1 is kept, and unit 2 waits for unit 0.
        let turns = Turns::new(3, 1, Vec::new(), &place, &|()| Ok(()));
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            workers::run(2, 0, &Cancel::never(), |w| {
                if w.count < 2 {
                    return Err(SearchError::Busy);
                }
                if w.index == 1 {
                    turns.put(1, 1, 1, true, &|| w.stopped())?;
                    return turns.put(2, 2, 1, true, &|| w.stopped());
                }
                while lock(&turns.state).bytes == 0 {
                    std::thread::sleep(Duration::from_millis(1));
                }
                panic!("a bug in the worker making unit 0");
            })
        }));
        assert!(ran.is_err(), "the panic reached the caller: {:?}", ran.ok());
    }

    /// Three records, which no build here reads.
    struct Unread;

    impl Source for Unread {
        fn records(&self) -> u32 {
            3
        }

        fn lines(&self, _: u32, _: u32, _: u8, _: &mut Workspace, _: &mut dyn FnMut(&Line)) -> cbformat::Result<()> {
            unreachable!("no worker was free to read them")
        }
    }

    /// A build whose pass waits for a worker while a search holds every one
    /// stops once it is asked to, `Superseded`, rather than waiting on until
    /// it gives up, and leaves no file (#180). The one worker runs in a child
    /// process, which has it however many the computer or
    /// `OSCHESS_BRIDGE_THREADS` gives this one.
    #[test]
    fn a_build_waiting_for_a_worker_stops_when_asked() {
        let name = "explorer::build::tests::a_build_waiting_for_a_worker_stops_when_asked";
        if !in_child(name, 1, Duration::from_secs(120)) {
            return;
        }
        assert_eq!(threads(), 1);
        let dir = std::env::temp_dir().join(format!("bridge-build-waiting-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (target, progress, limits) = (dir.join("db.idx"), Progress::default(), Limits::default());
        let plan = Plan { first: 1, last: 3, generation: 7 };
        let search = workers::one(&Cancel::never()).unwrap();
        let (built, after) = std::thread::scope(|s| {
            let build = s.spawn(|| build_with(&Unread, &plan, &target, &progress, &limits));
            // Its stream pass starts reading, which takes a worker first.
            let until = Instant::now() + workers::tests::PATIENCE;
            while progress.phase() != "reading" {
                assert!(Instant::now() < until, "the build starts reading");
                std::thread::sleep(Duration::from_millis(1));
            }
            let asked = Instant::now();
            progress.ask_stop();
            let built = build.join().unwrap();
            (built, asked.elapsed())
        });
        assert!(matches!(built, Err(SearchError::Superseded)), "{:?}", built.err());
        assert!(after < limits.workers_wait / 2, "it stopped {after:?} after it was asked");
        drop(search);
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none(), "no file is left");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A section's passes take half the workers at most, one for every 64
    /// records at most, and as many as the share holds beside a quarter of it
    /// for the buffers and beside the largest unit's room; the buffers take
    /// the rest, but no more than the entries take twice over and that room,
    /// nor than a pass may; a share without room for one worker and its least
    /// is too small.
    #[test]
    fn passes_plan_their_room_within_the_share() {
        let mb = 1 << 20;
        let passes = Passes { table: mb, worker: 4 * mb, item: 16, items: 1 << 30, least: 64, largest: 0 };
        let plan = |passes: &Passes, share: usize, records: u64, pass_bytes: Option<usize>| {
            let room = passes.room(share, 32, records, pass_bytes).unwrap();
            assert_eq!((room.fixed, room.each), (mb, 4 * mb));
            (room.workers, room.least, room.room)
        };
        assert_eq!(plan(&passes, 1 << 34, 10_000_000, None), (16, 1024, (1 << 34) - 65 * mb));
        assert_eq!(plan(&passes, 1 << 34, 200, None).0, 4, "a worker for every 64 records");
        // Three workers fit beside a quarter of 16 MiB, two beside a largest
        // unit's 8 MiB, which the passes then take whatever searches hold.
        assert_eq!(plan(&passes, 17 * mb, 10_000_000, None), (3, 1024, 4 * mb));
        let crowded = Passes { largest: 8 * mb, ..passes };
        assert_eq!(plan(&crowded, 17 * mb, 10_000_000, None), (2, 8 * mb, 8 * mb));
        let few = Passes { items: 1_000, ..passes };
        assert_eq!(plan(&few, 1 << 34, 10_000_000, None), (16, 1024, 1_000 * 32 + 1024));
        assert_eq!(plan(&passes, 1 << 34, 10_000_000, Some(4096)), (16, 1024, 4096));
        for share in [5 * mb + 1023, 5 * mb - 1, 0] {
            assert!(matches!(passes.room(share, 32, 10_000_000, None), Err(SearchError::TooLarge)), "{share}");
        }
        assert!(matches!(passes.room(1 << 34, 32, 10_000_000, Some(1023)), Err(SearchError::TooLarge)));
        // Each worker kept holds a worker's least of the entries.
        assert_eq!(passes.capacity(4 * mb, 3), (1 << 18, 3));
        assert_eq!(passes.capacity(2048, 3), (128, 2));
    }

    /// A pass plans the units that nearly fill its room, one at least,
    /// however large; and a worker's buffer keeps room for about twice what
    /// a chunk of the pass adds.
    #[test]
    fn a_pass_plans_its_units() {
        let counts = [5, 5, 100, 5, 0];
        let count = |unit: usize| counts[unit];
        assert_eq!(plan_pass(0, counts.len(), 12, count), (2, 10), "11 of 12 planned");
        assert_eq!(plan_pass(2, counts.len(), 12, count), (3, 100), "one at least");
        assert_eq!(plan_pass(3, counts.len(), 12, count), (5, 5));
        assert_eq!(plan_pass(3, counts.len(), 0, count), (4, 5));
        assert_eq!(Chunks::new(1, 1_000, 4).spare(500), 16);
        assert_eq!(Chunks::new(5, 4, 1).spare(500), 16_000, "no records");
    }

    /// A worker whose buffer is full leaves the chunks to the others while
    /// another takes them, and the last one taking them goes on, full or not.
    #[test]
    fn the_last_worker_taking_chunks_goes_on() {
        let chunks = Chunks::new(1, 100_000, 3);
        let mut takers: Vec<Taker<'_>> = (0..3).map(|_| chunks.taker(3)).collect();
        assert!(takers.iter_mut().all(|t| t.take(false).is_some()));
        assert!(takers[0].take(true).is_none() && takers[1].take(true).is_none());
        assert!(takers[2].take(true).is_some(), "the last one goes on");
        assert!(takers[2].take(true).is_some());
    }

    /// The pass's workers count as taking chunks from the first one's start:
    /// a worker that is full before the others start leaves the chunks to
    /// them, and the last of them to start goes on (#237). Counted as they
    /// started, the first one found itself alone, went past full, and ended
    /// the pass early for all.
    #[test]
    fn a_full_worker_leaves_the_chunks_to_workers_not_started_yet() {
        let chunks = Chunks::new(1, 100_000, 3);
        let mut first = chunks.taker(3);
        assert!(first.take(false).is_some());
        assert!(first.take(true).is_none(), "two have not started");
        let mut second = chunks.taker(3);
        assert!(second.take(true).is_none(), "one has not started");
        let mut third = chunks.taker(3);
        assert!(third.take(true).is_some(), "the last one goes on, full or not");
        let mut rest = 0;
        while third.take(true).is_some() {
            rest += 1;
        }
        assert!(rest > 0 && chunks.take().is_none(), "every chunk is taken");
    }

    /// Chunks cover the records once each, whatever the workers, up to the
    /// last record a game can have.
    #[test]
    fn chunks_cover_every_record_once() {
        for (first, last, workers) in
            [(1, 1, 8), (1, 2_000, 8), (1, 100_000, 3), (5, 4, 2), (MAX_GAME - 50, MAX_GAME, 4)]
        {
            let chunks = Chunks::new(first, last, workers);
            let mut next = first;
            while let Some((lo, hi)) = chunks.take() {
                assert_eq!(lo, next);
                assert!(hi >= lo && hi <= last);
                next = hi + 1;
            }
            assert_eq!(next, last.max(first - 1) + 1, "{first}..={last}");
        }
        let top = Chunks::new(u32::MAX - 5, u32::MAX, 1);
        assert_eq!(top.take(), Some((u32::MAX - 5, u32::MAX)));
        assert_eq!(top.take(), None);
        assert_eq!(top.take(), None);
    }
}
