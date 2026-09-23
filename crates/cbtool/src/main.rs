//! `cbtool`: inspect, verify and export ChessBase 2CBH databases; inspect
//! and verify classic CBH ones.

use std::io::{BufWriter, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Instant;

mod databases;

use cbformat::movetable::{self, Captured, MoveWord};
use cbformat::replay::walk_tree;
use cbformat::v2::{Batch, Database, RecordKind, Start, Token};

mod classic;

const USAGE: &str = "usage:
  cbtool info   <db>
  cbtool verify <db> [--limit N]           decode and replay every game and analysis
                                           (<db> may be a classic .cbh database)
  cbtool pgn    <db> [--out FILE] [ID...]  export games as PGN (all games when no ids)
  cbtool databases <dir>                   the databases ChessBase's database window lists
                                           (dir: the ChessBase documents folder)

CBTOOL_THREADS sets the number of worker threads (default: one per CPU).";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("info") if args.len() == 2 => info(&args[1]),
        Some("verify") if args.len() >= 2 => verify(&args[1], &args[2..]),
        Some("pgn") if args.len() >= 2 => pgn(&args[1], &args[2..]),
        Some("databases") if args.len() == 2 => databases::databases(&args[1]),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;

fn info(path: &str) -> AnyResult<bool> {
    if classic::is_classic(path) {
        return classic::info(path);
    }
    let db = Database::open(path)?;
    println!("records        {}", db.record_count());
    println!("format version {}", db.format_version());
    let e = db.entities();
    for (i, name) in ["players", "tournaments", "sources", "type 3", "teams", "game tags"].iter().enumerate() {
        println!("{name:<14} {}", e.count(i));
    }
    Ok(true)
}

#[derive(Default)]
pub(crate) struct Stats {
    games: u64,
    texts: u64,
    analyses: u64,
    unknown_kind: u64,
    deleted: u64,
    chess960: u64,
    setups: u64,
    main_plies: u64,
    total_plies: u64,
    null_moves: u64,
    promo_captures: u64,
    promo_captures_distinct: u64,
    en_passant: u64,
    failures: u64,
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
        self.failures += o.failures;
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

/// Decodes and replays one record, adding to `s`; the first 50 failures of
/// the whole run are kept in `failures`.
fn verify_record(batch: &Batch<'_>, id: u32, s: &mut Stats, failures: &Mutex<Vec<(u32, String)>>) {
    let fail = |s: &mut Stats, msg: String| {
        s.failures += 1;
        let mut f = failures.lock().unwrap_or_else(|e| e.into_inner());
        if f.len() < 50 {
            f.push((id, msg));
        }
    };
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
    let data = match batch.moves_of(&r) {
        Ok(d) => d,
        Err(e) => return fail(s, e.to_string()),
    };
    let moves = match data.moves() {
        Ok(m) => m,
        Err(e) => return fail(s, e.to_string()),
    };
    if moves.is_chess960() {
        s.chess960 += 1;
    }
    if matches!(moves.start(), Ok(Start::Setup(_))) {
        s.setups += 1;
    }
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
    match walk_tree(&moves, |_, _, _| {}) {
        Ok(t) => {
            s.main_plies += t.main_line_plies as u64;
            s.total_plies += t.total_plies as u64;
        }
        Err(e) => fail(s, e.to_string()),
    }
}

fn verify(path: &str, rest: &[String]) -> AnyResult<bool> {
    let limit = match rest {
        [flag, n] if flag == "--limit" => Some(n.parse::<u32>()?),
        [] => None,
        _ => return Err(USAGE.into()),
    };
    if classic::is_classic(path) {
        return classic::verify(path, limit);
    }
    let db = Database::open(path)?;
    let n = limit.map_or(db.record_count(), |l| l.min(db.record_count()));
    let failures: Mutex<Vec<(u32, String)>> = Mutex::new(Vec::new());
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
                    verify_record(&batch, id, &mut s, &failures);
                }
            }
            total.lock().unwrap_or_else(|e| e.into_inner()).add(&s);
        });
    }
    let stats = total.into_inner().unwrap_or_else(|e| e.into_inner());
    Ok(report(n, started, &stats, failures))
}

/// Prints the statistics of a `verify` run and its first failures; whether
/// every record passed.
fn report(n: u32, started: Instant, stats: &Stats, failures: Mutex<Vec<(u32, String)>>) -> bool {
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
    println!("failures           {}", stats.failures);
    let mut f = failures.into_inner().unwrap_or_else(|e| e.into_inner());
    f.sort();
    for (id, msg) in &f {
        println!("  game {id}: {msg}");
    }
    stats.failures == 0
}

