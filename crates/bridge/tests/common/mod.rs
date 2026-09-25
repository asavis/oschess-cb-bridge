//! What the test files share: the fixture of `docs/search-grammar.md`,
//! written as a 2CBH database and as a classic one with the same content.
//! Each test file uses a part of it.
//!
//! The search memory budget (`search::memory`), the answer budget
//! (`budget`) and the search workers (`search::workers`) are one per
//! process, and `cargo test` runs the tests of a binary beside each other.
//! So a test that asserts on their totals (`held()`, `taken()`) needs the
//! process to itself (#63). `search_budget.rs` and `explorer_budget.rs` hold
//! one such test each; the tests of `explorer_small_budget.rs` each run again
//! in a child process of their own. A second test beside one of them must not
//! reserve from those budgets, or it belongs in a binary of its own. Anywhere
//! else, a test asserts on its own holds only.
#![allow(dead_code)]

use cbformat::fixture::{Builder, TempDb, quiet};
use cbformat::fixture_cbh::{self, Tok, encode, move_record};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};
use chesscore::Board;

pub const DOC: &str = include_str!("../../../../docs/search-grammar.md");

/// The non-blank lines of the document's fenced block tagged `tag`.
pub fn block(tag: &str) -> Vec<&'static str> {
    block_in(DOC, tag)
}

/// The non-blank lines of `doc`'s fenced block tagged `tag`, with `\n` or
/// `\r\n` line ends: a Windows checkout may convert them.
pub fn block_in<'a>(doc: &'a str, tag: &str) -> Vec<&'a str> {
    let fence = format!("```{tag}");
    let open = doc
        .match_indices(&fence)
        .map(|(at, _)| at + fence.len())
        .find(|&end| doc[end..].starts_with('\n') || doc[end..].starts_with("\r\n"))
        .unwrap_or_else(|| panic!("no {tag} block"));
    let start = open + doc[open..].find('\n').unwrap() + 1;
    let end = start + doc[start..].find("```").unwrap();
    doc[start..end].lines().filter(|l| !l.trim().is_empty()).collect()
}

/// Entity ids by name, in order of first use; id 0 is the empty name.
#[derive(Default)]
pub struct Names(pub Vec<String>);

impl Names {
    pub fn id(&mut self, name: &str) -> i64 {
        if name == "-" {
            return 0;
        }
        if self.0.is_empty() {
            self.0.push(String::new());
        }
        let at = self.0.iter().position(|n| n == name).unwrap_or_else(|| {
            self.0.push(name.to_string());
            self.0.len() - 1
        });
        at as i64
    }
}

fn container(size: usize, record: &[u8]) -> Vec<u8> {
    if record.is_empty() {
        return vec![0; size];
    }
    let mut c = (record.len() as i32).to_le_bytes().to_vec();
    c.extend(record);
    assert!(c.len() <= size);
    c.resize(size, 0);
    c
}

fn string(s: &str) -> Vec<u8> {
    let mut v = (s.len() as i32).to_le_bytes().to_vec();
    v.extend(s.as_bytes());
    v
}

