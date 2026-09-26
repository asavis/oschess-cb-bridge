//! The first half of a build: each worker turns its games into (position,
//! game) entries, sorts them in memory within its share of the budget, and
//! writes them as sorted runs; runs beyond one merge's fan-in are merged into
//! longer runs first.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::search::SearchError;
use crate::search::memory::{Cancel, Hold, Refused};
use crate::search::workers::{self, threads};

use super::file::read_at;
use super::format::Outcome;
use super::source::{Line, Source, Workspace};

/// One game passing through one position, in 16 bytes: the key, the game and
/// its outcome, and the move, ply and rating.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub key: u64,
    /// The game number in the high 30 bits, the outcome in the low 2.
    pub game_outcome: u32,
    /// The move in the low 14 bits, the ply in the next 6, the rating in the top 12.
    pub meta: u32,
}

pub const ENTRY_BYTES: usize = 16;
/// The largest game number an entry holds.
pub const MAX_GAME: u32 = (1 << 30) - 1;
/// Runs merged at once.
pub const FAN_IN: usize = 256;
/// The read buffer of each run in a merge.
pub const RUN_BUFFER: usize = 64 << 10;
/// The top bits of a key that name its part.
const PART_BITS: u32 = 4;
/// Ranges of keys the final merge takes apart, on the workers. Keys are
/// hashes, so equal ranges of their values hold about as many positions.
pub const PARTS: usize = 1 << PART_BITS;

/// The part of the keys `key` lies in.
pub fn part_of(key: u64) -> usize {
    (key >> (64 - PART_BITS)) as usize
}

impl Entry {
    pub fn new(key: u64, game: u32, outcome: Outcome, mv: u16, ply: u8, elo: u16) -> Entry {
        Entry {
            key,
            game_outcome: game << 2 | outcome as u32,
            meta: u32::from(mv & 0x3fff) | u32::from(ply & 63) << 14 | u32::from(elo.min(4095)) << 20,
        }
    }

    pub fn game(&self) -> u32 {
        self.game_outcome >> 2
    }
    pub fn outcome(&self) -> Outcome {
        Outcome::from_bits(self.game_outcome)
    }
    pub fn mv(&self) -> u16 {
        (self.meta & 0x3fff) as u16
    }
    pub fn ply(&self) -> u8 {
        (self.meta >> 14 & 63) as u8
    }
    pub fn elo(&self) -> u16 {
        (self.meta >> 20) as u16
    }

    fn to_bytes(self) -> [u8; ENTRY_BYTES] {
        let mut b = [0u8; ENTRY_BYTES];
        b[0..8].copy_from_slice(&self.key.to_le_bytes());
        b[8..12].copy_from_slice(&self.game_outcome.to_le_bytes());
        b[12..16].copy_from_slice(&self.meta.to_le_bytes());
        b
    }

    fn from_bytes(b: &[u8; ENTRY_BYTES]) -> Entry {
        Entry {
            key: u64::from_le_bytes(b[0..8].try_into().unwrap_or([0; 8])),
            game_outcome: u32::from_le_bytes(b[8..12].try_into().unwrap_or([0; 4])),
            meta: u32::from_le_bytes(b[12..16].try_into().unwrap_or([0; 4])),
        }
    }
}

/// A sorted run on disk and the entries it holds.
pub struct Run {
    pub path: PathBuf,
    pub entries: u64,
    /// Where each part's entries start in the run, and where the run ends.
    pub parts: [u64; PARTS + 1],
}

/// Where each part starts among entries of `counts[part]` each, in order.
fn starts(counts: &[u64; PARTS]) -> [u64; PARTS + 1] {
    let mut at = [0; PARTS + 1];
    for k in 0..PARTS {
        at[k + 1] = at[k] + counts[k];
    }
    at
}

/// How far a build has come: records read, then entries merged.
#[derive(Default)]
pub struct Progress {
    pub phase: std::sync::Mutex<&'static str>,
    pub done: AtomicU64,
    pub total: AtomicU64,
    /// Distinct positions merged so far, kept or dropped.
    pub positions: AtomicU64,
    /// Games left out because their moves could not be read.
    pub skipped: AtomicU64,
    pub stop: AtomicBool,
}

impl Progress {
    pub fn start(&self, phase: &'static str, total: u64) {
        *self.phase.lock().unwrap_or_else(|e| e.into_inner()) = phase;
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }

