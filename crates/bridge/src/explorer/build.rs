//! A whole build: sorted runs from the games, then a merge that adds up each
//! position's games, results, moves and notable games and writes them in key
//! order, dropping single-game positions beyond the pruning ply. The merge
//! takes the [`PARTS`] ranges of keys apart on the workers, each into a file of
//! its own blocks, and the index file is those blocks in key order, then the
//! table of blocks.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use crate::search::SearchError;
use crate::search::memory::Cancel;
use crate::search::workers::{self, threads};

use super::format::{
    BLOCK_DATA, BLOCK_KEYS, Block, Counts, HEADER_LEN, Header, MAX_PLY, NO_MOVE, Stats, TOP_GAMES, crc32,
};
use super::runs::{self, Entry, Limits, PARTS, Progress, RUN_BUFFER, Run, io};
use super::source::Source;

/// Entries a merging worker counts before it adds them to the progress.
const PROGRESS_STEP: u64 = 1 << 16;

/// What to build: records `first..=last` of the database at `generation`,
/// with single-game positions beyond `prune_ply` dropped.
pub struct Plan {
    pub first: u32,
    pub last: u32,
    pub prune_ply: u8,
    pub generation: u64,
}

/// The writer's memory: its output buffer, a block's keys and records, and the
/// table of blocks.
pub const WRITER_BYTES: usize = (1 << 20) + BLOCK_KEYS * 12 + super::format::MAX_BLOCK_DATA + (4 << 20);

/// Builds the index of `plan` into `target`, through a temporary file renamed
/// at the end; the runs go to `work`, which is emptied afterwards.
pub fn build_with(
    source: &dyn Source,
    plan: &Plan,
    work: &Path,
    target: &Path,
    progress: &Progress,
    limits: &Limits,
) -> Result<Header, SearchError> {
    let _ = std::fs::remove_dir_all(work);
    std::fs::create_dir_all(work).map_err(|e| io(work, e))?;
    let result = build_in(source, plan, work, target, progress, limits);
    let _ = std::fs::remove_dir_all(work);
    if result.is_err() {
        let _ = std::fs::remove_file(temporary(target));
    }
    result
}

fn build_in(
    source: &dyn Source,
    plan: &Plan,
    work: &Path,
    target: &Path,
    progress: &Progress,
    limits: &Limits,
) -> Result<Header, SearchError> {
    // The merges fit the build's share with the writer's memory, so a merge
    // never waits for what the same build holds.
    let fan_ins = runs::fan_ins(limits.share, WRITER_BYTES)?;
    progress.start("reading", u64::from(plan.last.saturating_sub(plan.first) + 1));
    let runs = runs::write_runs(source, plan.first, plan.last, MAX_PLY, work, progress, limits)?;
    let entries: u64 = runs.iter().map(|r| r.entries).sum();
    progress.start("merging", entries);
    let runs = runs::reduce(runs, work, progress, fan_ins)?;
    let parts = merge_parts(&runs, work, plan.prune_ply, progress, limits.share);
    for r in &runs {
        let _ = std::fs::remove_file(&r.path);
    }
    let parts = parts?;
    let partial = temporary(target);
    let _writer_memory = runs::reserve(WRITER_BYTES, progress)?;
    let header = Header {
        max_ply: MAX_PLY,
        prune_ply: plan.prune_ply,
        first_record: plan.first,
        last_record: plan.last,
        generation: plan.generation,
        games: 0,
        keys: 0,
        blocks: 0,
        table_offset: 0,
        table_crc: 0,
        file_len: 0,
    };
    let header = assemble(&parts, &partial, header)?;
    std::fs::rename(&partial, target).map_err(|e| io(target, e))?;
    Ok(header)
}

/// One part's blocks, written apart: their file, its length, the part's
/// table of blocks at offsets within that file, and its counts.
struct Part {
    path: PathBuf,
    len: u64,
    table: Vec<Block>,
    keys: u64,
    /// Games indexed: every game starts with its ply-0 position, once.
    games: u64,
}

