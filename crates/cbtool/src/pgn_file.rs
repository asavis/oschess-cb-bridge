//! `info` and `verify` for a PGN file: its index built in a temporary file,
//! then every game's header read, and its main line played as written.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use cbformat::codepage::CodePage;
use cbformat::pgnfile::lex::Lexer;
use cbformat::pgnfile::line::{LineEnd, main_line};
use cbformat::pgnfile::{self, Database, MAX_TEXT};

use crate::AnyResult;

/// The index of `path` in a temporary file, removed on drop.
struct Built {
    db: Database,
    index: PathBuf,
    seconds: f64,
}

impl Drop for Built {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.index);
    }
}

fn build(path: &str, page: CodePage) -> AnyResult<Built> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let pgn = Path::new(path);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let index = std::env::temp_dir().join(format!("cbtool-{}-{n}.head", std::process::id()));
    let started = Instant::now();
    pgnfile::build(pgn, &index, 0, page, &mut |_| true)?;
    let seconds = started.elapsed().as_secs_f64();
    let db = Database::open(pgn, &index, 0, page)?;
    Ok(Built { db, index, seconds })
}

/// `--code-page N` among `rest`: the page of text that is not UTF-8.
pub fn code_page(rest: &[String]) -> AnyResult<CodePage> {
    match rest.iter().position(|a| a == "--code-page") {
        None => Ok(CodePage::WESTERN),
        Some(i) => {
            let n: u32 = rest.get(i + 1).ok_or("--code-page needs a number")?.parse()?;
            Ok(CodePage::new(n))
        }
    }
}

pub fn info(path: &str, page: CodePage) -> AnyResult<bool> {
    let b = build(path, page)?;
    let bytes = std::fs::metadata(path)?.len();
    println!("records        {}", b.db.record_count());
    println!("players        {}", b.db.players());
    println!("tournaments    {}", b.db.tournaments());
    println!("annotators     {}", b.db.annotators());
    println!("file bytes     {bytes}");
    println!("index bytes    {}", std::fs::metadata(&b.index)?.len());
    println!("index built in {:.1} s", b.seconds);
    Ok(true)
}

/// What `verify` counts.
#[derive(Debug, Default)]
struct Report {
    games: u64,
    chess960: u64,
    variants: u64,
    setups: u64,
    bad_starts: u64,
    plies: u64,
    nulls: u64,
    unplayable: u64,
    legacy: u64,
    unreadable: u64,
    first_unplayable: Vec<(u32, String)>,
    index_seconds: f64,
}

impl Report {
    /// Whether every game verified: read, set up and played to its end.
    /// Chess960 games and other variants are left out, and a null move ends
    /// a main line as it may.
    fn verified(&self) -> bool {
        self.unreadable == 0 && self.unplayable == 0 && self.bad_starts == 0
    }
}

fn check(path: &str, page: CodePage) -> AnyResult<Report> {
    let b = build(path, page)?;
    let db = &b.db;
    let mut report = Report { index_seconds: b.seconds, ..Report::default() };
    let mut lexer = Lexer::new();
    for id in 1..=db.record_count() {
        let r = db.record(id)?;
        report.games += 1;
        report.setups += u64::from(r.has_setup());
        if r.is_chess960() {
            report.chess960 += 1;
            continue;
        }
        if r.is_other_variant() {
            report.variants += 1;
            continue;
        }
        // Refused before anything is allocated when over the limit.
        let Ok(bytes) = db.bytes(&r, MAX_TEXT) else {
            report.unreadable += 1;
            continue;
        };
        report.legacy += u64::from(std::str::from_utf8(&bytes).is_err());
        let mut plies = 0u64;
        let end = main_line(&bytes, &mut lexer, &mut |_, mv| {
            plies += u64::from(mv.is_some());
            true
        });
        report.plies += plies;
        match end {
            LineEnd::NullMove => report.nulls += 1,
            LineEnd::BadStart => report.bad_starts += 1,
            LineEnd::Unplayable(text) => {
                report.unplayable += 1;
                if report.first_unplayable.len() < 10 {
                    report.first_unplayable.push((id, String::from_utf8_lossy(&text).into_owned()));
                }
            }
            LineEnd::End | LineEnd::Stopped => {}
        }
    }
    Ok(report)
}

/// Verifies every game; fails when any cannot be read, set up or played.
pub fn verify(path: &str, page: CodePage) -> AnyResult<bool> {
    let started = Instant::now();
    let r = check(path, page)?;
    println!("games               {}", r.games);
    println!("chess960            {}", r.chess960);
    println!("other variants      {}", r.variants);
    println!("set-up positions    {}, unreadable {}", r.setups, r.bad_starts);
    println!("main-line plies     {}", r.plies);
    println!("null moves          {}", r.nulls);
    println!("not UTF-8           {} (read as code page {})", r.legacy, page.number());
    println!("unplayable moves    {}", r.unplayable);
    for (id, text) in &r.first_unplayable {
        println!("  game {id}: {text:?}");
    }
    println!("unreadable texts    {}", r.unreadable);
    println!("index built in {:.1} s, verified in {:.1} s", r.index_seconds, started.elapsed().as_secs_f64());
    Ok(r.verified())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("cbtool-pgn-{}-{name}.pgn", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn a_game_that_does_not_play_fails_the_verification() {
        let path =
            file("fails", b"[FEN \"not a position\"]\n\n1. e4 *\n\n[Event \"x\"]\n\n1. e4 Ke7 *\n\n1. d4 -- *\n");
        let r = check(path.to_str().unwrap(), CodePage::WESTERN).unwrap();
        assert_eq!((r.games, r.bad_starts, r.unplayable, r.nulls), (3, 1, 1, 1));
        assert!(!r.verified());
        let ok = file("passes", b"[Event \"x\"]\n\n1. e4 e5 *\n\n1. d4 -- *\n");
        assert!(check(ok.to_str().unwrap(), CodePage::WESTERN).unwrap().verified());
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(ok);
    }

    #[test]
    fn an_oversized_game_is_unreadable_not_allocated() {
        // One game running past the limit: a comment left open to the end.
        let path = file("oversized", b"[Event \"x\"]\n\n1. e4 {");
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(MAX_TEXT as u64 + 1024).unwrap();
        drop(f);
        let r = check(path.to_str().unwrap(), CodePage::WESTERN).unwrap();
        assert_eq!((r.games, r.unreadable), (1, 1));
        assert!(!r.verified());
        let _ = std::fs::remove_file(path);
    }
}
