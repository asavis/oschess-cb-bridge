//! Checks that the full PGN form (asavis/oschess-cb-bridge#42) loses nothing:
//! for every annotated game of each database, of either format, the texts and
//! annotations of each type the decoder reads are counted, and so are the
//! comments and commands of the game's full form. Prints counts only.
//!
//! `cargo run --release -p cbformat --example full_form_census -- <database>…`

use std::collections::BTreeMap;

use cbformat::pgn::Options;
use cbformat::v2::{Annotation, RecordKind};
use cbformat::view::Base;

/// What a full form holds, by kind: `text`, a type code in hex, or `rest`.
fn written(pgn: &str) -> BTreeMap<String, u64> {
    let mut out: BTreeMap<String, u64> = BTreeMap::new();
    let mut add = |k: &str, n: usize| {
        if n > 0 {
            *out.entry(k.to_string()).or_default() += n as u64;
        }
    };
    add("text", pgn.matches("{[%lang ").count());
    let named = [
        ("mdl", "22"),
        ("cbquote", "13"),
        ("cbcritical", "18"),
        ("cbpawns", "14"),
        ("cbpath", "15"),
        ("cbcolour", "23"),
        ("cblink", "1c"),
        ("cbvideo", "20"),
        ("cbtraining", "09"),
    ];
    for (name, code) in named {
        add(code, pgn.matches(&format!("[%{name} ")).count());
    }
    for part in pgn.split("[%cbraw type=").skip(1) {
        add(part.split(';').next().unwrap_or("?"), 1);
    }
    add("rest", pgn.matches("[%cbrest ").count());
    out
}

fn main() {
    let mut totals: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let (mut games, mut mismatched, mut errors) = (0u64, 0u64, 0u64);
    for path in std::env::args().skip(1) {
        let Ok(base) = Base::open(&path) else {
            errors += 1;
            continue;
        };
        let full = Options { full: true, ..Options::default() };
        for id in 1..=base.record_count() {
            let Ok(h) = base.header(id) else { continue };
            if h.kind() != RecordKind::Game {
                continue;
            }
            let Ok(Some(a)) = base.annotations_of(&h) else { continue };
            if a.is_empty() {
                continue;
            }
            let mut decoded: BTreeMap<String, u64> = BTreeMap::new();
            for an in a.blocks.iter().flat_map(|b| &b.annotations) {
                let key = match an {
                    Annotation::Text { text, .. } if text.chars().any(|c| !c.is_whitespace() && !c.is_control()) => {
                        "text".to_string()
                    }
                    Annotation::Other { code, .. } => format!("{code:02x}"),
                    _ => continue,
                };
                *decoded.entry(key).or_default() += 1;
            }
            if a.stopped_at.is_some() {
                *decoded.entry("rest".into()).or_default() += 1;
            }
            let Ok(r) = base.pgn(id, &full) else {
                errors += 1;
                continue;
            };
            games += 1;
            let got = written(&r.pgn);
            if got != decoded {
                mismatched += 1;
            }
            for (k, n) in &decoded {
                totals.entry(k.clone()).or_default().0 += n;
            }
            for (k, n) in &got {
                totals.entry(k.clone()).or_default().1 += n;
            }
        }
    }
    println!("annotated games {games}, games whose full form differs from the census {mismatched}, errors {errors}");
    println!("kind: decoded / written");
    for (k, (d, w)) in &totals {
        println!("  {k:<5} {d:>9} / {w:>9}{}", if d == w { "" } else { "   DIFFERS" });
    }
}
