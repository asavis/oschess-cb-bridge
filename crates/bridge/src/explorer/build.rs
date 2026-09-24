//! A whole build: sorted runs from the games, then one merge that adds up each
//! position's games, results, moves and notable games and writes the index
//! file in key order, dropping single-game positions beyond the pruning ply.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use crate::search::SearchError;

use super::format::{BLOCK_KEYS, Block, Counts, HEADER_LEN, Header, MAX_PLY, NO_MOVE, Stats, TOP_GAMES, crc32};
use super::runs::{self, Entry, Progress, io};
use super::source::Source;

/// What to build: records `first..=last`, pruned or not, and the facts the
/// header records.
pub struct Plan {
    pub kind: u8,
    pub first: u32,
    pub last: u32,
    pub prune_ply: u8,
    pub generation: u64,
    pub digest: u64,
}

/// Builds the index of `plan` into `target`, through a temporary file renamed
/// at the end; the runs go to `work`, which is emptied afterwards.
pub fn build(
    source: &dyn Source,
    plan: &Plan,
    work: &Path,
    target: &Path,
    progress: &Progress,
) -> Result<Header, SearchError> {
    let _ = std::fs::remove_dir_all(work);
    std::fs::create_dir_all(work).map_err(|e| io(work, e))?;
    let result = build_in(source, plan, work, target, progress);
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
) -> Result<Header, SearchError> {
    progress.start("reading", u64::from(plan.last.saturating_sub(plan.first) + 1));
    let runs = runs::write_runs(source, plan.first, plan.last, MAX_PLY, work, progress)?;
    let entries: u64 = runs.iter().map(|r| r.entries).sum();
    progress.start("merging", entries);
    let runs = runs::reduce(runs, work, progress)?;
    let partial = temporary(target);
    let mut writer = Writer::create(&partial)?;
    let mut agg = Aggregate::default();
    // Every game indexed starts with its ply-0 position, once.
    let mut games = 0u64;
    runs::merge(&runs, progress, |e| {
        if progress.stop.load(Ordering::Relaxed) {
            return Err(SearchError::Superseded);
        }
        progress.done.fetch_add(1, Ordering::Relaxed);
        if e.ply() == 0 {
            games += 1;
        }
        if agg.count.games > 0 && e.key != agg.key {
            progress.positions.fetch_add(1, Ordering::Relaxed);
            agg.emit(plan.prune_ply, &mut writer)?;
        }
        agg.add(&e);
        Ok(())
    })?;
    if agg.count.games > 0 {
        progress.positions.fetch_add(1, Ordering::Relaxed);
        agg.emit(plan.prune_ply, &mut writer)?;
    }
    let header = Header {
        kind: plan.kind,
        max_ply: MAX_PLY,
        prune_ply: plan.prune_ply,
        first_record: plan.first,
        last_record: plan.last,
        generation: plan.generation,
        digest: plan.digest,
        games,
        keys: 0,
        blocks: 0,
        table_offset: 0,
        table_crc: 0,
        file_len: 0,
    };
    let header = writer.finish(header)?;
    std::fs::rename(&partial, target).map_err(|e| io(target, e))?;
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

/// Writes blocks in key order, then the table of blocks and the header.
struct Writer {
    out: BufWriter<File>,
    path: PathBuf,
    offset: u64,
    keys: Vec<u8>,
    data: Vec<u8>,
    first_key: u64,
    in_block: usize,
    table: Vec<u8>,
    blocks: u32,
    total_keys: u64,
    record: Vec<u8>,
}

impl Writer {
    fn create(path: &Path) -> Result<Writer, SearchError> {
        let mut out = BufWriter::with_capacity(1 << 20, File::create(path).map_err(|e| io(path, e))?);
        out.write_all(&[0u8; HEADER_LEN]).map_err(|e| io(path, e))?;
        Ok(Writer {
            out,
            path: path.to_path_buf(),
            offset: HEADER_LEN as u64,
            keys: Vec::new(),
            data: Vec::new(),
            first_key: 0,
            in_block: 0,
            table: Vec::new(),
            blocks: 0,
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
        if self.in_block == BLOCK_KEYS {
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
        block.encode(&mut self.table);
        self.blocks += 1;
        all.clear();
        self.keys = all;
        self.data.clear();
        self.in_block = 0;
        Ok(())
    }

    fn finish(mut self, mut header: Header) -> Result<Header, SearchError> {
        self.flush_block()?;
        header.keys = self.total_keys;
        header.blocks = self.blocks;
        header.table_offset = self.offset;
        header.table_crc = crc32(&self.table);
        header.file_len = self.offset + self.table.len() as u64;
        let path = self.path.clone();
        self.out.write_all(&self.table).map_err(|e| io(&path, e))?;
        let mut file = self.out.into_inner().map_err(|e| io(&path, e.into_error()))?;
        file.seek(SeekFrom::Start(0)).map_err(|e| io(&path, e))?;
        file.write_all(&header.encode()).map_err(|e| io(&path, e))?;
        file.sync_all().map_err(|e| io(&path, e))?;
        Ok(header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::format::Outcome;

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