/// Merges each part of `runs` into a file of its blocks in `dir`, on up to
/// half the workers and as many at once as `share` holds their read buffers
/// and writers; the parts in key order.
fn merge_parts(
    runs: &[Run],
    dir: &Path,
    prune_ply: u8,
    progress: &Progress,
    share: usize,
) -> Result<Vec<Part>, SearchError> {
    let each = runs.len() * RUN_BUFFER + WRITER_BYTES;
    let want = threads().div_ceil(2).min(PARTS).min(share / each).max(1);
    let merged: Vec<Mutex<Option<Part>>> = (0..PARTS).map(|_| Mutex::new(None)).collect();
    workers::run(want, 0, &Cancel::never(), |w| {
        for k in (w.index..PARTS).step_by(w.count) {
            let part = merge_part(runs, k, dir, prune_ply, progress, &|| w.stopped())?;
            *merged[k].lock().unwrap_or_else(|e| e.into_inner()) = Some(part);
        }
        Ok(())
    })?;
    merged
        .into_iter()
        .map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner()).ok_or(SearchError::Superseded))
        .collect()
}

/// Merges part `k` of `runs` into its file in `dir`.
fn merge_part(
    runs: &[Run],
    k: usize,
    dir: &Path,
    prune_ply: u8,
    progress: &Progress,
    stopped: &dyn Fn() -> bool,
) -> Result<Part, SearchError> {
    let _writer_memory = runs::reserve(WRITER_BYTES, progress)?;
    let mut writer = Writer::create(&dir.join(format!("part-{k}")))?;
    let mut agg = Aggregate::default();
    let (mut games, mut done, mut positions) = (0u64, 0u64, 0u64);
    let report = |done: &mut u64, positions: &mut u64| {
        progress.done.fetch_add(std::mem::take(done), Ordering::Relaxed);
        progress.positions.fetch_add(std::mem::take(positions), Ordering::Relaxed);
    };
    runs::merge_part(runs, k, progress, |e| {
        if progress.stop.load(Ordering::Relaxed) || stopped() {
            return Err(SearchError::Superseded);
        }
        done += 1;
        if done == PROGRESS_STEP {
            report(&mut done, &mut positions);
        }
        if e.ply() == 0 {
            games += 1;
        }
        if agg.count.games > 0 && e.key != agg.key {
            positions += 1;
            agg.emit(prune_ply, &mut writer)?;
        }
        agg.add(&e);
        Ok(())
    })?;
    if agg.count.games > 0 {
        positions += 1;
        agg.emit(prune_ply, &mut writer)?;
    }
    report(&mut done, &mut positions);
    writer.close(games)
}

/// Writes the index file at `path`: the header, the parts' blocks in key
/// order, each part's file removed once copied, and the table of blocks.
fn assemble(parts: &[Part], path: &Path, mut header: Header) -> Result<Header, SearchError> {
    let mut out = BufWriter::with_capacity(1 << 20, File::create(path).map_err(|e| io(path, e))?);
    out.write_all(&[0u8; HEADER_LEN]).map_err(|e| io(path, e))?;
    let mut offset = HEADER_LEN as u64;
    let mut table = Vec::new();
    let mut blocks = 0u64;
    for part in parts {
        let mut input = File::open(&part.path).map_err(|e| io(&part.path, e))?;
        let copied = std::io::copy(&mut input, &mut out).map_err(|e| io(path, e))?;
        if copied != part.len {
            return Err(io(&part.path, std::io::Error::other("a part has the wrong length")));
        }
        drop(input);
        let _ = std::fs::remove_file(&part.path);
        for b in &part.table {
            Block { offset: b.offset + offset, ..*b }.encode(&mut table);
        }
        offset += part.len;
        blocks += part.table.len() as u64;
        header.keys += part.keys;
        header.games += part.games;
    }
    header.blocks = u32::try_from(blocks).map_err(|_| SearchError::TooLarge)?;
    header.table_offset = offset;
    header.table_crc = crc32(&table);
    header.file_len = offset + table.len() as u64;
    out.write_all(&table).map_err(|e| io(path, e))?;
    let mut file = out.into_inner().map_err(|e| io(path, e.into_error()))?;
    file.seek(SeekFrom::Start(0)).map_err(|e| io(path, e))?;
    file.write_all(&header.encode()).map_err(|e| io(path, e))?;
    file.sync_all().map_err(|e| io(path, e))?;
    Ok(header)
}