    pub fn phase(&self) -> &'static str {
        *self.phase.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub fn io(path: &Path, e: std::io::Error) -> SearchError {
    SearchError::Read(cbformat::Error::Io(path.to_path_buf(), e))
}

/// How long a build waits for memory that searches hold before it gives up.
pub const MEMORY_WAIT: Duration = Duration::from_secs(60);

/// Reserves `bytes` without evicting what searches keep, waiting while they
/// hold the budget: a build yields to searches, and fails `Busy` only after
/// [`MEMORY_WAIT`].
pub fn reserve(bytes: usize, progress: &Progress) -> Result<Hold, SearchError> {
    let deadline = Instant::now() + MEMORY_WAIT;
    loop {
        match Hold::reserve_quietly(bytes) {
            Ok(hold) => return Ok(hold),
            Err(Refused::Busy) if Instant::now() < deadline && !progress.stop.load(Ordering::Relaxed) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(refused) => return Err(refused.into()),
        }
    }
}

/// What a build may use.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The most the build holds in the search budget at once: half of it, so
    /// that searches keep the rest.
    pub share: usize,
    /// At most this many entries in a run, below what a worker's memory
    /// holds; tests use it to make many runs from few games.
    pub run_entries: Option<usize>,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { share: crate::search::memory::budget() / 2, run_entries: None }
    }
}

/// The fan-ins of a build's merges within `share` bytes: an intermediate merge
/// holds a read buffer per run and one write buffer; the final one a read
/// buffer per run beside the `writer`'s bytes. `TooLarge` when either could
/// not take two runs.
pub fn fan_ins(share: usize, writer: usize) -> Result<(usize, usize), SearchError> {
    let middle = (share / RUN_BUFFER).saturating_sub(1).min(FAN_IN);
    let last = (share.saturating_sub(writer) / RUN_BUFFER).min(FAN_IN);
    if middle < 2 || last < 2 {
        return Err(SearchError::TooLarge);
    }
    Ok((middle, last))
}

/// The memory of one worker's entries: a quarter of the budget shared by the
/// workers, from 4 MiB to 64 MiB.
fn run_bytes(workers: usize) -> usize {
    (crate::search::memory::budget() / 4 / workers.max(1)).clamp(4 << 20, 64 << 20)
}

/// Writes the entries of records `first..=last` as sorted runs in `dir`, on at
/// most half the shared workers, so that searches keep the rest.
pub fn write_runs(
    source: &dyn Source,
    first: u32,
    last: u32,
    max_ply: u8,
    dir: &Path,
    progress: &Progress,
    limits: &Limits,
) -> Result<Vec<Run>, SearchError> {
    if last < first {
        return Ok(Vec::new());
    }
    if last > MAX_GAME {
        return Err(SearchError::TooLarge);
    }
    let total = u64::from(last - first + 1);
    // Half the workers at most, and no more than half the budget holds with
    // their entries and read buffers, so that searches keep the rest.
    let want = threads().div_ceil(2).min(total.div_ceil(4096) as usize).max(1);
    let per_worker = run_bytes(want);
    let fit = limits.share / (per_worker + Workspace::BYTES);
    if fit == 0 {
        return Err(SearchError::TooLarge);
    }
    let want = want.min(fit).max(1);
    let runs = workers::run(want, 0, &Cancel::never(), |w| {
        // The entries, and the buffers games are read into, reserved first.
        let hold = reserve(per_worker + Workspace::BYTES, progress)?;
        let mut work = Workspace::new().ok_or(Refused::Busy)?;
        let capacity =
            (per_worker / std::mem::size_of::<Entry>()).min(limits.run_entries.unwrap_or(usize::MAX)).max(64);
        let mut buf: Vec<Entry> = Vec::new();
        buf.try_reserve_exact(capacity).map_err(|_| Refused::Busy)?;
        let per = total.div_ceil(w.count as u64);
        let lo = u64::from(first) + w.index as u64 * per;
        let hi = (lo + per - 1).min(u64::from(last));
        let mut runs = Vec::new();
        let mut failed = None;
        let mut next = lo;
        while next <= hi && failed.is_none() {
            if w.stopped() || progress.stop.load(Ordering::Relaxed) {
                return Err(SearchError::Superseded);
            }
            let end = (next + 4095).min(hi);
            source.lines(next as u32, end as u32, max_ply, &mut work, &mut |line: &Line| {
                if failed.is_some() {
                    return;
                }
                if buf.len() + line.positions.len() > capacity
                    && let Err(e) = flush(&mut buf, dir, w.index, &mut runs)
                {
                    failed = Some(e);
                    return;
                }
                for &(key, mv, ply) in &line.positions {
                    buf.push(Entry::new(key, line.number, line.outcome, mv, ply, line.elo));
                }
            })?;
            progress.done.fetch_add(end - next + 1, Ordering::Relaxed);
            next = end + 1;
        }
        if let Some(e) = failed {
            return Err(e);
        }
        flush(&mut buf, dir, w.index, &mut runs)?;
        progress.skipped.fetch_add(work.skipped, Ordering::Relaxed);
        drop((buf, work, hold));
        Ok(runs)
    })?;
    Ok(runs.into_iter().flatten().collect())
}

