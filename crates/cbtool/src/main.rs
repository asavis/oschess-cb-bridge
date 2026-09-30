//! `cbtool`: inspect, verify and export ChessBase databases, 2CBH and
//! classic CBH; inspect and verify PGN files as the bridge reads them; run
//! the bridge in a console.

use std::path::Path;
use std::process::ExitCode;

mod databases;

use cbformat::Limits;
use cbformat::view::{self, Base};

mod classic;
mod export;
mod pgn_file;
mod profile;
mod verify;

const USAGE: &str = "usage:
  cbtool info   <db> [--code-page N]
  cbtool verify <db> [--limit N] [--code-page N]
                                           decode and replay every game and analysis
  cbtool pgn    <db> [--out FILE] [--lang LANGS] [ID...]
                                           export games as PGN (all games when no ids)
  cbtool databases <dir>                   the databases ChessBase's database window lists
                                           (dir: the ChessBase documents folder)
  cbtool bridge [--database <path>]... [--show-token] [--new-token]
                                           run the oschess bridge in this console
  cbtool profile <db> --index <dir> [--engine <exe>] [--background]
                                           time the bridge's flows against <db> (#83):
                                           timings and counts only; with --background,
                                           while the bridge builds its index unasked

<db> is a 2CBH (.2cbh) or classic (.cbh) database. info and verify also read a
PGN file (.pgn), as the bridge serves it; --code-page N reads its text that is
not UTF-8 in Windows code page N (default: 1252).

--lang takes ISO 639-1 codes in order of preference, comma-separated, for the
language of comments (default: English, else the first a game has).

CBTOOL_THREADS sets the number of worker threads (default: one per CPU).";

/// What `verify` reads and `pgn` exports of a game: all that its format
/// allows, so that checking or exporting a database takes every record the
/// reader can decode, where a server bounds each game far lower
/// ([`Limits::default`]).
const LIMITS: Limits = Limits::format_max();

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("info") if args.len() >= 2 => info(&args[1], &args[2..]),
        Some("verify") if args.len() >= 2 => verify::verify(&args[1], &args[2..]),
        Some("pgn") if args.len() >= 2 => export::pgn(&args[1], &args[2..]),
        Some("databases") if args.len() == 2 => databases::databases(&args[1]),
        Some("profile") => profile::run(&args[1..]),
        // The bridge `profile` asks, in a process of its own; not in the usage.
        Some("profile-serve") => profile::serve(&args[1..]),
        Some("bridge") => {
            bridge::start::console("cbtool bridge", args[1..].iter().cloned()).map(|()| true).map_err(Into::into)
        }
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

/// The options `info` and `verify` take, each at most once: `--limit N`
/// (`verify` only) and `--code-page N` (a PGN file only). Anything else is an
/// error, before any database is read.
#[derive(Debug, Default, PartialEq)]
struct Opts {
    limit: Option<u32>,
    code_page: Option<u32>,
}

fn opts(rest: &[String], limit: bool, pgn: bool) -> AnyResult<Opts> {
    let mut o = Opts::default();
    let mut args = rest.iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("{flag} needs a number\n\n{USAGE}"))?;
        let n: u32 = value.parse().map_err(|_| format!("{flag} takes a whole number, not {value:?}"))?;
        match flag.as_str() {
            "--limit" if limit && o.limit.is_none() => o.limit = Some(n),
            "--code-page" if pgn && o.code_page.is_none() => o.code_page = Some(n),
            _ => return Err(USAGE.into()),
        }
    }
    Ok(o)
}

fn page(o: &Opts) -> cbformat::codepage::CodePage {
    o.code_page.map_or(cbformat::codepage::CodePage::WESTERN, cbformat::codepage::CodePage::new)
}

fn info(path: &str, rest: &[String]) -> AnyResult<bool> {
    let format = view::format_of(Path::new(path));
    let o = opts(rest, false, format == view::Format::Pgn)?;
    if format == view::Format::Pgn {
        return pgn_file::info(path, page(&o));
    }
    let db = Base::open(path)?;
    println!("records        {}", db.record_count());
    // Each format has entity tables of its own.
    match &db {
        Base::TwoCbh(db) => {
            println!("format version {}", db.format_version());
            let e = db.entities();
            for (i, name) in ["players", "tournaments", "sources", "type 3", "teams", "game tags"].iter().enumerate() {
                println!("{name:<14} {}", e.count(i));
            }
        }
        Base::Cbh(db) => classic::info(db),
        Base::Pgn(_) => {}
    }
    Ok(true)
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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
}