fn temporary(target: &Path) -> PathBuf {
    let mut name = target.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".partial");
    target.with_file_name(name)
}

/// One position's entries, added up as the merge passes them.
#[derive(Default)]
struct Aggregate {
    key: u64,
    count: Counts,
    min_ply: u8,
    moves: Vec<(u16, Counts)>,
    /// The best games so far, best first: (rating, game).
    top: Vec<(u16, u32)>,
}

impl Aggregate {
    fn add(&mut self, e: &Entry) {
        if self.count.games == 0 {
            self.key = e.key;
            self.min_ply = e.ply();
        }
        self.min_ply = self.min_ply.min(e.ply());
        let outcome = e.outcome();
        self.count.add(outcome);
        if e.mv() != NO_MOVE {
            match self.moves.iter_mut().find(|m| m.0 == e.mv()) {
                Some(m) => m.1.add(outcome),
                None => {
                    let mut c = Counts::default();
                    c.add(outcome);
                    self.moves.push((e.mv(), c));
                }
            }
        }
        // Higher rating first; among equal ratings, the later game.
        let item = (e.elo(), e.game());
        let at = self.top.partition_point(|&t| t > item);
        if at < TOP_GAMES {
            self.top.insert(at, item);
            self.top.truncate(TOP_GAMES);
        }
    }

    /// Writes the position unless it is one game's beyond `prune_ply`, and
    /// starts afresh.
    fn emit(&mut self, prune_ply: u8, writer: &mut Writer) -> Result<(), SearchError> {
        if !(self.count.games == 1 && self.min_ply > prune_ply) {
            self.moves.sort_unstable_by(|a, b| b.1.games.cmp(&a.1.games).then(a.0.cmp(&b.0)));
            let stats = Stats {
                counts: self.count,
                moves: std::mem::take(&mut self.moves),
                top: self.top.iter().map(|t| t.1).collect(),
            };
            writer.push(self.key, &stats)?;
        }
        self.count = Counts::default();
        self.moves.clear();
        self.top.clear();
        Ok(())
    }
}

/// Writes one part's blocks in key order into a file of their own, and keeps
/// its table of blocks.
struct Writer {
    out: BufWriter<File>,
    path: PathBuf,
    offset: u64,
    keys: Vec<u8>,
    data: Vec<u8>,
    first_key: u64,
    in_block: usize,
    table: Vec<Block>,
    total_keys: u64,
    record: Vec<u8>,
}

impl Writer {
    fn create(path: &Path) -> Result<Writer, SearchError> {
        let out = BufWriter::with_capacity(1 << 20, File::create(path).map_err(|e| io(path, e))?);
        Ok(Writer {
            out,
            path: path.to_path_buf(),
            offset: 0,
            keys: Vec::new(),
            data: Vec::new(),
            first_key: 0,
            in_block: 0,
            table: Vec::new(),
            total_keys: 0,
            record: Vec::new(),
        })
    }

    fn push(&mut self, key: u64, stats: &Stats) -> Result<(), SearchError> {
        if self.in_block == 0 {
            self.first_key = key;
        }
        self.record.clear();
        stats.encode(&mut self.record);
        let at = u32::try_from(self.data.len()).map_err(|_| SearchError::TooLarge)?;
        self.keys.extend(key.to_le_bytes());
        self.keys.extend(at.to_le_bytes());
        self.data.extend_from_slice(&self.record);
        self.in_block += 1;
        self.total_keys += 1;
        if self.in_block == BLOCK_KEYS || self.data.len() >= BLOCK_DATA {
            self.flush_block()?;
        }
        Ok(())
    }