/// Refuses an output path that is one of the database's own files, by name or
/// through any alias such as a hard link: creating it would truncate the input
/// while it is being read.
fn refuse_database_file(out: &Path, db: &Database) -> AnyResult<()> {
    if !out.exists() {
        return Ok(());
    }
    for input in db.file_paths() {
        if input.exists() && same_file::is_same_file(out, &input)? {
            return Err(format!("refusing to overwrite {}, a file of the database being read", out.display()).into());
        }
    }
    Ok(())
}

fn pgn(path: &str, rest: &[String]) -> AnyResult<bool> {
    let db = Database::open(path)?;
    let mut out_path = None;
    let mut ids = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--out" {
            out_path = Some(it.next().ok_or(USAGE)?.clone());
        } else {
            ids.push(a.parse::<u32>()?);
        }
    }
    if ids.is_empty() {
        ids = game_ids(&db)?;
    }
    let sink: Box<dyn Write + Send> = match out_path {
        Some(p) => {
            refuse_database_file(Path::new(&p), &db)?;
            Box::new(std::fs::File::create(p)?)
        }
        None => Box::new(std::io::stdout()),
    };
    let mut w = BufWriter::with_capacity(1 << 20, sink);
    let ok = export_in_order(&ids, threads(), &|id, r| r.game(&db, id), &mut w)?;
    w.flush()?;
    Ok(ok)
}

/// The id of every game, from headers read [`cbformat::v2::MAX_BATCH_RECORDS`]
/// at a time. A failed read fails the export: a database damaged or truncated
/// under it must not give a silently incomplete one.
fn game_ids(db: &Database) -> cbformat::Result<Vec<u32>> {
    let mut ids = Vec::new();
    let mut first = 1;
    loop {
        let records = db.records(first, db.record_count())?;
        let Some(last) = records.last() else { break };
        ids.extend(records.iter().filter(|r| r.kind() == RecordKind::Game).map(|r| r.id()));
        let Some(next) = last.id().checked_add(1) else { break };
        first = next;
    }
    Ok(ids)
}

/// Games per task, and the rendered text a task may hold before its turn to
/// write. A task over the budget waits for its turn and then streams the rest
/// of its games straight to the output, so memory stays near
/// `threads × PGN_TASK_BYTES` however large each game renders.
const PGN_CHUNK: usize = 1_024;
const PGN_TASK_BYTES: usize = 4 << 20;

/// Rendered games waiting for their turn to be written: the text, and each
/// failure with the text offset it occurred at, to keep stderr in game order.
#[derive(Default)]
struct Rendered<'db> {
    text: String,
    errors: Vec<(usize, String)>,
    /// The records around the last one rendered, read together.
    batch: Option<Batch<'db>>,
}

/// Records read together when rendering; ids outside the batch start a new one.
const RENDER_BATCH: u32 = 1_024;

impl<'db> Rendered<'db> {
    fn game(&mut self, db: &'db Database, id: u32) {
        if !self.batch.as_ref().is_some_and(|b| b.ids().contains(&id)) {
            self.batch = db.batch(id, id.saturating_add(RENDER_BATCH - 1)).ok();
        }
        let rendered = match &self.batch {
            Some(batch) => batch.record(id).and_then(|r| {
                if r.kind() != RecordKind::Game {
                    return cbformat::pgn::game(db, id);
                }
                let data = batch.moves_of(&r)?;
                cbformat::pgn::game_from(db, &r, &data.moves()?)
            }),
            None => cbformat::pgn::game(db, id),
        };
        match rendered {
            Ok(game) => {
                self.text.push_str(&game);
                self.text.push('\n');
            }
            Err(e) => self.errors.push((self.text.len(), format!("game {id}: {e}"))),
        }
    }

    fn write_to(&mut self, out: &mut dyn Write) -> std::io::Result<()> {
        let mut at = 0;
        for (offset, error) in &self.errors {
            out.write_all(&self.text.as_bytes()[at..*offset])?;
            eprintln!("{error}");
            at = *offset;
        }
        out.write_all(&self.text.as_bytes()[at..])?;
        self.text.clear();
        self.errors.clear();
        Ok(())
    }
}

