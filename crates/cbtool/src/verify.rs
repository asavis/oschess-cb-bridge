//! `cbtool verify`: decode and replay every record of a 2CBH or classic
//! database, counting what the reader meets. A PGN file is verified in
//! [`crate::pgn_file`].
//!
//! Both formats run [`verify_record`]: the record, its kind, its annotations
//! and every failure are read and counted alike, and each format counts its
//! moves its own way ([`Records`]).

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use cbformat::game::{GameAnnotations, Head, RecordKind, Start};
use cbformat::movetable::{self, Captured, MoveWord};
use cbformat::replay::{TreeStats, walk_tree};
use cbformat::v2::{self, Token};
use cbformat::view::{self, Base};

use crate::{AnyResult, LIMITS, opts, page, pgn_file, run_workers, threads};

/// What `verify` counts.
#[derive(Default)]
pub(crate) struct Stats {
    pub(crate) games: u64,
    pub(crate) texts: u64,
    pub(crate) analyses: u64,
    pub(crate) unknown_kind: u64,
    pub(crate) deleted: u64,
    pub(crate) chess960: u64,
    pub(crate) setups: u64,
    pub(crate) main_plies: u64,
    pub(crate) total_plies: u64,
    pub(crate) null_moves: u64,
    pub(crate) promo_captures: u64,
    pub(crate) promo_captures_distinct: u64,
    pub(crate) en_passant: u64,
    pub(crate) annotated: u64,
    pub(crate) annotations_incomplete: u64,
    /// Games with annotations past their last move, and those annotations.
    pub(crate) past_end_games: u64,
    pub(crate) past_end: u64,
    pub(crate) failures: u64,
}

impl Stats {
    fn add(&mut self, o: &Stats) {
        self.games += o.games;
        self.texts += o.texts;
        self.analyses += o.analyses;
        self.unknown_kind += o.unknown_kind;
        self.deleted += o.deleted;
        self.chess960 += o.chess960;
        self.setups += o.setups;
        self.main_plies += o.main_plies;
        self.total_plies += o.total_plies;
        self.null_moves += o.null_moves;
        self.promo_captures += o.promo_captures;
        self.promo_captures_distinct += o.promo_captures_distinct;
        self.en_passant += o.en_passant;
        self.annotated += o.annotated;
        self.annotations_incomplete += o.annotations_incomplete;
        self.past_end_games += o.past_end_games;
        self.past_end += o.past_end;
        self.failures += o.failures;
    }

    /// Counts how a game starts: in Chess960, and from a set-up position.
    pub(crate) fn count_start(&mut self, chess960: bool, start: cbformat::Result<Start>) {
        if chess960 {
            self.chess960 += 1;
        }
        if matches!(start, Ok(Start::Setup(_))) {
            self.setups += 1;
        }
    }

    /// Counts a game with `n` annotations past its last move, when it has any.
    fn count_past_end(&mut self, n: usize) {
        if n > 0 {
            self.past_end_games += 1;
            self.past_end += n as u64;
        }
    }
}

/// Failures kept for the report: the first of a run, whichever worker met
/// them.
const KEPT_FAILURES: usize = 50;

/// The failures of a run, shared by its workers: each is counted in the
/// [`Stats`] of the worker that met it, and the first [`KEPT_FAILURES`] are
/// kept for the report.
#[derive(Default)]
struct Failures(Mutex<Vec<(u32, String)>>);

impl Failures {
    fn add(&self, s: &mut Stats, id: u32, msg: String) {
        s.failures += 1;
        let mut f = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if f.len() < KEPT_FAILURES {
            f.push((id, msg));
        }
    }

    /// The failures kept, in the order of their records.
    fn into_sorted(self) -> Vec<(u32, String)> {
        let mut f = self.0.into_inner().unwrap_or_else(|e| e.into_inner());
        f.sort();
        f
    }
}

/// The records of a batch as `verify` reads them: what each format does its
/// own way.
pub(crate) trait Records {
    type Record: Head;

    fn record(&self, id: u32) -> cbformat::Result<Self::Record>;

    /// Decodes the moves of `r` within [`LIMITS`], counts its start and its
    /// moves into `s`, and replays every line.
    fn moves(&self, r: &Self::Record, s: &mut Stats) -> cbformat::Result<TreeStats>;

    /// The annotations of `r`, within [`LIMITS`].
    fn annotations(&self, r: &Self::Record) -> cbformat::Result<Option<GameAnnotations>>;
}

