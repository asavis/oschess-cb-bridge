//! A whole build (#147), in three kinds of pass, which write nothing but the
//! two files they build, `<id>.moves.partial` and `<id>.idx.partial`:
//!
//! 1. The stream pass reads the database's games once, writes their main
//!    lines as the move stream, and counts the tree's entries in each part of
//!    the keys and the deep section's postings in each of its blocks.
//! 2. The tree's passes replay each game's first positions from the stream,
//!    for as many parts of the keys at a time as the build's share of the
//!    budget holds, and write the tree in key order ([`super::tree`]).
//! 3. The deep section's passes replay each whole line, for as many buckets
//!    at a time, and write its blocks in order ([`super::deep`]).
//!
//! Both files carry one build id and are renamed into place at the end: the
//! stream first, then the index.

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use crate::search::SearchError;
use crate::search::memory::{Cancel, Refused};
use crate::search::workers::{self, threads};

use super::deep;
use super::file::Bad;
use super::format::{
    DEEP_BLOCK_BITS, HEADER_LEN, Header, MAX_PLY, PRUNE_PLY, deep_bits, deep_bucket, part_bits, part_of,
};
use super::runs::{Limits, MAX_GAME, Progress, io, reserve};
use super::source::{Line, Source, Workspace};
use super::stream::{self, BATCH, Stream};
use super::tree;

/// What to build: records `first..=last` of the database at `generation`.
pub struct Plan {
    pub first: u32,
    pub last: u32,
    pub generation: u64,
}

/// What the index file's writer buffers.
const OUT_BUFFER: usize = 1 << 20;

/// Builds the index of `plan` into `target`, and its move stream beside it
/// ([`stream::path_of`]), each through a file of its own renamed at the end;
/// nothing else is written. A failed build removes both.
pub fn build_with(
    source: &dyn Source,
    plan: &Plan,
    target: &Path,
    progress: &Progress,
    limits: &Limits,
) -> Result<Header, SearchError> {
    let (partial, moves) = (temporary(target), stream::path_of(target));
    let moves_partial = temporary(&moves);
    let result = build_in(source, plan, target, progress, limits);
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
        let _ = std::fs::remove_file(&moves_partial);
    }
    result
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
    let (partial, moves) = (temporary(target), stream::path_of(target));
    let moves_partial = temporary(&moves);
    let (part_bits, bits) = (part_bits(plan.last), deep_bits(plan.last));
    let writer = stream::Writer::create(&moves_partial, plan.first, plan.last)?;
    let counted = read_games(source, plan, &writer, part_bits, bits, progress, limits)?;
    let build_id = stream::build_id();
    writer.finish(plan.generation, build_id)?;
    // The build reads back the stream it has written, mapped, from the
    // operating system's file cache, which holds it outside the budget.
    let stream = Stream::open(&moves_partial).map_err(|e| from_bad(&moves_partial, e))?;
    let _out_memory = reserve(OUT_BUFFER, progress)?;
    let share = limits.share.checked_sub(counted.bytes + OUT_BUFFER).ok_or(SearchError::TooLarge)?;
    let mut out = Out::create(&partial)?;
    let tree = tree::write(&stream, &counted.entries, part_bits, &mut out, progress, share, limits)?;
    let deep_offset = out.offset;
    let deep = deep::write(&stream, &counted.postings, bits, &mut out, progress, share, limits)?;
    let header = Header {
        max_ply: MAX_PLY,
        prune_ply: PRUNE_PLY,
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
    out.finish(&header)?;
    drop(stream);
    // The stream, then the index: a stop between the two leaves files of
    // different builds, which are rebuilt. On Windows an old file may be
    // mapped by an answer still in flight, which each rename waits for.
    stream::replace(&moves_partial, &moves).map_err(|e| io(&moves, e))?;
    stream::replace(&partial, target).map_err(|e| io(target, e))?;
    Ok(header)
}