/// A `.2lid` with the six entity types, holding players (type 0), tournaments
/// (type 1) and the game tags that carry titles (type 5).
pub fn lid(players: &[String], tournaments: &[String], titles: &[String]) -> Vec<u8> {
    let tables: [Vec<Vec<u8>>; 6] = [
        players
            .iter()
            .map(|name| {
                let (last, first) = name.split_once(", ").unwrap_or((name, ""));
                [string(last), string(first)].concat()
            })
            .collect(),
        tournaments.iter().map(|t| [string(""), string(t), vec![0; 4]].concat()).collect(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        // One title, in language 0.
        titles.iter().map(|t| [1i32.to_le_bytes().to_vec(), 0i32.to_le_bytes().to_vec(), string(t)].concat()).collect(),
    ];
    let count = tables.iter().map(Vec::len).max().unwrap_or(0).max(1);
    // Containers big enough for the longest record.
    let size = tables.iter().flatten().map(|r| r.len() + 4).max().unwrap_or(0).max(64).next_multiple_of(8);
    let mut d = Vec::new();
    d.extend(184i32.to_be_bytes());
    d.extend(6i32.to_be_bytes());
    for _ in &tables {
        d.extend((size as i32).to_be_bytes());
        d.extend((count as i64).to_be_bytes());
        d.extend((-1i64).to_be_bytes());
    }
    d.resize(184, 0);
    for id in 0..count {
        for table in &tables {
            d.extend(container(size, table.get(id).map_or(&[][..], Vec::as_slice)));
        }
    }
    d
}

pub fn put(rec: &mut [u8; 192], at: usize, bytes: &[u8]) {
    rec[at..at + bytes.len()].copy_from_slice(bytes);
}

/// The fixture of the document, written to a temporary directory, plus `extra`
/// rows in the same form. A guiding text or an analysis takes its title from
/// the event column and its author from the annotator column, and stores them
/// in its own header layout.
pub fn fixture(name: &str, extra: &[&str]) -> TempDb {
    let (mut players, mut tournaments, mut titles) = (Names::default(), Names::default(), Names::default());
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    let rows = block("fixture").into_iter().filter(|l| !l.starts_with('#')).chain(extra.iter().copied());
    for (i, line) in rows.enumerate() {
        let f: Vec<&str> = line.split('|').map(str::trim).collect();
        assert_eq!(f[0].parse::<usize>().unwrap(), i + 1, "fixture rows are numbered in order");
        let rec = b.game(e4);
        let (white, black, annotator) = (players.id(f[2]), players.id(f[3]), players.id(f[12]));
        let ids: &[(usize, i64)] = match f[1] {
            "text" => {
                rec[0] |= 2;
                &[(0x20, annotator), (0x28, titles.id(f[4]))]
            }
            "analysis" => {
                rec[2] = 2;
                &[(0x18, titles.id(f[4])), (0x28, annotator)]
            }
            kind => {
                if kind == "deleted" {
                    rec[0] |= 0x80;
                }
                &[(0x18, white), (0x20, black), (0x28, tournaments.id(f[4])), (0x30, annotator)]
            }
        };
        for &(at, id) in ids {
            put(rec, at, &id.to_le_bytes());
        }
        if f[1] == "text" || f[1] == "analysis" {
            continue;
        }
        let date: Vec<i32> = f[5].split('.').map(|p| p.parse().unwrap_or(0)).collect();
        put(rec, 0xbc, &((date[0] << 9) | (date[1] << 5) | date[2]).to_le_bytes());
        let (round, sub): (i16, i16) = match f[6] {
            "-" => (0, 0),
            r => match r.split_once('(') {
                Some((r, s)) => (r.parse().unwrap(), s.trim_end_matches(')').parse().unwrap()),
                None => (r.parse().unwrap(), 0),
            },
        };
        put(rec, 0x5a, &round.to_le_bytes());
        put(rec, 0x5c, &sub.to_le_bytes());
        rec[0x58] = match f[7] {
            "0-1" => 0,
            "1/2-1/2" => 1,
            "1-0" => 2,
            _ => 3,
        };
        let eco = match f[8].as_bytes() {
            [l, d1, d2] => (u16::from(l - b'A') * 100 + u16::from(d1 - b'0') * 10 + u16::from(d2 - b'0') + 1) * 128,
            _ => 0,
        };
        put(rec, 0x80, &eco.to_le_bytes());
        put(rec, 0x8a, &f[9].parse::<i16>().unwrap().to_le_bytes());
        put(rec, 0x60, &f[10].parse::<i16>().unwrap().to_le_bytes());
        put(rec, 0x70, &f[11].parse::<i16>().unwrap().to_le_bytes());
    }
    b.lid(lid(&players.0, &tournaments.0, &titles.0));
    b.write(name)
}

/// The fixture of the document as a classic database, plus `extra` rows: the
/// same records, names and fields, as the classic format stores them. A
/// guiding text keeps its title in its own text record and names its author
/// as an annotator; the format has no analyses. The builder's own entities
/// come first and are used by no record.
pub fn classic_fixture(name: &str, extra: &[&str]) -> TempDb {
    let mut b = fixture_cbh::Builder::new();
    let e4 = move_record(0, None, None, &encode(&Board::startpos(), &[Tok::Mv("e2e4"), Tok::End], 0, false));
    let mut ids = ClassicIds::default();
    let rows = block("fixture").into_iter().filter(|l| !l.starts_with('#')).chain(extra.iter().copied());
    for (i, line) in rows.enumerate() {
        let f: Vec<&str> = line.split('|').map(str::trim).collect();
        assert_eq!(f[0].parse::<usize>().unwrap(), i + 1, "fixture rows are numbered in order");
        let annotator = ids.annotator(&mut b, f[12]);
        if f[1] == "text" {
            put3(b.text(&[(0, f[4].as_bytes())]), 0x0d, annotator);
            continue;
        }
        assert_ne!(f[1], "analysis", "the classic format has no analyses");
        let (white, black, event) = (ids.player(&mut b, f[2]), ids.player(&mut b, f[3]), ids.tournament(&mut b, f[4]));
        let rec = b.game(&e4);
        if f[1] == "deleted" {
            rec[0] |= 0x80;
        }
        for (at, id) in [(0x09, white), (0x0c, black), (0x0f, event), (0x12, annotator)] {
            put3(rec, at, id);
        }
        let date: Vec<u32> = f[5].split('.').map(|p| p.parse().unwrap_or(0)).collect();
        put3(rec, 0x18, (date[0] << 9) | (date[1] << 5) | date[2]);
        let (round, sub): (u8, u8) = match f[6] {
            "-" => (0, 0),
            r => match r.split_once('(') {
                Some((r, s)) => (r.parse().unwrap(), s.trim_end_matches(')').parse().unwrap()),
                None => (r.parse().unwrap(), 0),
            },
        };
        rec[0x1d] = round;
        rec[0x1e] = sub;
        rec[0x1b] = match f[7] {
            "0-1" => 0,
            "1/2-1/2" => 1,
            "1-0" => 2,
            _ => 3,
        };
        let eco = match f[8].as_bytes() {
            [l, d1, d2] => (u16::from(l - b'A') * 100 + u16::from(d1 - b'0') * 10 + u16::from(d2 - b'0') + 1) * 128,
            _ => 0,
        };
        rec[0x23..0x25].copy_from_slice(&eco.to_be_bytes());
        rec[0x2d] = f[9].parse().unwrap();
        rec[0x1f..0x21].copy_from_slice(&f[10].parse::<u16>().unwrap().to_be_bytes());
        rec[0x21..0x23].copy_from_slice(&f[11].parse::<u16>().unwrap().to_be_bytes());
    }
    b.write(name)
}

/// A 24-bit big-endian id at `at` of a classic header.
pub fn put3(rec: &mut [u8; 46], at: usize, v: u32) {
    rec[at..at + 3].copy_from_slice(&v.to_be_bytes()[1..]);
}

/// The classic entity ids of the fixture's names, each added on first use;
/// `-` is an empty name.
#[derive(Default)]
struct ClassicIds {
    players: Vec<(String, u32)>,
    tournaments: Vec<(String, u32)>,
    annotators: Vec<(String, u32)>,
}

fn known(list: &mut Vec<(String, u32)>, name: &str, add: impl FnOnce(&str) -> u32) -> u32 {
    let name = if name == "-" { "" } else { name };
    if let Some(&(_, id)) = list.iter().find(|(n, _)| n == name) {
        return id;
    }
    let id = add(name);
    list.push((name.to_string(), id));
    id
}

impl ClassicIds {
    fn player(&mut self, b: &mut fixture_cbh::Builder, name: &str) -> u32 {
        known(&mut self.players, name, |n| {
            let (last, first) = n.split_once(", ").unwrap_or((n, ""));
            b.player(last, first)
        })
    }

    fn tournament(&mut self, b: &mut fixture_cbh::Builder, name: &str) -> u32 {
        known(&mut self.tournaments, name, |n| b.tournament(n, ""))
    }

    fn annotator(&mut self, b: &mut fixture_cbh::Builder, name: &str) -> u32 {
        known(&mut self.annotators, name, |n| b.annotator(n))
    }
}
