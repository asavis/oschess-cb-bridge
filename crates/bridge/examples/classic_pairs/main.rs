//! Serves both copies of classic and 2CBH database pairs through the bridge's
//! HTTP API and compares the answers, printing counts only (#25, acceptance 4
//! and 5):
//!
//! ```text
//! cargo run --release -p bridge --example classic_pairs -- <scratch dir> <a.cbh> <a.2cbh> [<b.cbh> <b.2cbh> …]
//! ```
//!
//! For each pair:
//!
//! 1. Names: each game's players, tournament, site and annotator in the two
//!    copies, each equal or one of the differences the classic format
//!    explains, a closed list (`names::NameDiff`): characters Windows-1252
//!    lacks stored in another form, a cut at the classic field's width, and
//!    for an annotator the same words in another order. Any other difference
//!    fails the pair.
//! 2. Rows: every record's row, field by field; a name field may differ only
//!    where the record's name differs in a known way.
//! 3. Lists: for each query of a fixed list — the conformance corpus of
//!    `docs/search-grammar.md`, every sort key both ways, and player, event
//!    and annotator queries for the names the 2CBH copy suggests most — the
//!    record numbers of the whole result, page by page. A result that differs
//!    is explained by names when it agrees once the records whose names differ
//!    in a known way, in the fields the query reads, are left out.
//! 4. Suggestions for each letter and field, compared the same way.
//! 5. Explorer: the answers for sampled positions of the games' main lines,
//!    field by field: counts, moves and the top games' numbers, ratings,
//!    results and years must be equal; their names may differ only where the
//!    game's names differ in a known way.
//!
//! Nothing a database holds is printed: no name, no game, only counts. The
//! scratch directory receives the position indexes. The exit status is 1 when
//! any pair has a difference nothing explains.

use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bridge::access::{DEFAULT_ORIGINS, Policy};
use bridge::api::App;
use bridge::catalog::{Catalog, id_of};
use bridge::explorer::format::MAX_PLY;
use bridge::search::query::{self, Field, SortKey};
use bridge::server;
use cbformat::replay::TreeVisitor;
use cbformat::v2::RecordKind;
use cbformat::view::Base;
use chesscore::{Board, Move};

mod json;
mod names;

use json::Json;
use names::{ANNOTATOR, BLACK, EVENT, FIELDS, Names, WHITE, bit};

const TOKEN: &str = "classic-pairs-harness-token-0123456789abcdef";
const DOC: &str = include_str!("../../../../docs/search-grammar.md");
const POSITIONS: usize = 200;
const SORT_KEYS: [&str; 12] = [
    "number",
    "white",
    "black",
    "whiteElo",
    "blackElo",
    "result",
    "moves",
    "eco",
    "tournament",
    "date",
    "round",
    "annotator",
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 || args.len().is_multiple_of(2) {
        eprintln!("usage: classic_pairs <scratch dir> <a.cbh> <a.2cbh> [<b.cbh> <b.2cbh> …]");
        std::process::exit(2);
    }
    let scratch = PathBuf::from(&args[0]);
    let mut failed = false;
    for (i, pair) in args[1..].chunks(2).enumerate() {
        let dir = scratch.join(format!("pair-{i}"));
        std::fs::create_dir_all(&dir).expect("scratch directory");
        let label = Path::new(&pair[0]).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        println!("== pair {i}: {label}");
        failed |= !compare(Path::new(&pair[0]), Path::new(&pair[1]), &dir);
    }
    if failed {
        std::process::exit(1);
    }
}

struct Server {
    port: u16,
    classic: String,
    two: String,
}

fn serve(classic: &Path, two: &Path, dir: &Path) -> Server {
    let listeners = server::bind(0).expect("bind");
    let port = listeners[0].local_addr().expect("address").port();
    let app = App {
        version: "harness",
        policy: Policy { port, origins: DEFAULT_ORIGINS.iter().map(|o| o.to_string()).collect(), token: TOKEN.into() },
        catalog: Catalog::new([classic.to_path_buf(), two.to_path_buf()]),
        between_reads: None,
    };
    app.catalog.explorer.set_dir(dir.to_path_buf());
    let app = Arc::new(app);
    std::thread::spawn(move || server::serve(listeners, app));
    Server { port, classic: id_of(classic), two: id_of(two) }
}