    fn flush_block(&mut self) -> Result<(), SearchError> {
        if self.in_block == 0 {
            return Ok(());
        }
        let mut all = std::mem::take(&mut self.keys);
        all.extend_from_slice(&self.data);
        let block = Block {
            first_key: self.first_key,
            offset: self.offset,
            keys: self.in_block as u32,
            data_len: u32::try_from(self.data.len()).map_err(|_| SearchError::TooLarge)?,
            crc: crc32(&all),
        };
        self.out.write_all(&all).map_err(|e| io(&self.path, e))?;
        self.offset += all.len() as u64;
        self.table.push(block);
        all.clear();
        self.keys = all;
        self.data.clear();
        self.in_block = 0;
        Ok(())
    }

    /// The part as written, `games` games in it.
    fn close(mut self, games: u64) -> Result<Part, SearchError> {
        self.flush_block()?;
        self.out.flush().map_err(|e| io(&self.path, e))?;
        Ok(Part { path: self.path, len: self.offset, table: self.table, keys: self.total_keys, games })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::file::IndexFile;
    use crate::explorer::format::{Outcome, PRUNE_PLY};

    /// Positions of every part, spread over three runs, merge part by part
    /// into one index: its table passes the checks of opening, and every
    /// position holds all its games, as one merge of all the runs would.
    #[test]
    fn parts_merge_into_one_index_in_key_order() {
        let dir = std::env::temp_dir().join(format!("bridge-build-parts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = |i: u64| i.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let mut games_of = std::collections::BTreeMap::new();
        let mut runs = Vec::new();
        let mut game = 0u32;
        for r in 0..3 {
            let mut buf = Vec::new();
            for i in 0..4_000u64 {
                if (i + r as u64).is_multiple_of(3) {
                    continue;
                }
                game += 1;
                // Each game passes one position at ply 0 and another at ply 2.
                buf.push(Entry::new(key(i), game, Outcome::White, 100, 0, 2000));
                buf.push(Entry::new(key(i + 10_000), game, Outcome::Draw, 101, 2, 2100));
                *games_of.entry(key(i)).or_insert(0u64) += 1;
                *games_of.entry(key(i + 10_000)).or_insert(0u64) += 1;
            }
            runs::flush(&mut buf, &dir, r, &mut runs).unwrap();
        }
        assert!((0..PARTS).all(|k| runs.iter().all(|r| r.parts[k + 1] > r.parts[k])), "every run holds every part");
        let parts = merge_parts(&runs, &dir, PRUNE_PLY, &Progress::default(), 64 << 20).unwrap();
        assert_eq!(parts.len(), PARTS);
        let header = Header {
            max_ply: MAX_PLY,
            prune_ply: PRUNE_PLY,
            first_record: 1,
            last_record: game,
            generation: 1,
            games: 0,
            keys: 0,
            blocks: 0,
            table_offset: 0,
            table_crc: 0,
            file_len: 0,
        };
        let path = dir.join("index");
        let header = assemble(&parts, &path, header).unwrap();
        assert_eq!((header.games, header.keys), (u64::from(game), games_of.len() as u64));
        assert!(header.blocks as usize >= PARTS, "a part's blocks end with it");
        let file = IndexFile::open(&path).unwrap();
        for (&key, &games) in &games_of {
            assert_eq!(file.lookup(key).unwrap().map(|s| s.counts.games), Some(games), "{key:x}");
        }
        assert_eq!(file.lookup(key(4_000)).unwrap(), None);
        assert!(!dir.join("part-0").exists(), "a part's file is removed once copied");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_position_keeps_its_best_games_and_counts_each_move() {
        let mut a = Aggregate::default();
        for g in 1..=20u32 {
            let outcome = [Outcome::White, Outcome::Draw, Outcome::Black, Outcome::Other][g as usize % 4];
            a.add(&Entry::new(9, g, outcome, if g % 2 == 0 { 70 } else { 71 }, 3, (g * 100) as u16));
        }
        assert_eq!(a.count, Counts { games: 20, white: 5, draws: 5, black: 5 });
        assert_eq!(a.top.len(), TOP_GAMES);
        assert_eq!(a.top[0], (2000, 20));
        assert_eq!(a.moves.iter().map(|m| m.1.games).sum::<u64>(), 20);
    }
}