/// The file at `path` with `.partial` after its name.
fn temporary(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".partial");
    path.with_file_name(name)
}

/// A failure of the file at `path` as a build's failure.
pub(super) fn from_bad(path: &Path, e: Bad) -> SearchError {
    match e {
        Bad::Io(e) => io(path, e),
        Bad::Corrupt(what) => corrupt(path, what),
        Bad::Busy => SearchError::Busy,
    }
}

/// The file at `path` does not hold what it should.
pub(super) fn corrupt(path: &Path, what: &str) -> SearchError {
    io(path, std::io::Error::other(what.to_string()))
}

/// What the stream pass counted: the tree's entries in each part of the
/// keys, and the deep section's postings in each of its blocks, held in the
/// budget until the build ends.
struct Counted {
    entries: Vec<u64>,
    postings: Vec<u64>,
    bytes: usize,
    _memory: crate::search::memory::Hold,
}

/// The stream pass: reads records `plan.first..=plan.last` on at most half
/// the shared workers, so that searches keep the rest, a block of the stream
/// at a time, and writes each game's line to `writer`; counts what each line
/// adds to the tree, in parts of `part_bits` bits, and to the deep section,
/// of buckets of `bits` bits.
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
    let bytes = 8 * (parts + blocks);
    let memory = reserve(bytes, progress)?;
    let mut counted = Counted { entries: vec![0; parts], postings: vec![0; blocks], bytes, _memory: memory };
    let total = (u64::from(plan.last) + 1).saturating_sub(u64::from(plan.first));
    progress.start("reading", total);
    if total == 0 {
        return Ok(counted);
    }
    let batches = total.div_ceil(BATCH as u64);
    // Half the workers at most, and no more than the share holds with their
    // read buffers, their part of the stream and their counts.
    let per_worker = Workspace::BYTES + stream::WORKER_BYTES + bytes;
    let fit = limits.share.saturating_sub(bytes) / per_worker;
    if fit == 0 {
        return Err(SearchError::TooLarge);
    }
    let want = threads().div_ceil(2).min(batches as usize).min(fit).max(1);
    let next = AtomicU64::new(0);
    let found = workers::run(want, 0, &Cancel::never(), |w| {
        let hold = reserve(per_worker, progress)?;
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
        let mut postings = zeros(blocks).ok_or(Refused::Busy)?;
        let mut count = |line: &Line| {
            for &(key, _, _) in &line.positions {
                entries[part_of(key, part_bits)] += 1;
            }
            for &s in &line.structures {
                postings[(deep_bucket(s, bits) >> DEEP_BLOCK_BITS) as usize] += 1;
            }
        };
        loop {
            // Whole blocks of the stream, so that each block is one worker's.
            let batch = next.fetch_add(1, Ordering::Relaxed);
            if batch >= batches {
                break;
            }
            if w.stopped() || progress.stop.load(Ordering::Relaxed) {
                return Err(SearchError::Superseded);
            }
            let lo = u64::from(plan.first) + batch * BATCH as u64;
            let hi = (lo + BATCH as u64 - 1).min(u64::from(plan.last));
            part.begin(lo as u32, hi as u32);
            let mut failed = None;
            source.lines(lo as u32, hi as u32, MAX_PLY, &mut work, &mut |line: &Line| {
                if failed.is_none() {
                    count(line);
                    failed = part.add(line).err();
                }
            })?;
            match failed {
                Some(e) => return Err(e),
                None => part.end()?,
            }
            progress.done.fetch_add(hi - lo + 1, Ordering::Relaxed);
        }
        progress.skipped.fetch_add(work.skipped, Ordering::Relaxed);
        drop((work, part, hold));
        Ok((entries, postings))
    })?;
    for (entries, postings) in found {
        for (all, one) in counted.entries.iter_mut().zip(entries) {
            *all += one;
        }
        for (all, one) in counted.postings.iter_mut().zip(postings) {
            *all += one;
        }
    }
    Ok(counted)
}