/// Renders `ids` with `render` on `threads` workers and writes them in the
/// order given. Returns whether every game rendered, with failures reported on
/// stderr in the same order. A write error stops every worker: no chunk is
/// claimed and no game rendered after it, and waiting workers are woken.
fn export_in_order<'db>(
    ids: &[u32],
    threads: usize,
    render: &(dyn Fn(u32, &mut Rendered<'db>) + Sync),
    out: &mut (dyn Write + Send),
) -> AnyResult<bool> {
    struct Turn<'w> {
        next: usize,
        out: &'w mut (dyn Write + Send),
        ok: bool,
        failed: Option<std::io::Error>,
    }
    let chunks: Vec<&[u32]> = ids.chunks(PGN_CHUNK).collect();
    let claimed = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let turn = Mutex::new(Turn { next: 0, out, ok: true, failed: None });
    let your_turn = Condvar::new();
    run_workers(threads.min(chunks.len()), &|| {
        let mut r = Rendered::default();
        while !stop.load(Ordering::Relaxed) {
            let k = claimed.fetch_add(1, Ordering::Relaxed);
            let Some(chunk) = chunks.get(k) else { break };
            let mut done = 0;
            while done < chunk.len() && r.text.len() < PGN_TASK_BYTES && !stop.load(Ordering::Relaxed) {
                render(chunk[done], &mut r);
                done += 1;
            }
            let mut t = turn.lock().unwrap_or_else(|e| e.into_inner());
            while t.next != k && t.failed.is_none() {
                t = your_turn.wait(t).unwrap_or_else(|e| e.into_inner());
            }
            if t.failed.is_some() {
                break;
            }
            // Our turn: write what is held, then stream the rest.
            let mut chunk_ok = r.errors.is_empty();
            let mut result = r.write_to(&mut *t.out);
            for &id in &chunk[done..] {
                if result.is_err() {
                    break;
                }
                render(id, &mut r);
                chunk_ok &= r.errors.is_empty();
                result = r.write_to(&mut *t.out);
            }
            t.ok &= chunk_ok;
            match result {
                Ok(()) => t.next += 1,
                Err(e) => {
                    t.failed = Some(e);
                    stop.store(true, Ordering::Relaxed);
                }
            }
            your_turn.notify_all();
        }
    });
    let t = turn.into_inner().unwrap_or_else(|e| e.into_inner());
    match t.failed {
        Some(e) => Err(e.into()),
        None => Ok(t.ok),
    }
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

/// Most worker threads started, whatever `CBTOOL_THREADS` asks for.
const MAX_THREADS: usize = 256;

/// Worker threads: `CBTOOL_THREADS` if set, otherwise one per CPU; at most
/// [`MAX_THREADS`].
fn threads() -> usize {
    thread_count(std::env::var("CBTOOL_THREADS").ok().as_deref(), std::thread::available_parallelism().ok())
}

fn thread_count(setting: Option<&str>, cpus: Option<std::num::NonZeroUsize>) -> usize {
    setting
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| cpus.map_or(1, |n| n.get()))
        .min(MAX_THREADS)
}

/// Runs `work` on up to `count` scoped threads. Every worker runs the same
/// loop and takes its work from shared state, so a thread that cannot be
/// started only costs parallelism; if none can, `work` runs on the calling
/// thread.
fn run_workers(count: usize, work: &(dyn Fn() + Sync)) {
    std::thread::scope(|scope| {
        let mut started = 0;
        for _ in 0..count.max(1) {
            if std::thread::Builder::new().spawn_scoped(scope, work).is_err() {
                break;
            }
            started += 1;
        }
        if started == 0 {
            work();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink that accepts `left` bytes and then fails every write.
    struct FailAfter {
        left: usize,
    }

    impl Write for FailAfter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.left == 0 {
                return Err(std::io::Error::other("sink full"));
            }
            let n = buf.len().min(self.left);
            self.left -= n;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn fake(id: u32, r: &mut Rendered<'_>) {
        r.text.push_str(&format!("game {id}\n"));
    }

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
    fn thread_count_is_bounded() {
        let cpus = std::num::NonZeroUsize::new(8);
        assert_eq!(thread_count(None, cpus), 8);
        assert_eq!(thread_count(Some("3"), cpus), 3);
        assert_eq!(thread_count(Some("0"), cpus), 8);
        assert_eq!(thread_count(Some("many"), cpus), 8);
        assert_eq!(thread_count(Some("18446744073709551615"), cpus), MAX_THREADS);
        assert_eq!(thread_count(None, None), 1);
    }

    #[test]
    fn workers_run_even_with_no_count() {
        let runs = AtomicUsize::new(0);
        run_workers(0, &|| {
            runs.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(runs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn writes_in_order() {
        let ids: Vec<u32> = (0..5_000).collect();
        let mut out = Vec::new();
        assert!(export_in_order(&ids, 4, &fake, &mut out).unwrap());
        let want: String = ids.iter().map(|id| format!("game {id}\n")).collect();
        assert_eq!(String::from_utf8(out).unwrap(), want);
    }

    #[test]
    fn a_write_error_stops_the_workers() {
        let ids: Vec<u32> = (0..1_000_000).collect();
        let rendered = AtomicUsize::new(0);
        let render = |id: u32, r: &mut Rendered| {
            rendered.fetch_add(1, Ordering::Relaxed);
            fake(id, r);
        };
        let threads = 4;
        let err = export_in_order(&ids, threads, &render, &mut FailAfter { left: 100 }).unwrap_err();
        assert!(err.to_string().contains("sink full"), "{err}");
        // At most the chunks already claimed when the write failed.
        let bound = (threads + 1) * PGN_CHUNK;
        let n = rendered.load(Ordering::Relaxed);
        assert!(n <= bound, "rendered {n} games after the output failed; bound {bound}");
    }
}