/// 2CBH counts its moves from their words, whose captured piece is stored.
impl Records for v2::Batch<'_> {
    type Record = v2::Record;

    fn record(&self, id: u32) -> cbformat::Result<v2::Record> {
        v2::Batch::record(self, id)
    }

    fn moves(&self, r: &v2::Record, s: &mut Stats) -> cbformat::Result<TreeStats> {
        let data = self.moves_of_within(r, LIMITS.game_bytes)?;
        let moves = data.moves()?;
        s.count_start(moves.is_chess960(), moves.start());
        for t in moves.tokens() {
            if let Token::Move(w) = t {
                match movetable::decode(w) {
                    Some(MoveWord::Null) => s.null_moves += 1,
                    Some(MoveWord::Normal { captured: Captured::EnPassant, .. }) => s.en_passant += 1,
                    Some(MoveWord::Normal { captured, promotion: Some(p), .. }) if captured != Captured::Nothing => {
                        s.promo_captures += 1;
                        if !same_kind(captured, p) {
                            s.promo_captures_distinct += 1;
                        }
                    }
                    _ => {}
                }
            }
        }
        walk_tree(&moves, |_, _, _| {})
    }

    fn annotations(&self, r: &v2::Record) -> cbformat::Result<Option<GameAnnotations>> {
        self.annotations_of_within(r, LIMITS.game_bytes)
    }
}

fn same_kind(captured: Captured, promoted: cbformat::movetable::Piece) -> bool {
    use cbformat::movetable::Piece;
    matches!(
        (captured, promoted),
        (Captured::Queen, Piece::Queen)
            | (Captured::Knight, Piece::Knight)
            | (Captured::Bishop, Piece::Bishop)
            | (Captured::Rook, Piece::Rook)
    )
}

/// Decodes and replays one record of either format, adding to `s`; its
/// failures are counted there and kept in `failures`.
fn verify_record(batch: &impl Records, id: u32, s: &mut Stats, failures: &Failures) {
    let fail = |s: &mut Stats, msg: String| failures.add(s, id, msg);
    let r = match batch.record(id) {
        Ok(r) => r,
        Err(e) => return fail(s, e.to_string()),
    };
    if r.is_deleted() {
        s.deleted += 1;
    }
    match r.kind() {
        RecordKind::Game => s.games += 1,
        RecordKind::Analysis => s.analyses += 1,
        RecordKind::Text => {
            s.texts += 1;
            return;
        }
        RecordKind::Unknown(_) => {
            s.unknown_kind += 1;
            return;
        }
    }
    let plies = match batch.moves(&r, s) {
        Ok(t) => {
            s.main_plies += u64::from(t.main_line_plies);
            s.total_plies += u64::from(t.total_plies);
            t.total_plies
        }
        Err(e) => return fail(s, e.to_string()),
    };
    // Only a 2CBH record is ever left incomplete: every annotation type of the
    // classic format has its size, so a classic record decodes, or it is
    // damaged.
    match batch.annotations(&r) {
        Ok(Some(a)) if !a.is_empty() => {
            s.annotated += 1;
            if let Err(e) = a.check_positions(plies).map(|n| s.count_past_end(n)) {
                fail(s, format!("annotations: {e}"));
            } else if let Some(u) = a.stopped_at {
                s.annotations_incomplete += 1;
                fail(s, format!("annotations: type {:#04x} of unknown layout at position {}", u.type_code, u.position));
            }
        }
        Ok(_) => {}
        Err(e) => fail(s, format!("annotations: {e}")),
    }
}

pub(crate) fn verify(path: &str, rest: &[String]) -> AnyResult<bool> {
    let format = view::format_of(Path::new(path));
    let o = opts(rest, true, format == view::Format::Pgn)?;
    if format == view::Format::Pgn {
        return pgn_file::verify(path, page(&o), o.limit);
    }
    let db = Base::open(path)?;
    let n = o.limit.map_or(db.record_count(), |l| l.min(db.record_count()));
    let failures = Failures::default();
    let started = Instant::now();
    // Workers claim runs of ids from a shared counter and fold their own
    // statistics, merged once at the end.
    let next_run = AtomicU64::new(0);
    let total = Mutex::new(Stats::default());
    let runs = u64::from(n).div_ceil(u64::from(RUN));
    if runs > 0 {
        run_workers(threads().min(runs as usize), &|| {
            let mut s = Stats::default();
            while let Some(ids) = run_ids(next_run.fetch_add(1, Ordering::Relaxed), n) {
                // A run is read in two large reads; if that fails, its records
                // are read one by one and report their own errors.
                let Ok(batch) = db.batch(*ids.start(), *ids.end()).or_else(|_| db.batch(1, 0)) else { continue };
                for id in ids {
                    match &batch {
                        view::Batch::TwoCbh(_, batch) => verify_record(batch, id, &mut s, &failures),
                        view::Batch::Cbh(_, batch) => verify_record(batch, id, &mut s, &failures),
                        view::Batch::Pgn(..) => {}
                    }
                }
            }
            total.lock().unwrap_or_else(|e| e.into_inner()).add(&s);
        });
    }
    let stats = total.into_inner().unwrap_or_else(|e| e.into_inner());
    Ok(report(n, started, &stats, failures, Some(db.has_annotations())))
}