fn get(port: u16, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let raw = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nOrigin: {}\r\nConnection: close\r\n\r\n",
        DEFAULT_ORIGINS[0]
    );
    s.write_all(raw.as_bytes()).expect("send");
    let mut out = Vec::new();
    s.read_to_end(&mut out).expect("receive");
    let out = String::from_utf8(out).expect("UTF-8 answer");
    let status = out.split(' ').nth(1).and_then(|s| s.parse().ok()).expect("status");
    (status, out.split_once("\r\n\r\n").map(|x| x.1.to_string()).unwrap_or_default())
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The numbers after each `pattern` in `body`.
fn numbers_after(body: &str, pattern: &str) -> Vec<u64> {
    body.match_indices(pattern)
        .filter_map(|(at, p)| {
            let rest = &body[at + p.len()..];
            let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
            rest[..end].parse().ok()
        })
        .collect()
}

/// A list request's whole result: the record numbers, or the error answer.
fn list(port: u16, id: &str, q: &str) -> Result<Vec<u32>, (u16, String)> {
    let mut out = Vec::new();
    loop {
        let path = format!("/v1/databases/{id}/games?q={}&offset={}&limit=500", encode(q), out.len());
        let (status, body) = get(port, &path);
        if status != 200 {
            return Err((status, body));
        }
        let total = numbers_after(&body, "\"total\":")[0] as usize;
        let page: Vec<u32> = numbers_after(&body, "{\"number\":").into_iter().map(|n| n as u32).collect();
        let empty = page.is_empty();
        out.extend(page);
        if out.len() >= total || empty {
            return Ok(out);
        }
    }
}

/// Suggestions as (name, games).
fn suggest(port: u16, id: &str, field: &str, prefix: &str) -> Vec<(String, u64)> {
    let (status, body) = get(port, &format!("/v1/databases/{id}/suggest?field={field}&prefix={}", encode(prefix)));
    assert_eq!(status, 200, "suggest");
    let mut out = Vec::new();
    for item in body.split("{\"value\":").skip(1) {
        // The value is a JSON string; its end is the first unescaped quote.
        let b = item.as_bytes();
        let mut i = 1;
        while i < b.len() && b[i] != b'"' {
            i += if b[i] == b'\\' { 2 } else { 1 };
        }
        let value = item[..=i.min(item.len() - 1)].to_string();
        let games = numbers_after(item, "\"games\":").first().copied().unwrap_or(0);
        out.push((value, games));
    }
    out
}

/// The name fields a query reads: its terms, and its sort.
fn fields_read(q: &str) -> u8 {
    let Ok(parsed) = query::parse(q) else { return 0 };
    let mut mask = 0;
    for t in &parsed.terms {
        mask |= match t.field {
            Field::Text => bit(WHITE) | bit(BLACK) | bit(EVENT) | bit(ANNOTATOR),
            Field::White => bit(WHITE),
            Field::Black => bit(BLACK),
            Field::Player => bit(WHITE) | bit(BLACK),
            Field::Event => bit(EVENT),
            Field::Annotator => bit(ANNOTATOR),
            _ => 0,
        };
    }
    mask | match parsed.sort.map(|s| s.key) {
        Some(SortKey::White) => bit(WHITE),
        Some(SortKey::Black) => bit(BLACK),
        Some(SortKey::Tournament) => bit(EVENT),
        Some(SortKey::Annotator) => bit(ANNOTATOR),
        _ => 0,
    }
}

#[derive(Default)]
struct Tally {
    total: u64,
    identical: u64,
    /// Explained by names, per the fields whose differences explain it.
    by_names: BTreeMap<&'static str, u64>,
    unexplained: u64,
}

fn field_label(bits: u8) -> &'static str {
    match bits {
        b if b == bit(ANNOTATOR) => "annotator",
        b if b & bit(ANNOTATOR) == 0 => "players/event",
        _ => "players/event+annotator",
    }
}

/// Compares two results of the query `q`.
fn judge(tally: &mut Tally, names: &Names, q: &str, a: &[u32], b: &[u32]) {
    tally.total += 1;
    if a == b {
        tally.identical += 1;
        return;
    }
    let read = fields_read(q);
    let mask = |n: u32| names.known_bits(n) & read;
    let keep = |v: &[u32]| v.iter().copied().filter(|&n| mask(n) == 0).collect::<Vec<_>>();
    if read != 0 && keep(a) == keep(b) {
        let bits = a.iter().chain(b).fold(0, |acc, &n| acc | mask(n));
        *tally.by_names.entry(field_label(bits)).or_insert(0) += 1;
    } else {
        tally.unexplained += 1;
    }
}

