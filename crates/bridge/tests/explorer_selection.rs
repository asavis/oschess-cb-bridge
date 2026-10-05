//! A direct sorting oracle over hand-built games, independent of the index's
//! numeric key, folding, replay and bounded selection implementations.
use std::io::{Seek, SeekFrom, Write};

use bridge::explorer::{self, format::TOP_GAMES, runs::Progress};
use bridge::search::memory::Cancel;
use cbformat::fixture::{Builder, words};
use cbformat::movetable::{END_OF_LINE, MOVES};
use cbformat::v2::Database;
use chesscore::Board;

mod common;
use common::{TestBridge, answered, board_after, fen_param, index_dir, member, objects};

const LINE: &str = "e2e4 e7e5 g1f3 b8c6 f1b5 a7a6 b5a4 g8f6 e1g1 f8e7 f1e1 b7b5 a4b3 d7d6 c2c3 e8g8 h2h3 c8b7 d2d4 f8e8 b1d2 e7f8 d4d5 c6b8 a2a4 c7c6 d5c6 b7c6";

#[derive(Clone)]
struct Game {
    rating: (i16, i16),
    date: (i32, i32, i32),
    line: String,
}

fn games() -> Vec<Game> {
    // The first 12 legacy leaders must lose to a recent lower-rated game.
    let mut games = vec![Game { rating: (3200, 3200), date: (1980, 1, 1), line: LINE.into() }; 12];
    let cases = [
        ((2700, 2700), (2023, 2, 28)), // inclusive 1-year boundary, leap-day anchor
        ((2699, 2700), (2024, 2, 29)), // half point below first threshold
        ((2700, 2701), (2023, 2, 28)), // half point above
        ((2700, 2700), (2024, 1, 1)),  // equal rating, different date
        ((2600, 2600), (2021, 2, 28)), // inclusive 3-year boundary
        ((2599, 2600), (2024, 2, 29)),
        ((2400, 2400), (2019, 2, 28)), // inclusive 5-year boundary
        ((2399, 2400), (2024, 2, 29)),
        ((3900, 0), (2024, 2, 29)), // one unknown becomes 1500: exactly 2700
        ((0, 3901), (2024, 2, 29)),
        ((0, 0), (2024, 2, 29)),
        ((3100, 3100), (0, 0, 0)),
        ((2700, 2700), (2023, 2, 27)),
        ((2600, 2600), (2021, 2, 27)),
        ((2400, 2400), (2019, 2, 27)),
        ((2800, 2800), (2023, 0, 0)),
        ((2800, 2800), (2023, 3, 0)),
        ((2800, 2800), (2024, 0, 0)),
    ];
    // Enough copies to force folding; half first reach e4 beyond the tree.
    for i in 0..216 {
        let (rating, date) = cases[i % cases.len()];
        let prefix = if i % 2 == 0 { "g1f3 g8f6 f3g1 f6g8 ".repeat(6) } else { String::new() };
        games.push(Game { rating, date, line: prefix + LINE });
    }
    games
}

fn packed((year, month, day): (i32, i32, i32)) -> i32 {
    year << 9 | month << 5 | day
}
fn normalized((year, month, day): (i32, i32, i32)) -> Option<(i32, i32, i32)> {
    (year > 0).then_some((year, month.max(1), day.max(1)))
}
fn sum(g: &Game) -> i32 {
    let known = |v: i16| if v > 0 { i32::from(v) } else { 1500 };
    known(g.rating.0) + known(g.rating.1)
}
fn expected(games: &[Game], members: impl Fn(u32) -> bool) -> Vec<u32> {
    let anchor = games.iter().filter_map(|g| normalized(g.date)).max();
    let tier = |g: &Game| {
        if let (Some((year, month, day)), Some(date)) = (anchor, normalized(g.date)) {
            for (tier, years, minimum) in [(0, 1, 5400), (1, 3, 5200), (2, 5, 4800)] {
                let y = year - years;
                let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
                let d = if month == 2 && day == 29 && !leap { 28 } else { day };
                if sum(g) >= minimum && date >= (y, month, d) {
                    return tier;
                }
            }
        }
        3
    };
    let mut selected: Vec<_> = games.iter().enumerate().filter(|(i, _)| members(*i as u32 + 1)).collect();
    selected.sort_by_key(|(i, g)| (tier(g), std::cmp::Reverse(sum(g)), std::cmp::Reverse(*i)));
    selected.into_iter().take(TOP_GAMES).map(|(i, _)| i as u32 + 1).collect()
}
fn database(games: &[Game], name: &str) -> cbformat::fixture::TempDb {
    let mut b = Builder::new();
    for g in games {
        let mut line = vec![MOVES];
        line.extend(words(&mut Board::startpos(), &g.line));
        line.push(END_OF_LINE);
        let at = b.moves(1, &line);
        let row = b.game(at);
        row[0x58] = 2;
        row[0x60..0x62].copy_from_slice(&g.rating.0.to_le_bytes());
        row[0x70..0x72].copy_from_slice(&g.rating.1.to_le_bytes());
        row[0xbc..0xc0].copy_from_slice(&packed(g.date).to_le_bytes());
    }
    b.write(name)
}
fn number(body: &str, field: &str) -> u64 {
    member(body, field).parse().unwrap()
}
fn numbers(body: &str, field: &str) -> Vec<u32> {
    objects(body, field).iter().map(|g| number(g, "number") as u32).collect()
}