/// Prints the statistics of a `verify` run and its first failures; whether
/// every record passed. `annotations` says whether the database has an
/// annotation file, and is `None` when the run does not read annotations.
fn report(n: u32, started: Instant, stats: &Stats, failures: Failures, annotations: Option<bool>) -> bool {
    let secs = started.elapsed().as_secs_f64();
    println!("records verified   {n} in {secs:.1} s ({:.0} records/s)", n as f64 / secs);
    println!("games              {}", stats.games);
    println!("analyses           {}", stats.analyses);
    println!("guiding texts      {}", stats.texts);
    println!("unknown kind       {}", stats.unknown_kind);
    println!("deleted            {}", stats.deleted);
    println!("chess960           {}", stats.chess960);
    println!("set-up starts      {}", stats.setups);
    println!("main-line plies    {}", stats.main_plies);
    println!("all plies          {}", stats.total_plies);
    println!("null moves         {}", stats.null_moves);
    println!("en passant         {}", stats.en_passant);
    println!(
        "promotion captures {} ({} with captured != promoted piece)",
        stats.promo_captures, stats.promo_captures_distinct
    );
    match annotations {
        Some(true) => {
            println!("annotated          {}", stats.annotated);
            println!("  incomplete       {}", stats.annotations_incomplete);
            println!(
                "  past the end     {} ({} annotations moved to the last move)",
                stats.past_end_games, stats.past_end
            );
        }
        Some(false) => println!("annotated          no annotation file"),
        None => {}
    }
    println!("failures           {}", stats.failures);
    for (id, msg) in &failures.into_sorted() {
        println!("  game {id}: {msg}");
    }
    stats.failures == 0
}

/// Ids per run claimed by a `verify` worker.
const RUN: u32 = 4_096;

/// The ids of run `k` over `1..=n`, or `None` past the end. In 64 bits, so
/// the last run ends at `n` even when `n` is `u32::MAX`.
fn run_ids(k: u64, n: u32) -> Option<std::ops::RangeInclusive<u32>> {
    let first = k.checked_mul(u64::from(RUN))?.checked_add(1)?;
    if first > u64::from(n) {
        return None;
    }
    let last = (first + u64::from(RUN) - 1).min(u64::from(n));
    Some(first as u32..=last as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_cover_every_id_up_to_the_largest_count() {
        assert_eq!(run_ids(0, 0), None);
        assert_eq!(run_ids(0, 1), Some(1..=1));
        assert_eq!(run_ids(0, RUN), Some(1..=RUN));
        assert_eq!(run_ids(1, RUN), None);
        assert_eq!(run_ids(1, RUN + 1), Some(RUN + 1..=RUN + 1));
        let last = u64::from(u32::MAX - 1) / u64::from(RUN);
        assert_eq!(run_ids(last, u32::MAX).map(|r| *r.end()), Some(u32::MAX));
        assert_eq!(run_ids(last + 1, u32::MAX), None);
        assert_eq!(run_ids(u64::MAX, u32::MAX), None);
    }

    #[test]
    fn every_failure_is_counted_and_the_first_are_kept_in_record_order() {
        let failures = Failures::default();
        let mut s = Stats::default();
        for id in (1..=KEPT_FAILURES as u32 + 10).rev() {
            failures.add(&mut s, id, format!("bad {id}"));
        }
        assert_eq!(s.failures, KEPT_FAILURES as u64 + 10);
        let kept = failures.into_sorted();
        assert_eq!(kept.len(), KEPT_FAILURES);
        assert_eq!(kept.first(), Some(&(11, "bad 11".to_string())));
        assert_eq!(kept.last(), Some(&(KEPT_FAILURES as u32 + 10, format!("bad {}", KEPT_FAILURES + 10))));
    }
}
