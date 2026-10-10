//! Counts what the bridge reads of the guiding texts of every database under
//! a folder (asavis/oschess-cb-bridge#324), without printing any of their
//! contents: texts read and failed, contents by kind, spans by kind, and game
//! and text links resolved or not, with the answer sizes.
//!
//! `cargo run --release -p bridge --example guide_census -- <folder>`

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bridge::store::Store;
use bridge::texts::{contents, resolve};
use cbformat::game::guide::{Body, Span};
use cbformat::game::{Head, RecordKind};
use cbformat::view::Base;

#[derive(Default)]
struct Census {
    databases: BTreeMap<&'static str, u64>,
    texts: u64,
    failed: BTreeMap<String, u64>,
    contents: BTreeMap<&'static str, u64>,
    spans: BTreeMap<&'static str, u64>,
    games: (u64, u64),
    links: (u64, u64),
    largest_answer: usize,
    /// Header reads that failed, which end the database's count early.
    unread_batches: u64,
}

fn databases(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            databases(&p, out);
        } else if p
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| matches!(x.to_ascii_lowercase().as_str(), "cbh" | "2cbh"))
        {
            out.push(p);
        }
    }
}

fn count<S: Store>(db: &S, c: &mut Census) {
    let n = db.record_count();
    let mut first = 1;
    while first <= n {
        let Ok(records) = db.records(first, n) else {
            c.unread_batches += 1;
            break;
        };
        let Some(last) = records.last().map(|r| r.id()) else { break };
        for r in records.iter().filter(|r| r.kind() == RecordKind::Text) {
            c.texts += 1;
            let text = match db.guiding_text(r) {
                Ok(t) => t,
                Err(e) => {
                    // The reason without its numbers, which name records.
                    let reason: String = e.to_string().chars().filter(|ch| !ch.is_ascii_digit()).collect();
                    *c.failed.entry(reason).or_default() += 1;
                    continue;
                }
            };
            for content in &text.contents {
                match &content.body {
                    Body::Html(_) => *c.contents.entry("html").or_default() += 1,
                    Body::Paragraphs(p) => {
                        *c.contents.entry("paragraphs").or_default() += 1;
                        for span in p.iter().flat_map(|p| &p.spans) {
                            let kind = match span {
                                Span::Text { .. } => "text",
                                Span::Diagram { .. } => "diagram",
                                Span::Game(_) => "game",
                                Span::TextLink { .. } => "textLink",
                            };
                            *c.spans.entry(kind).or_default() += 1;
                        }
                    }
                }
            }
            match resolve(db, &text) {
                Ok(resolved) => {
                    let found = |v: &[Option<u32>]| (v.iter().filter(|n| n.is_some()).count() as u64, v.len() as u64);
                    let (g, gn) = found(&resolved.games);
                    let (t, tn) = found(&resolved.texts);
                    c.games = (c.games.0 + g, c.games.1 + gn);
                    c.links = (c.links.0 + t, c.links.1 + tn);
                    c.largest_answer = c.largest_answer.max(contents(&text, &resolved).len());
                }
                Err(_) => *c.failed.entry("links not resolved".into()).or_default() += 1,
            }
        }
        if last == u32::MAX {
            break;
        }
        first = last + 1;
    }
}

fn main() {
    let root = std::env::args().nth(1).expect("a folder");
    let mut paths = Vec::new();
    databases(Path::new(&root), &mut paths);
    let mut c = Census::default();
    for p in paths {
        let Ok(base) = Base::open(&p) else {
            *c.databases.entry("not opened").or_default() += 1;
            continue;
        };
        match &base {
            Base::TwoCbh(db) => {
                *c.databases.entry("2cbh").or_default() += 1;
                count(db, &mut c)
            }
            Base::Cbh(db) => {
                *c.databases.entry("cbh").or_default() += 1;
                count(db, &mut c)
            }
            Base::Pgn(_) => {}
        }
    }
    println!("databases          {:?}", c.databases);
    println!("guiding texts      {}", c.texts);
    println!("failed             {:?}", c.failed);
    println!("contents           {:?}", c.contents);
    println!("spans              {:?}", c.spans);
    println!("game links found   {} of {}", c.games.0, c.games.1);
    println!("text links found   {} of {}", c.links.0, c.links.1);
    println!("largest contents   {} bytes", c.largest_answer);
    println!("header reads failed {}", c.unread_batches);
}
