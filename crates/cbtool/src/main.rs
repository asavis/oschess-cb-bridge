//! `cbtool`: inspect, verify and export ChessBase 2CBH databases.

use std::io::{BufWriter, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Instant;

use cbformat::movetable::{self, Captured, MoveWord};
use cbformat::replay::walk_tree;
use cbformat::v2::{Database, RecordKind, Start, Token};
use rayon::prelude::*;

const USAGE: &str = "usage:
  cbtool info   <db>
  cbtool verify <db> [--limit N]           decode and replay every game and analysis
  cbtool pgn    <db> [--out FILE] [ID...]  export games as PGN (all games when no ids)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("info") if args.len() == 2 => info(&args[1]),
        Some("verify") if args.len() >= 2 => verify(&args[1], &args[2..]),
        Some("pgn") if args.len() >= 2 => pgn(&args[1], &args[2..]),
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
struct Stats {
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
    fn add(mut self, o: Stats) -> Stats {
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
        self
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

fn verify(path: &str, rest: &[String]) -> AnyResult<bool> {
    let limit = match rest {
        [flag, n] if flag == "--limit" => Some(n.parse::<u32>()?),
        [] => None,
        _ => return Err(USAGE.into()),
    };
    let db = Database::open(path)?;
    let n = limit.map_or(db.record_count(), |l| l.min(db.record_count()));
    let failures: Mutex<Vec<(u32, String)>> = Mutex::new(Vec::new());
    let started = Instant::now();
    let stats = (1..=n)
        .into_par_iter()
        .fold(Stats::default, |mut s, id| {
            let fail = |s: &mut Stats, msg: String| {
                s.failures += 1;
                let mut f = failures.lock().unwrap();
                if f.len() < 50 {
                    f.push((id, msg));
                }
            };
            let r = match db.record(id) {
                Ok(r) => r,
                Err(e) => {
                    fail(&mut s, e.to_string());
                    return s;
                }
            };
            if r.is_deleted() {
                s.deleted += 1;
            }
            match r.kind() {
                RecordKind::Game => s.games += 1,
                RecordKind::Analysis => s.analyses += 1,
                RecordKind::Text => {
                    s.texts += 1;
                    return s;
                }
                RecordKind::Unknown(_) => {
                    s.unknown_kind += 1;
                    return s;
                }
            }
            let moves = match db.moves_of(&r) {
                Ok(m) => m,
                Err(e) => {
                    fail(&mut s, e.to_string());
                    return s;
                }
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
                        Some(MoveWord::Normal { captured, promotion: Some(p), .. })
                            if captured != Captured::Nothing =>
                        {
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
                Err(e) => fail(&mut s, e.to_string()),
            }
            s
        })
        .reduce(Stats::default, Stats::add);
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
    let mut f = failures.into_inner().unwrap();
    f.sort();
    for (id, msg) in &f {
        println!("  game {id}: {msg}");
    }
    Ok(stats.failures == 0)
}

/// Refuses an output path that is one of the database's own files, by name or
/// through any alias such as a hard link: creating it would truncate the input
/// while it is memory-mapped.
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
        ids = (1..=db.record_count()).filter(|&id| db.record(id).is_ok_and(|r| r.kind() == RecordKind::Game)).collect();
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

/// Games per task, and the rendered text a task may hold before its turn to
/// write. A task over the budget waits for its turn and then streams the rest
/// of its games straight to the output, so memory stays near
/// `threads × PGN_TASK_BYTES` however large each game renders.
const PGN_CHUNK: usize = 1_024;
const PGN_TASK_BYTES: usize = 4 << 20;

/// Rendered games waiting for their turn to be written: the text, and each
/// failure with the text offset it occurred at, to keep stderr in game order.
#[derive(Default)]
struct Rendered {
    text: String,
    errors: Vec<(usize, String)>,
}

impl Rendered {
    fn game(&mut self, db: &Database, id: u32) {
        match cbformat::pgn::game(db, id) {
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
fn export_in_order(
    ids: &[u32],
    threads: usize,
    render: &(dyn Fn(u32, &mut Rendered) + Sync),
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
    std::thread::scope(|scope| {
        for _ in 0..threads.clamp(1, chunks.len().max(1)) {
            scope.spawn(|| {
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
        }
    });
    let t = turn.into_inner().unwrap_or_else(|e| e.into_inner());
    match t.failed {
        Some(e) => Err(e.into()),
        None => Ok(t.ok),
    }
}

/// Worker threads: `CBTOOL_THREADS` if set, otherwise one per CPU.
fn threads() -> usize {
    std::env::var("CBTOOL_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
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

    fn fake(id: u32, r: &mut Rendered) {
        r.text.push_str(&format!("game {id}\n"));
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