/// Sorts `buf` by key and game and writes it as a run.
pub(super) fn flush(buf: &mut Vec<Entry>, dir: &Path, worker: usize, runs: &mut Vec<Run>) -> Result<(), SearchError> {
    if buf.is_empty() {
        return Ok(());
    }
    buf.sort_unstable_by_key(|e| (e.key, e.game_outcome));
    let path = dir.join(format!("run-{worker}-{}", runs.len()));
    let mut out = BufWriter::with_capacity(RUN_BUFFER, File::create(&path).map_err(|e| io(&path, e))?);
    for e in buf.iter() {
        out.write_all(&e.to_bytes()).map_err(|e| io(&path, e))?;
    }
    out.flush().map_err(|e| io(&path, e))?;
    let mut counts = [0u64; PARTS];
    for e in buf.iter() {
        counts[part_of(e.key)] += 1;
    }
    runs.push(Run { path, entries: buf.len() as u64, parts: starts(&counts) });
    buf.clear();
    Ok(())
}

/// The files of `runs`, each opened once and checked against its length, for
/// their readers to share.
pub fn open(runs: &[Run]) -> Result<Vec<File>, SearchError> {
    runs.iter()
        .map(|run| {
            let file = File::open(&run.path).map_err(|e| io(&run.path, e))?;
            let len = file.metadata().map_err(|e| io(&run.path, e))?.len();
            if len != run.entries * ENTRY_BYTES as u64 {
                return Err(io(&run.path, std::io::Error::other("a run has the wrong length")));
            }
            Ok(file)
        })
        .collect()
}

/// A run's entries `from..to` read in order from its file, which other
/// readers may share: each reads at its own offset into its own buffer.
pub struct RunReader<'a> {
    file: &'a File,
    path: &'a Path,
    offset: u64,
    left: u64,
    buf: Vec<u8>,
    at: usize,
}

impl<'a> RunReader<'a> {
    fn new(file: &'a File, run: &'a Run, from: u64, to: u64) -> RunReader<'a> {
        RunReader {
            file,
            path: &run.path,
            offset: from * ENTRY_BYTES as u64,
            left: to.saturating_sub(from),
            buf: Vec::new(),
            at: 0,
        }
    }

    pub fn next_entry(&mut self) -> Result<Option<Entry>, SearchError> {
        if self.left == 0 {
            return Ok(None);
        }
        if self.at == self.buf.len() {
            let entries = (RUN_BUFFER / ENTRY_BYTES).min(usize::try_from(self.left).unwrap_or(usize::MAX));
            self.buf.resize(entries * ENTRY_BYTES, 0);
            read_at(self.file, self.offset, &mut self.buf).map_err(|e| io(self.path, e))?;
            self.offset += self.buf.len() as u64;
            self.at = 0;
        }
        let bytes: &[u8; ENTRY_BYTES] = self.buf[self.at..self.at + ENTRY_BYTES]
            .try_into()
            .map_err(|_| io(self.path, std::io::Error::other("a short read of a run")))?;
        self.at += ENTRY_BYTES;
        self.left -= 1;
        Ok(Some(Entry::from_bytes(bytes)))
    }
}

/// Merges `runs` in key order, calling `each` with every entry; the runs are
/// deleted once merged. At most [`FAN_IN`] runs may be given.
pub fn merge(
    runs: &[Run],
    progress: &Progress,
    each: impl FnMut(Entry) -> Result<(), SearchError>,
) -> Result<(), SearchError> {
    let _buffers = reserve(runs.len() * RUN_BUFFER, progress)?;
    let files = open(runs)?;
    merge_readers(runs.iter().zip(&files).map(|(r, f)| RunReader::new(f, r, 0, r.entries)).collect(), each)?;
    drop(files);
    for r in runs {
        let _ = std::fs::remove_file(&r.path);
    }
    Ok(())
}