/// The games of a pass, handed to its workers a few at a time as each comes
/// free.
pub(super) struct Chunks {
    next: AtomicU64,
    last: u64,
    size: u64,
}

impl Chunks {
    /// Records `first..=last` for `workers` workers: about sixteen chunks a
    /// worker, of 16 to 1,024 records.
    pub fn new(first: u32, last: u32, workers: usize) -> Chunks {
        let records = (u64::from(last) + 1).saturating_sub(u64::from(first));
        let size = records.div_ceil(16 * workers.max(1) as u64).clamp(16, 1024);
        Chunks { next: AtomicU64::new(u64::from(first)), last: u64::from(last), size }
    }

    /// The next records to take, first and last; `None` once all are taken.
    pub fn take(&self) -> Option<(u32, u32)> {
        let lo = self.next.fetch_add(self.size, Ordering::Relaxed);
        (lo <= self.last).then(|| (lo as u32, (lo + self.size - 1).min(self.last) as u32))
    }
}

/// The index file being written, from its start to its end: the header's
/// room first, filled in last.
pub(super) struct Out {
    file: BufWriter<File>,
    path: PathBuf,
    /// Where the next bytes go.
    pub offset: u64,
}

impl Out {
    fn create(path: &Path) -> Result<Out, SearchError> {
        let file = File::create(path).map_err(|e| io(path, e))?;
        let mut out = Out { file: BufWriter::with_capacity(OUT_BUFFER, file), path: path.to_path_buf(), offset: 0 };
        out.put(&[0u8; HEADER_LEN])?;
        Ok(out)
    }

    pub fn put(&mut self, bytes: &[u8]) -> Result<(), SearchError> {
        self.file.write_all(bytes).map_err(|e| io(&self.path, e))?;
        self.offset += bytes.len() as u64;
        Ok(())
    }

    /// Writes `header` in its room and syncs the file.
    fn finish(self, header: &Header) -> Result<(), SearchError> {
        let path = self.path;
        let mut file = self.file.into_inner().map_err(|e| io(&path, e.into_error()))?;
        file.seek(SeekFrom::Start(0)).map_err(|e| io(&path, e))?;
        file.write_all(&header.encode()).map_err(|e| io(&path, e))?;
        file.sync_all().map_err(|e| io(&path, e))
    }
}

/// Units of work made on the workers in any order and written in theirs, so
/// that a file is written once, from its start to its end, whatever the
/// workers' progress. Each worker takes the next unit as it comes free, makes
/// its bytes and hands them over: they are written at once when every unit
/// before theirs is, else kept until then, and the worker goes on with its
/// next unit, so that no worker waits for another to write; whoever writes a
/// unit writes the kept ones that follow it. What is kept stays within
/// `room` bytes: a worker whose bytes do not fit waits until they do, or
/// until its unit's turn has come. A unit too large for one hand-over is
/// handed over in pieces, in order, the last one ending it.
pub(super) struct Turns<T, M> {
    taken: AtomicU32,
    units: u32,
    room: usize,
    state: Mutex<Queue<T, M>>,
    written: Condvar,
}

/// The units written and kept.
struct Queue<T, M> {
    /// The next unit to write.
    next: u32,
    sink: T,
    /// The pieces made ahead of their turn, by unit, in order: each with its
    /// bytes and whether it ends its unit.
    kept: BTreeMap<u32, VecDeque<(M, usize, bool)>>,
    bytes: usize,
}

/// Writes a piece of a unit to the sink.
pub(super) type WriteUnit<'a, T, M> = &'a (dyn Fn(&mut T, M) -> Result<(), SearchError> + Sync);

impl<T, M> Turns<T, M> {
    pub fn new(units: usize, room: usize, sink: T) -> Turns<T, M> {
        let units = u32::try_from(units).unwrap_or(u32::MAX);
        let queue = Queue { next: 0, sink, kept: BTreeMap::new(), bytes: 0 };
        Turns { taken: AtomicU32::new(0), units, room, state: Mutex::new(queue), written: Condvar::new() }
    }

