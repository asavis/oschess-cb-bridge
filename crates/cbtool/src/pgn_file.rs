//! `info` and `verify` for a PGN file: its index built in a temporary file,
//! then every game's header read, and its main line played as written.

use std::path::{Path, PathBuf};
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
    let pgn = Path::new(path);
    let index = std::env::temp_dir().join(format!("cbtool-{}.head", std::process::id()));
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

pub fn verify(path: &str, page: CodePage) -> AnyResult<bool> {
    let started = Instant::now();
    let b = build(path, page)?;
    let db = &b.db;
    let (mut games, mut chess960, mut variants, mut setups, mut bad_starts) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let (mut plies, mut nulls, mut unplayable, mut legacy, mut unreadable) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut first_unplayable: Vec<(u32, String)> = Vec::new();
    let mut lexer = Lexer::new();
    for id in 1..=db.record_count() {
        let r = db.record(id)?;
        games += 1;
        if r.has_setup() {
            setups += 1;
        }
        if r.is_chess960() {
            chess960 += 1;
            continue;
        }
        if r.is_other_variant() {
            variants += 1;
            continue;
        }
        let mut bytes = vec![0u8; r.len() as usize];
        if r.len() as usize > MAX_TEXT || db.read_span(r.offset(), &mut bytes).is_err() {
            unreadable += 1;
            continue;
        }
        if std::str::from_utf8(&bytes).is_err() {
            legacy += 1;
        }
        let mut line_plies = 0u64;
        let end = main_line(&bytes, &mut lexer, &mut |_, mv| {
            line_plies += u64::from(mv.is_some());
            true
        });
        plies += line_plies;
        match end {
            LineEnd::NullMove => nulls += 1,
            LineEnd::BadStart => bad_starts += 1,
            LineEnd::Unplayable(text) => {
                unplayable += 1;
                if first_unplayable.len() < 10 {
                    first_unplayable.push((id, String::from_utf8_lossy(&text).into_owned()));
                }
            }
            LineEnd::End | LineEnd::Stopped => {}
        }
    }
    println!("games               {games}");
    println!("chess960            {chess960}");
    println!("other variants      {variants}");
    println!("set-up positions    {setups}, unreadable {bad_starts}");
    println!("main-line plies     {plies}");
    println!("null moves          {nulls}");
    println!("not UTF-8           {legacy} (read as code page {})", page.number());
    println!("unplayable moves    {unplayable}");
    for (id, text) in &first_unplayable {
        println!("  game {id}: {text:?}");
    }
    println!("unreadable texts    {unreadable}");
    println!("index built in {:.1} s, verified in {:.1} s", b.seconds, started.elapsed().as_secs_f64());
    Ok(unreadable == 0)
}
