//! Sort orders: every record number, in the order of one key.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use cbformat::v2::Eco;

use super::SearchError;
use super::memory::{Held, Hold, Refused};
use super::names::NameTable;
use super::query::{Sort, SortKey};
use super::scan::{Control, scan};
use crate::store::{Head, Store};

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
    let mut out: Vec<u32> = Vec::new();
    out.try_reserve_exact(total).map_err(|_| Refused::Busy)?;
    let mut heap: BinaryHeap<Reverse<(u64, usize, usize)>> =
        runs.iter().enumerate().filter(|(_, r)| !r.is_empty()).map(|(i, r)| Reverse((r[0], i, 0))).collect();
    while let Some(Reverse((packed, run, at))) = heap.pop() {
        if out.len().is_multiple_of(1 << 20) && ctl.cancel.is_cancelled() {
            return Err(SearchError::Superseded);
        }
        out.push(packed as u32);
        if let Some(&next) = runs[run].get(at + 1) {
            heap.push(Reverse((next, run, at + 1)));
        }
    }
    drop(runs);
    hold.shrink(out.capacity() * 4);
    Ok(Held::new(out, hold))
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