fn print_tally(what: &str, t: &Tally) {
    let names: Vec<String> = t.by_names.iter().map(|(k, v)| format!("{k} {v}")).collect();
    println!(
        "{what}: {} compared, {} identical, {} explained by names [{}], {} unexplained",
        t.total,
        t.identical,
        t.by_names.values().sum::<u64>(),
        names.join(", "),
        t.unexplained
    );
}

/// The queries: the corpus, every sort both ways, and name queries for the
/// names the 2CBH copy suggests most.
fn queries(s: &Server) -> Vec<String> {
    let mut qs: Vec<String> =
        block("corpus").into_iter().filter_map(|l| l.rsplit_once("=>").map(|(q, _)| q.trim().to_string())).collect();
    for key in SORT_KEYS {
        qs.push(format!("sort:{key}-asc"));
        qs.push(format!("sort:{key}-desc"));
    }
    for (field, qualifiers) in
        [("player", &["player", "white", "black"][..]), ("event", &["event"][..]), ("annotator", &["annotator"][..])]
    {
        let mut all: Vec<(String, u64)> = Vec::new();
        for c in 'a'..='z' {
            all.extend(suggest(s.port, &s.two, field, &c.to_string()));
        }
        all.sort_by(|x, y| y.1.cmp(&x.1).then_with(|| x.0.cmp(&y.0)));
        all.dedup();
        for (json, _) in all.iter().take(5) {
            // The value as JSON; it is searchable, so it holds no quote.
            let name = json.trim_matches('"').replace("\\\\", "\\");
            for qualifier in qualifiers {
                qs.push(format!("{qualifier}:\"{name}\""));
            }
            qs.push(format!("{}:\"{name}\" sort:date", qualifiers[0]));
            qs.push(format!(
                "{}:\"{name}\" sort:{}-desc",
                qualifiers[0],
                if field == "event" { "white" } else { "tournament" }
            ));
        }
    }
    qs
}

/// The non-blank lines of the document's fenced block tagged `tag`.
fn block(tag: &str) -> Vec<&'static str> {
    let fence = format!("```{tag}\n");
    let start = DOC.find(&fence).expect("block") + fence.len();
    let end = start + DOC[start..].find("```").expect("block end");
    DOC[start..end].lines().filter(|l| !l.trim().is_empty()).collect()
}

/// Positions reached by main lines of the 2CBH copy's games.
struct Boards {
    boards: Vec<Board>,
    done: bool,
}