    /// The next unit to make, `None` once all are taken.
    pub fn take(&self) -> Option<usize> {
        let unit = self.taken.fetch_add(1, Ordering::Relaxed);
        (unit < self.units).then_some(unit as usize)
    }

    /// Hands over `made`, `bytes` long, a piece of `unit`, the last one when
    /// `last`, to be written with `write`. A worker waiting for room stops,
    /// `Superseded`, once `stopped`, as every worker does after another one
    /// failed.
    pub fn put(
        &self,
        unit: usize,
        made: M,
        bytes: usize,
        last: bool,
        stopped: &dyn Fn() -> bool,
        write: WriteUnit<'_, T, M>,
    ) -> Result<(), SearchError> {
        let unit = unit as u32;
        let mut q = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // A piece fits when nothing is kept, however large.
        while q.next != unit && q.bytes > 0 && q.bytes + bytes > self.room {
            if stopped() {
                return Err(SearchError::Superseded);
            }
            q = self.written.wait_timeout(q, Duration::from_millis(50)).unwrap_or_else(|e| e.into_inner()).0;
        }
        if q.next != unit {
            q.bytes += bytes;
            q.kept.entry(unit).or_default().push_back((made, bytes, last));
            return Ok(());
        }
        write(&mut q.sink, made)?;
        if !last {
            return Ok(());
        }
        q.next += 1;
        // The units kept after it, as far as they are whole; the pieces of
        // one still being made are written, and its worker writes the rest.
        loop {
            let next = q.next;
            let Some(pieces) = q.kept.remove(&next) else { break };
            let mut ended = false;
            for (made, bytes, last) in pieces {
                q.bytes -= bytes;
                write(&mut q.sink, made)?;
                ended = last;
            }
            if !ended {
                break;
            }
            q.next += 1;
        }
        self.written.notify_all();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Units made on many workers, each taking its time, are written in
    /// their order, some in pieces among them, and no more than the room is
    /// ever kept; a worker waiting for room stops once told to.
    #[test]
    fn units_are_written_in_order_whatever_the_workers_progress() {
        let most = std::sync::atomic::AtomicUsize::new(0);
        let turns: Turns<(Vec<usize>, usize), (usize, usize)> = Turns::new(500, 30, (Vec::new(), 0));
        let write = |out: &mut (Vec<usize>, usize), (unit, _): (usize, usize)| {
            out.0.push(unit);
            Ok(())
        };
        std::thread::scope(|s| {
            for w in 0..8u64 {
                let (turns, write, most) = (&turns, &write, &most);
                s.spawn(move || {
                    while let Some(unit) = turns.take() {
                        std::thread::sleep(Duration::from_micros((unit as u64 * 7919 + w) % 200));
                        // Every tenth unit in three pieces.
                        let pieces = if unit % 10 == 0 { 3 } else { 1 };
                        for p in 0..pieces {
                            turns.put(unit, (unit, p), 10, p + 1 == pieces, &|| false, write).unwrap();
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
        assert_eq!((q.next, q.bytes, q.kept.len()), (500, 0, 0));
        assert!(most.into_inner() <= 30, "kept within the room");
        // Unit 1 kept, unit 2 waits for room that unit 0, never handed over,
        // would free: it stops once told to.
        let stuck: Turns<Vec<usize>, usize> = Turns::new(3, 10, Vec::new());
        let write = |out: &mut Vec<usize>, unit: usize| {
            out.push(unit);
            Ok(())
        };
        stuck.put(1, 1, 10, true, &|| false, &write).unwrap();
        assert!(matches!(stuck.put(2, 2, 10, true, &|| true, &write), Err(SearchError::Superseded)));
        stuck.put(0, 0, 10, true, &|| false, &write).unwrap();
        assert_eq!(stuck.state.into_inner().unwrap().sink, [0, 1]);
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