/// Merges part `part` of `runs` in key order, calling `each` with its every
/// entry, from `files`, the runs' files as [`open`] gave them, which the other
/// parts share. The caller holds a read buffer per run in the budget.
pub fn merge_part(
    runs: &[Run],
    files: &[File],
    part: usize,
    each: impl FnMut(Entry) -> Result<(), SearchError>,
) -> Result<(), SearchError> {
    let readers = runs.iter().zip(files).map(|(r, f)| RunReader::new(f, r, r.parts[part], r.parts[part + 1])).collect();
    merge_readers(readers, each)
}

fn merge_readers(
    mut readers: Vec<RunReader<'_>>,
    mut each: impl FnMut(Entry) -> Result<(), SearchError>,
) -> Result<(), SearchError> {
    let mut heap = BinaryHeap::with_capacity(readers.len());
    for (i, r) in readers.iter_mut().enumerate() {
        if let Some(e) = r.next_entry()? {
            heap.push(Reverse((e.key, e.game_outcome, i, e.meta)));
        }
    }
    while let Some(Reverse((key, game_outcome, i, meta))) = heap.pop() {
        each(Entry { key, game_outcome, meta })?;
        if let Some(e) = readers[i].next_entry()? {
            heap.push(Reverse((e.key, e.game_outcome, i, e.meta)));
        }
    }
    Ok(())
}

/// Merges groups of at most `middle` runs into longer runs until at most
/// `last` remain, each group's buffers reserved before it is merged.
pub fn reduce(
    mut runs: Vec<Run>,
    dir: &Path,
    progress: &Progress,
    (middle, last): (usize, usize),
) -> Result<Vec<Run>, SearchError> {
    let mut level = 0;
    while runs.len() > last.max(1) {
        let mut next = Vec::new();
        for (n, group) in runs.chunks(middle.max(2)).enumerate() {
            let _out_buffer = reserve(RUN_BUFFER, progress)?;
            let path = dir.join(format!("merged-{level}-{n}"));
            let mut out = BufWriter::with_capacity(RUN_BUFFER, File::create(&path).map_err(|e| io(&path, e))?);
            let mut counts = [0u64; PARTS];
            merge(group, progress, |e| {
                counts[part_of(e.key)] += 1;
                out.write_all(&e.to_bytes()).map_err(|e| io(&path, e))
            })?;
            out.flush().map_err(|e| io(&path, e))?;
            next.push(Run { path, entries: counts.iter().sum(), parts: starts(&counts) });
        }
        runs = next;
        level += 1;
    }
    Ok(runs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_pack_every_field() {
        let e = Entry::new(u64::MAX - 3, MAX_GAME, Outcome::Black, 0x3fff, 40, 4095);
        assert_eq!(
            (e.key, e.game(), e.outcome(), e.mv(), e.ply(), e.elo()),
            (u64::MAX - 3, MAX_GAME, Outcome::Black, 0x3fff, 40, 4095)
        );
        assert_eq!(Entry::from_bytes(&e.to_bytes()), e);
        assert_eq!(Entry::new(1, 2, Outcome::Draw, 5, 0, 9999).elo(), 4095, "ratings are capped");
    }

    #[test]
    fn runs_merge_in_key_order_across_levels() {
        let dir = std::env::temp_dir().join(format!("bridge-runs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut runs = Vec::new();
        for r in 0..(FAN_IN + 3) {
            let mut buf: Vec<Entry> = (0..5u64)
                .map(|i| Entry::new((i * 7919 + r as u64 * 104_729) % 1000, r as u32, Outcome::White, 0, 0, 0))
                .collect();
            flush(&mut buf, &dir, r, &mut runs).unwrap();
        }
        let progress = Progress::default();
        let runs = reduce(runs, &dir, &progress, (FAN_IN, FAN_IN)).unwrap();
        assert!(runs.len() <= FAN_IN);
        let mut keys = Vec::new();
        merge(&runs, &progress, |e| {
            keys.push(e.key);
            Ok(())
        })
        .unwrap();
        assert_eq!(keys.len(), (FAN_IN + 3) * 5);
        assert!(keys.windows(2).all(|w| w[0] <= w[1]));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
