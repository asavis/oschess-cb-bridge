//! `cbtool`: inspect, verify and export ChessBase 2CBH databases.

use std::io::{BufWriter, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Mutex;
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
    let sink: Box<dyn Write> = match out_path {
        Some(p) => {
            refuse_database_file(Path::new(&p), &db)?;
            Box::new(std::fs::File::create(p)?)
        }
        None => Box::new(std::io::stdout().lock()),
    };
    let mut w = BufWriter::new(sink);
    let mut ok = true;
    for id in ids {
        match cbformat::pgn::game(&db, id) {
            Ok(text) => writeln!(w, "{text}")?,
            Err(e) => {
                eprintln!("game {id}: {e}");
                ok = false;
            }
        }
    }
    w.flush()?;
    Ok(ok)
}
