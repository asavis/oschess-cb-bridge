//! Sort orders: every record number, in the order of one key.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Mutex;

use cbformat::game::Eco;

use super::SearchError;
use super::memory::{Cancel, Held, Hold, Refused};
use super::names::NameTable;
use super::query::{Sort, SortKey};
use super::scan::{Control, scan};
use super::workers::{self, threads};
use crate::store::{Head, Store};

/// Records a merging worker takes at least; a smaller order merges on one.
const MERGE_PART_MIN: usize = 1 << 18;
/// Records merged between two cancellation checks.
const MERGE_CHECK: usize = 1 << 20;

/// The ranks a key needs besides the record: players' and annotators' name
/// orders (one where annotators are players), and the joint name order of
/// tournaments and of the titles of guiding texts and analyses, with the
/// table where a title's key finds its id.
pub struct Ranks<'a> {
    pub players: Option<&'a [u32]>,
    pub annotators: Option<&'a [u32]>,
    pub tournaments: Option<&'a [u32]>,
    pub titles: Option<&'a [u32]>,
    pub title_table: Option<&'a NameTable>,
}

/// Results in PGN string order: `*`, `0-0`, `0-1`, `1-0`, `1/2-1/2`.
fn result_rank(pgn: &str) -> u32 {
    match pgn {
        "*" => 0,
        "0-0" => 1,
        "0-1" => 2,
        "1-0" => 3,
        _ => 4,
    }
}

/// The key of `r` as a number whose order is the key's order. Unknown values
/// are 0, so they come first ascending and last descending. A guiding text or
/// an analysis has its own layout: it sorts by its title as the tournament and
/// by its author as the annotator, and has no other key.
fn key(r: &impl Head, key: SortKey, ranks: &Ranks<'_>) -> u32 {
    // An empty name has rank 0, and so has a missing one.
    let rank = |table: Option<&[u32]>, id: i64| {
        usize::try_from(id).ok().and_then(|i| table.and_then(|t| t.get(i))).copied().unwrap_or(0)
    };
    if let Some((title, author)) = r.other() {
        return match key {
            SortKey::Number => r.id(),
            SortKey::Tournament => rank(ranks.titles, ranks.title_table.map_or(title, |t| t.slot(title))),
            SortKey::Annotator => rank(ranks.annotators, author),
            _ => 0,
        };
    }
    match key {
        SortKey::Number => r.id(),
        SortKey::White => rank(ranks.players, r.white()),
        SortKey::Black => rank(ranks.players, r.black()),
        SortKey::Annotator => rank(ranks.annotators, r.annotator()),
        SortKey::Tournament => rank(ranks.tournaments, r.tournament()),
        SortKey::WhiteElo => r.elo().0.max(0) as u32,
        SortKey::BlackElo => r.elo().1.max(0) as u32,
        SortKey::Result => result_rank(r.result().pgn()),
        SortKey::Moves => r.move_count().max(0) as u32,
        // The code as shown; ChessBase's hidden sub-code does not order it.
        SortKey::Eco => match r.eco() {
            Eco::Code { code, .. } => u32::from(code) + 1,
            _ => 0,
        },
        SortKey::Date => (r.played_date().0 & 0x1f_ffff) as u32,
        // A sub-round is shown only with a round.
        SortKey::Round => match r.round() {
            (n, _) if n <= 0 => 0,
            (n, s) => ((n as u32) << 16) | s.max(0) as u32,
        },
    }
}

/// Bytes a sort order needs while it is built: a key and number per record,
/// and the finished order.
pub fn build_bytes(records: u32) -> usize {
    records as usize * 12
}

/// Every record number in `sort` order; ties by number, ascending, in both
/// directions. Each worker sorts its own range, and the ranges are merged.
/// The memory is reserved before anything is allocated, and the order keeps
/// what it holds.
pub fn build<S: Store>(
    db: &S,
    ctl: &Control<'_>,
    sort: Sort,
    ranks: &Ranks<'_>,
) -> Result<Held<Vec<u32>>, SearchError> {
    let total = db.record_count() as usize;
    let mut hold = Hold::reserve(build_bytes(db.record_count()))?;
    let flip = if sort.descending { u32::MAX } else { 0 };
    let runs = scan(
        db,
        ctl,
        |len| {
            let mut run: Vec<u64> = Vec::new();
            run.try_reserve_exact(len).map_err(|_| Refused::Busy)?;
            Ok(run)
        },
        |run, r| {
            run.push((u64::from(key(r, sort.key, ranks) ^ flip) << 32) | u64::from(r.id()));
            Ok(())
        },
        // Each worker sorts its own range; the ranges are merged below.
        |run| run.sort_unstable(),
    )?;
    let merged: usize = runs.iter().map(Vec::len).sum();
    let mut out: Vec<u32> = Vec::new();
    out.try_reserve_exact(total.max(merged)).map_err(|_| Refused::Busy)?;
    out.resize(merged, 0);
    merge(&runs, threads().min(merged / MERGE_PART_MIN).max(1), ctl.cancel, &mut out)?;
    drop(runs);
    hold.shrink(out.capacity() * 4);
    Ok(Held::new(out, hold))
}