impl TreeVisitor for Boards {
    fn play(&mut self, before: &Board, mv: Option<Move>, main_line: bool) {
        if self.done || !main_line || self.boards.len() > usize::from(MAX_PLY) {
            self.done = true;
            return;
        }
        self.boards.push(before.clone());
        if mv.is_none() {
            self.done = true;
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn sample(two: &Base, count: usize, seed: u64) -> Vec<String> {
    let mut rng = Rng(seed | 1);
    let mut fens = Vec::new();
    let mut seen = HashSet::new();
    let n = two.record_count();
    let mut tries = 0;
    while fens.len() < count && tries < count * 50 && n > 0 {
        tries += 1;
        let id = (rng.next() % u64::from(n)) as u32 + 1;
        let Ok(h) = two.header(id) else { continue };
        if h.kind() != RecordKind::Game || h.is_deleted() {
            continue;
        }
        let Ok(moves) = two.moves_of(&h) else { continue };
        let mut b = Boards { boards: Vec::new(), done: false };
        let _ = moves.walk(&mut b);
        if b.boards.is_empty() {
            continue;
        }
        let board = &b.boards[(rng.next() % b.boards.len() as u64) as usize];
        if board.is_chess960() {
            continue;
        }
        let fen = board.fen();
        if seen.insert(fen.clone()) {
            fens.push(fen);
        }
    }
    fens
}

/// The explorer's answer, once its index is built.
fn explorer(port: u16, id: &str, fen: &str) -> (u16, String) {
    let path = format!("/v1/databases/{id}/explorer?fen={}", encode(fen));
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let (status, body) = get(port, &path);
        if status != 409 || !body.contains("\"indexing\"") || Instant::now() > deadline {
            return (status, body);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// How two answers compare.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Verdict {
    Identical,
    /// Differ in name fields only, each where the record's name differs in a
    /// known way.
    Names,
    Different,
}

/// Compares two JSON objects member by member: `skip` is left out, `names`
/// may differ where record `number`'s names differ in a known way, and every
/// other member must be equal.
fn members(a: &Json, b: &Json, number: Option<u32>, names: &Names, skip: &[&str], fields: &[(&str, usize)]) -> Verdict {
    let (Json::Obj(a), Json::Obj(b)) = (a, b) else { return Verdict::Different };
    if a.len() != b.len() {
        return Verdict::Different;
    }
    let mut verdict = Verdict::Identical;
    for ((ka, va), (kb, vb)) in a.iter().zip(b) {
        if ka != kb {
            return Verdict::Different;
        }
        if skip.contains(&ka.as_str()) || va == vb {
            continue;
        }
        match (fields.iter().find(|(k, _)| k == ka), number) {
            (Some(&(_, field)), Some(n)) if names.explains(n, field) => verdict = verdict.max(Verdict::Names),
            _ => return Verdict::Different,
        }
    }
    verdict
}

/// The number member of a row or a top game.
fn number_of(v: &Json) -> Option<u32> {
    match v.get("number") {
        Some(Json::Num(n)) => n.parse().ok(),
        _ => None,
    }
}

/// Compares two explorer answers: the generation is left out, the top games
/// are compared one by one, and everything else must be equal.
fn judge_answer(a: &Json, b: &Json, names: &Names) -> Verdict {
    let top = |v: &Json| match v.get("topGames") {
        Some(Json::Arr(items)) => Some(items.clone()),
        _ => None,
    };
    let (Some(ta), Some(tb)) = (top(a), top(b)) else { return Verdict::Different };
    let mut verdict = members(a, b, None, names, &["generation", "topGames"], &[]);
    if ta.len() != tb.len() {
        return Verdict::Different;
    }
    let fields = [("white", WHITE), ("black", BLACK), ("event", EVENT)];
    for (x, y) in ta.iter().zip(&tb) {
        let n = number_of(x);
        if n.is_none() || n != number_of(y) {
            return Verdict::Different;
        }
        verdict = verdict.max(members(x, y, n, names, &[], &fields));
    }
    verdict
}

/// Every record's row, in number order, as JSON; the error answer otherwise.
fn rows(port: u16, id: &str) -> Result<Vec<Json>, (u16, String)> {
    let mut out = Vec::new();
    loop {
        let (status, body) = get(port, &format!("/v1/databases/{id}/games?offset={}&limit=500", out.len()));
        if status != 200 {
            return Err((status, body));
        }
        let Some(page) = json::parse(&body) else { return Err((status, "not JSON".into())) };
        let total = match page.get("total") {
            Some(Json::Num(n)) => n.parse::<usize>().unwrap_or(0),
            _ => 0,
        };
        let Some(Json::Arr(items)) = page.get("rows") else { return Err((status, "no rows".into())) };
        let empty = items.is_empty();
        out.extend(items.iter().cloned());
        if out.len() >= total || empty {
            return Ok(out);
        }
    }
}

fn compare(classic_path: &Path, two_path: &Path, dir: &Path) -> bool {
    let (classic, two) = match (Base::open(classic_path), Base::open(two_path)) {
        (Ok(a), Ok(b)) => (a, b),
        _ => {
            println!("could not open both copies");
            return false;
        }
    };
    println!("records: classic {}, 2cbh {}", classic.record_count(), two.record_count());
    let started = Instant::now();
    let names = Names::compare(&classic, &two);
    let described: Vec<String> = names.counts.iter().map(|((f, d), n)| format!("{f} {d:?} {n}")).collect();
    println!(
        "name differences (games): [{}]; of no known kind: {}; record kinds differing: {}",
        described.join(", "),
        names.unknown(),
        names.kinds
    );

    let s = serve(classic_path, two_path, dir);
    let (_, listed) = get(s.port, "/v1/databases");
    let ready = listed.matches("\"state\":\"ready\"").count();
    let cbh_ready = listed.contains("\"format\":\"cbh\",\"state\":\"ready\"");
    println!("databases ready: {ready} of 2; classic listed as cbh and ready: {cbh_ready}");

    let mut row_tally = Tally::default();
    let name_fields: Vec<(&str, usize)> = FIELDS.iter().enumerate().map(|(i, f)| (*f, i)).collect();
    match (rows(s.port, &s.classic), rows(s.port, &s.two)) {
        (Ok(a), Ok(b)) if a.len() == b.len() => {
            for (x, y) in a.iter().zip(&b) {
                row_tally.total += 1;
                let n = number_of(x);
                match if n.is_some() && n == number_of(y) {
                    members(x, y, n, &names, &[], &name_fields)
                } else {
                    Verdict::Different
                } {
                    Verdict::Identical => row_tally.identical += 1,
                    Verdict::Names => *row_tally.by_names.entry("names").or_insert(0) += 1,
                    Verdict::Different => row_tally.unexplained += 1,
                }
            }
        }
        _ => row_tally.unexplained += 1,
    }
    print_tally("rows", &row_tally);

    let mut lists = Tally::default();
    let mut errors_equal = 0u64;
    for q in queries(&s) {
        match (list(s.port, &s.classic, &q), list(s.port, &s.two, &q)) {
            (Ok(a), Ok(b)) => judge(&mut lists, &names, &q, &a, &b),
            (Err(a), Err(b)) if a == b => {
                lists.total += 1;
                lists.identical += 1;
                errors_equal += 1;
            }
            _ => {
                lists.total += 1;
                lists.unexplained += 1;
            }
        }
    }
    print_tally("lists", &lists);
    println!("  of which refused the same way by both: {errors_equal}");

    // Suggestions: a name that differs in a known way in either copy is left
    // out of both lists, and the rest must agree as far as both reach.
    let mut differing: HashSet<String> = HashSet::new();
    for id in 1..=classic.record_count().min(two.record_count()) {
        let m = names.known_bits(id);
        if m == 0 {
            continue;
        }
        for db in [&classic, &two] {
            let Ok(h) = db.header(id) else { continue };
            let Ok(n) = db.names(&h) else { continue };
            let player = |p: &Option<cbformat::v2::Player>| p.as_ref().map(|p| p.pgn()).unwrap_or_default();
            if m & bit(WHITE) != 0 {
                differing.insert(player(&n.white));
            }
            if m & bit(BLACK) != 0 {
                differing.insert(player(&n.black));
            }
            if m & bit(EVENT) != 0 {
                differing.insert(n.tournament.as_ref().map(|t| t.title.clone()).unwrap_or_default());
            }
            if m & bit(ANNOTATOR) != 0 {
                differing.insert(n.annotator.clone().unwrap_or_default());
            }
        }
    }
    let differing: HashSet<String> = differing.into_iter().map(|n| bridge::json::string(&n)).collect();
    let mut suggestions = Tally::default();
    for field in ["player", "event", "annotator"] {
        for c in 'a'..='z' {
            let (a, b) =
                (suggest(s.port, &s.classic, field, &c.to_string()), suggest(s.port, &s.two, field, &c.to_string()));
            suggestions.total += 1;
            if a == b {
                suggestions.identical += 1;
                continue;
            }
            let keep =
                |v: &[(String, u64)]| v.iter().filter(|x| !differing.contains(&x.0)).cloned().collect::<Vec<_>>();
            let (ka, kb) = (keep(&a), keep(&b));
            let n = ka.len().min(kb.len());
            if ka[..n] == kb[..n] {
                *suggestions
                    .by_names
                    .entry(if field == "annotator" { "annotator" } else { "players/event" })
                    .or_insert(0) += 1;
            } else {
                suggestions.unexplained += 1;
            }
        }
    }
    print_tally("suggestions", &suggestions);

    let fens = sample(&two, POSITIONS, 0x2025_0925 ^ u64::from(two.record_count()));
    let mut positions = Tally::default();
    let mut with_games = 0;
    for fen in &fens {
        let (a, b) = (explorer(s.port, &s.classic, fen), explorer(s.port, &s.two, fen));
        positions.total += 1;
        let (Some(ja), Some(jb)) = (json::parse(&a.1), json::parse(&b.1)) else {
            positions.unexplained += 1;
            continue;
        };
        if a.0 != 200 || b.0 != 200 {
            positions.unexplained += 1;
            continue;
        }
        if jb.get("games").is_some_and(|g| *g != Json::Num("0".into())) {
            with_games += 1;
        }
        match judge_answer(&ja, &jb, &names) {
            Verdict::Identical => positions.identical += 1,
            Verdict::Names => *positions.by_names.entry("top games' names").or_insert(0) += 1,
            Verdict::Different => positions.unexplained += 1,
        }
    }
    print_tally("explorer positions", &positions);
    println!("  positions with games in the 2cbh index: {with_games}");
    println!("time: {:.1} s", started.elapsed().as_secs_f64());
    let passed = [
        row_tally.unexplained,
        lists.unexplained,
        suggestions.unexplained,
        positions.unexplained,
        names.unknown(),
        names.kinds,
    ]
    .iter()
    .all(|&n| n == 0);
    println!("pair: {}", if passed { "pass" } else { "FAIL" });
    passed
}