#[test]
fn opening_deep_and_combined_selections_match_the_oracle_without_source_io() {
    let games = games();
    let db = database(&games, "selection-oracle");
    let dir = index_dir("selection-oracle");
    let source = Database::open(db.dir().join("db.2cbh")).unwrap();
    let index = explorer::prepare(&source, 1, &dir, "db", &Progress::default()).unwrap();
    drop(source);
    // Queries can use only the index: the original headers no longer exist.
    std::fs::rename(db.dir().join("db.2cbh"), db.dir().join("unavailable")).unwrap();
    let want = expected(&games, |_| true);
    for board in [Board::startpos(), board_after("e2e4"), board_after(LINE)] {
        let stats = explorer::stats(&index, &board, &Cancel::never()).unwrap().unwrap();
        assert_eq!(stats.featured, want);
        assert_eq!(stats.counts.games, games.len() as u64);
        assert_eq!(stats.counts.white, games.len() as u64);
        assert!(want.iter().any(|n| !stats.top.contains(n)), "not a reordering of the old top 12");
        if board.hash() == board_after(LINE).hash() {
            let deep = explorer::deep_stats(&index, &board, &Cancel::never()).unwrap().unwrap();
            assert_eq!(stats, deep, "a deep-only position");
        } else if board.hash() == board_after("e2e4").hash() {
            let opening = index.lookup(board.hash()).unwrap().unwrap();
            assert_eq!(opening.featured, expected(&games, |n| n <= 12 || (n - 13) % 2 == 1));
            assert!(opening.counts.games < stats.counts.games, "the combined answer includes deep games");
        }
    }
    drop(index);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn filters_use_the_whole_database_anchor_and_cache_only_their_own_generation() {
    let mut games = games();
    let db = database(&games, "selection-filters");
    let dir = index_dir("selection-filters");
    let (bridge, id) = TestBridge::database(&db, &dir);
    let position = fen_param(&board_after("e2e4").fen());
    let url = format!("/v1/databases/{id}/explorer?fen={position}");
    for (query, members) in [
        ("", (1..=games.len() as u32).collect::<Vec<_>>()),
        (
            "&q=date:..2023",
            (1..=games.len() as u32).filter(|n| (1..=2023).contains(&games[*n as usize - 1].date.0)).collect(),
        ),
        ("&q=whiteelo:2700..", (1..=games.len() as u32).filter(|n| games[*n as usize - 1].rating.0 >= 2700).collect()),
        ("&q=whiteelo:5000..", Vec::new()),
    ] {
        let path = format!("{url}{query}");
        let body = answered(bridge.port, &path);
        assert_eq!(numbers(&body, "featuredGames"), expected(&games, |n| members.contains(&n)), "{query}: {body}");
        assert_eq!(number(&body, "games"), members.len() as u64);
        let rows = answered(bridge.port, &format!("/v1/databases/{id}/games?fen={position}&limit=500{query}"));
        assert_eq!(numbers(&rows, "rows"), members, "All games membership is unchanged");
        assert_eq!(answered(bridge.port, &path), body, "cached result");
    }
    let original = answered(bridge.port, &format!("{url}&q=whiteelo:2700.."));
    drop(bridge);
    // A later game in the same database moves the anchor and invalidates the
    // cached selection even though the query and all other headers stay fixed.
    games[0].date = (2035, 1, 1);
    let mut file = std::fs::OpenOptions::new().write(true).open(db.dir().join("db.2cbh")).unwrap();
    file.seek(SeekFrom::Start(192 + 0xbc)).unwrap();
    file.write_all(&packed(games[0].date).to_le_bytes()).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let (bridge, _) = TestBridge::database(&db, &dir);
    let changed = answered(bridge.port, &format!("{url}&q=whiteelo:2700.."));
    assert_ne!(numbers(&original, "featuredGames"), numbers(&changed, "featuredGames"));
    assert_eq!(numbers(&changed, "featuredGames"), expected(&games, |n| games[n as usize - 1].rating.0 >= 2700));
    drop(bridge);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn historical_partial_and_unknown_dates_fill_without_quotas_and_ties_ignore_dates() {
    let base = games();
    for (case, games) in [
        (
            "historical",
            base.iter()
                .cloned()
                .map(|mut g| {
                    if g.date.0 > 0 {
                        g.date.0 -= 100;
                    }
                    g
                })
                .collect::<Vec<_>>(),
        ),
        (
            "unknown",
            base.iter()
                .cloned()
                .map(|mut g| {
                    g.date = (0, 0, 0);
                    g
                })
                .collect(),
        ),
        ("few", base[12..19].to_vec()),
        (
            "same-tier-dates",
            base.iter()
                .cloned()
                .map(|mut g| {
                    if g.date == (2023, 2, 28) {
                        g.date = (2024, 2, 28);
                    }
                    g
                })
                .collect(),
        ),
    ] {
        let db = database(&games, case);
        let dir = index_dir(case);
        let source = Database::open(db.dir().join("db.2cbh")).unwrap();
        let idx = explorer::prepare(&source, 1, &dir, "db", &Progress::default()).unwrap();
        let actual = idx.lookup(Board::startpos().hash()).unwrap().unwrap().featured;
        assert_eq!(actual, expected(&games, |_| true), "{case}");
        assert_eq!(actual.len(), games.len().min(TOP_GAMES));
        if case == "same-tier-dates" {
            assert_eq!(actual, expected(&base, |_| true));
        }
        drop(idx);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