/// Merges `runs`, each sorted, into `out` as record numbers in the order of
/// their packed keys, in up to `parts` parts on the workers. Every key holds
/// its record's number and so is unique: splitters sampled evenly from every
/// run cut the keys into ranges of values, and each range of every run merges
/// into its own part of `out`.
fn merge(runs: &[Vec<u64>], parts: usize, cancel: &Cancel, out: &mut [u32]) -> Result<(), SearchError> {
    let mut samples: Vec<u64> =
        runs.iter().filter(|r| !r.is_empty()).flat_map(|r| (1..parts).map(move |j| r[j * r.len() / parts])).collect();
    samples.sort_unstable();
    let splitters: Vec<u64> = match samples.len() {
        0 => Vec::new(),
        n => (1..parts).map(|k| samples[k * n / parts]).collect(),
    };
    // Where each part starts in each run; the last row is where the runs end.
    let mut starts = vec![vec![0; runs.len()]];
    starts.extend(splitters.iter().map(|&s| runs.iter().map(|r| r.partition_point(|&x| x < s)).collect()));
    starts.push(runs.iter().map(Vec::len).collect());
    let merge_part = |k: usize, part: &mut [u32], stopped: &dyn Fn() -> bool| -> Result<(), SearchError> {
        let (from, to) = (&starts[k], &starts[k + 1]);
        let mut heap: BinaryHeap<Reverse<(u64, usize, usize)>> =
            (0..runs.len()).filter(|&r| from[r] < to[r]).map(|r| Reverse((runs[r][from[r]], r, from[r]))).collect();
        let mut i = 0;
        while let Some(Reverse((packed, r, at))) = heap.pop() {
            if i % MERGE_CHECK == 0 && stopped() {
                return Err(SearchError::Superseded);
            }
            part[i] = packed as u32;
            i += 1;
            if at + 1 < to[r] {
                heap.push(Reverse((runs[r][at + 1], r, at + 1)));
            }
        }
        Ok(())
    };
    let parts = splitters.len() + 1;
    if parts == 1 {
        return merge_part(0, out, &|| cancel.is_cancelled());
    }
    let mut slots = Vec::with_capacity(parts);
    let mut rest = out;
    for k in 0..parts {
        let len = (0..runs.len()).map(|r| starts[k + 1][r] - starts[k][r]).sum();
        let (part, tail) = std::mem::take(&mut rest).split_at_mut(len);
        slots.push(Mutex::new(Some(part)));
        rest = tail;
    }
    workers::run(parts, 0, cancel, |w| {
        for k in (w.index..parts).step_by(w.count) {
            let part = slots[k].lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(part) = part {
                merge_part(k, part, &|| w.stopped() || cancel.is_cancelled())?;
            }
        }
        Ok(())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs merged in any number of parts give the order one sort of all
    /// their keys gives: few key values, so that many keys tie on the value
    /// and differ only by number, runs of one key, and empty runs.
    #[test]
    fn runs_merge_in_parts_as_one_sort() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut number = 0u32;
        for (count, per) in [(1, 1000), (5, 0), (16, 20_000), (3, 1), (7, 5)] {
            let mut runs: Vec<Vec<u64>> = (0..count)
                .map(|_| {
                    let len = per + (next() % 3) as usize;
                    let mut run: Vec<u64> = (0..len)
                        .map(|_| {
                            number += 1;
                            ((next() % 50) << 32) | u64::from(number)
                        })
                        .collect();
                    run.sort_unstable();
                    run
                })
                .collect();
            runs.insert(count / 2, Vec::new());
            let mut all = runs.concat();
            all.sort_unstable();
            let want: Vec<u32> = all.iter().map(|&p| p as u32).collect();
            for parts in [1, 2, 3, 16, 40] {
                let mut out = vec![0; want.len()];
                merge(&runs, parts, &Cancel::never(), &mut out).unwrap();
                assert_eq!(out, want, "{count} runs of about {per} keys in {parts} parts");
            }
        }
    }

    /// A superseded sort stops merging, on one part and on many.
    #[test]
    fn a_superseded_merge_stops() {
        let latest = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let old = Cancel::newest(&latest);
        let _newer = Cancel::newest(&latest);
        let runs: Vec<Vec<u64>> =
            (0..4u64).map(|r| (0..1000u64).map(|i| (i << 32) | (r * 1000 + i)).collect()).collect();
        for parts in [1, 4] {
            let mut out = vec![0; 4000];
            assert!(matches!(merge(&runs, parts, &old, &mut out), Err(SearchError::Superseded)));
        }
    }

    #[test]
    fn result_order_follows_the_pgn_text() {
        let mut texts = ["1/2-1/2", "1-0", "*", "0-1", "0-0"];
        texts.sort_by_key(|t| result_rank(t));
        let mut sorted = texts;
        sorted.sort();
        assert_eq!(texts, sorted);
    }
}
