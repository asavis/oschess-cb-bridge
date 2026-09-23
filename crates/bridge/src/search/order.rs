//! Sort orders: every record number, in the order of one key.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use cbformat::v2::{Database, Eco, Record, RecordKind};

use super::query::{Sort, SortKey};
use super::scan::{scan, threads};

/// The ranks a key needs besides the record: players' name order, and the
/// joint name order of tournaments and of the titles of guiding texts and
/// analyses.
pub struct Ranks<'a> {
    pub players: Option<&'a [u32]>,
    pub tournaments: Option<&'a [u32]>,
    pub titles: Option<&'a [u32]>,
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
fn key(r: &Record, key: SortKey, ranks: &Ranks<'_>) -> u32 {
    let rank = |table: Option<&[u32]>, id: i64| {
        usize::try_from(id).ok().and_then(|i| table.and_then(|t| t.get(i))).map_or(0, |&r| r + 1)
    };
    let other = match r.kind() {
        RecordKind::Game => None,
        RecordKind::Text => Some((r.text_title(), r.text_author())),
        RecordKind::Analysis => Some((r.analysis_title(), r.analysis_author())),
        RecordKind::Unknown(_) => Some((-1, -1)),
    };
    if let Some((title, author)) = other {
        return match key {
            SortKey::Number => r.id(),
            SortKey::Tournament => rank(ranks.titles, title),
            SortKey::Annotator => rank(ranks.players, author),
            _ => 0,
        };
    }
    match key {
        SortKey::Number => r.id(),
        SortKey::White => rank(ranks.players, r.white()),
        SortKey::Black => rank(ranks.players, r.black()),
        SortKey::Annotator => rank(ranks.players, r.annotator()),
        SortKey::Tournament => rank(ranks.tournaments, r.tournament()),
        SortKey::WhiteElo => r.white_elo().max(0) as u32,
        SortKey::BlackElo => r.black_elo().max(0) as u32,
        SortKey::Result => result_rank(r.result().pgn()),
        SortKey::Moves => r.move_count().max(0) as u32,
        SortKey::Eco => match r.eco() {
            Eco::Code { code, sub } => u32::from(code) * 128 + u32::from(sub) + 1,
            _ => 0,
        },
        SortKey::Date => (r.played_date().0 & 0x1f_ffff) as u32,
        SortKey::Round => ((i32::from(r.round()).max(0) as u32) << 16) | i32::from(r.subround()).max(0) as u32,
    }
}

/// Every record number in `sort` order; ties by number, ascending, in both
/// directions. Each worker sorts its own range, and the ranges are merged.
pub fn build(db: &Database, sort: Sort, ranks: &Ranks<'_>) -> cbformat::Result<Vec<u32>> {
    let flip = if sort.descending { u32::MAX } else { 0 };
    let mut runs = scan(db, Vec::new, |run: &mut Vec<u64>, r| {
        run.push((u64::from(key(r, sort.key, ranks) ^ flip) << 32) | u64::from(r.id()));
    })?;
    std::thread::scope(|s| {
        let workers = threads().max(1);
        let per = runs.len().div_ceil(workers).max(1);
        for chunk in runs.chunks_mut(per) {
            s.spawn(move || chunk.iter_mut().for_each(|run| run.sort_unstable()));
        }
    });
    let total: usize = runs.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(total);
    let mut heap: BinaryHeap<Reverse<(u64, usize, usize)>> =
        runs.iter().enumerate().filter(|(_, r)| !r.is_empty()).map(|(i, r)| Reverse((r[0], i, 0))).collect();
    while let Some(Reverse((packed, run, at))) = heap.pop() {
        out.push(packed as u32);
        if let Some(&next) = runs[run].get(at + 1) {
            heap.push(Reverse((next, run, at + 1)));
        }
    }
    runs.clear();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_order_follows_the_pgn_text() {
        let mut texts = ["1/2-1/2", "1-0", "*", "0-1", "0-0"];
        texts.sort_by_key(|t| result_rank(t));
        let mut sorted = texts;
        sorted.sort();
        assert_eq!(texts, sorted);
    }
}
