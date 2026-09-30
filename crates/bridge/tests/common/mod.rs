//! What the test files share: a bridge served on a free port, the requests a
//! test sends it and the reading of its answers, and the fixture of
//! `docs/search-grammar.md`, written as a 2CBH database, as a classic one and
//! as a PGN file with the same content. Each test file uses a part of it.
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
//!
//! A test that needs them otherwise than its binary's other tests have them,
//! or needs the process to itself, runs again in a child process of its own
//! ([`in_child`]).
#![allow(dead_code)]

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bridge::access::{DEFAULT_ORIGINS, Policy};
use bridge::api::App;
use bridge::catalog::{Busy, Catalog, id_of};
use bridge::server;
use cbformat::fixture::{Builder, TempDb, quiet};
use cbformat::fixture_cbh::{self, Tok, encode, move_record};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};
use chesscore::{Board, Move, Square};

/// The pairing token of every bridge a test serves or starts.
pub const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";
/// The origin the requests of [`request`] name, which the default policy
/// allows.
pub const ORIGIN: &str = DEFAULT_ORIGINS[0];

/// The policy of a test's bridge: the default origins and [`TOKEN`]. Its port
/// is the one [`serve`] binds.
pub fn policy() -> Policy {
    Policy { port: 0, origins: DEFAULT_ORIGINS.map(String::from).to_vec(), token: TOKEN.into() }
}

/// Serves `app` on a free loopback port, from a thread of its own, its
/// policy's port set to that one; the port.
pub fn serve(app: App) -> u16 {
    serve_shared(app).0
}

/// The app of a test's bridge serving the databases at `paths`, with
/// [`policy`] and no engine.
pub fn app_of(paths: impl IntoIterator<Item = PathBuf>) -> App {
    App::new("test", policy(), Catalog::new(paths))
}

/// Serves the databases at `paths` as [`serve_shared`] does, with their
/// indexes in `dir` as in a data folder.
pub fn start_with_dir(paths: impl IntoIterator<Item = PathBuf>, dir: &Path) -> (u16, Arc<App>) {
    serve_with_dir(app_of(paths), dir)
}

/// Serves `app` as [`serve_shared`] does, with its indexes in `dir` as in a
/// data folder.
pub fn serve_with_dir(app: App, dir: &Path) -> (u16, Arc<App>) {
    app.catalog.use_data_dir(dir);
    serve_shared(app)
}

/// [`serve`], and the app served, for a test that asks it things as it serves.
/// Its connections wait [`IDLE_TIMEOUT`] for a request.
pub fn serve_shared(mut app: App) -> (u16, Arc<App>) {
    let listeners = server::bind(0).unwrap();
    let port = listeners[0].local_addr().unwrap().port();
    app.policy.port = port;
    app.idle_timeout = IDLE_TIMEOUT;
    let app = Arc::new(app);
    let served = Arc::clone(&app);
    std::thread::spawn(move || server::serve(listeners, served));
    (port, app)
}

/// A bridge served for a test that gives up the position indexes it holds
/// when dropped. A held index keeps its move stream mapped, and a mapped file
/// cannot be replaced or removed on Windows: a test drops the bridge before
/// it changes or removes the files, or starts another bridge that rebuilds
/// them. Its threads go on listening, holding no index, until the process
/// ends.
pub struct Served {
    pub port: u16,
    app: Arc<App>,
}

impl Served {
    /// Serves `app` as [`serve`] does.
    pub fn new(app: App) -> Served {
        let (port, app) = serve_shared(app);
        Served { port, app }
    }

    /// Serves `app` as [`serve_with_dir`] does, with its indexes in `dir`.
    pub fn with_dir(app: App, dir: &Path) -> Served {
        app.catalog.use_data_dir(dir);
        Served::new(app)
    }

    /// Serves the 2CBH database `db` with its indexes in `dir`, as in a data
    /// folder: the bridge and the database's id.
    pub fn database(db: &TempDb, dir: &Path) -> (Served, String) {
        let path = db.dir().join("db.2cbh");
        (Served::with_dir(app_of([path.clone()]), dir), id_of(&path))
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        // Nothing writes to the bridge's folders once it is dropped (#236).
        settle(&self.app);
        self.app.catalog.explorer.release();
    }
}

/// Waits until none of `app`'s background work runs, for [`WAIT_LIMIT`] at
/// most ([`Catalog::settle`]), so that a test removes the bridge's folders
/// with nothing writing to them (#236). What still runs then fails the test,
/// by name, unless it is failing already.
pub fn settle(app: &App) {
    if let Err(Busy(running)) = app.catalog.settle(WAIT_LIMIT) {
        let what = format!("{} still ran {WAIT_LIMIT:?} after the bridge was dropped", running.join(", "));
        if std::thread::panicking() {
            eprintln!("{what}");
        } else {
            panic!("{what}");
        }
    }
}

/// `GET path` as the page sends it: with the token and an allowed `Origin`,
/// and `Connection: close`.
pub fn request(port: u16, path: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nOrigin: {ORIGIN}\r\nConnection: close\r\n\r\n"
    )
}

/// How long a test waits for the rest of an answer before it fails. Longer
/// than any answer of these tests takes on a loaded machine, it turns a bridge
/// that stops answering, or never closes a connection, into a failure that
/// names the request instead of a run that hangs.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a test's bridge keeps an open connection waiting for a request,
/// in place of [`bridge::http::IDLE_TIMEOUT`]. A loaded machine can stall a
/// test between its connect and its write for longer than that; the bridge
/// then closes the connection unanswered, as it should, and the test reads an
/// empty answer or a reset, or finds the idle connections holding the
/// connection cap gone (#217). No test's contract is that closing.
pub const IDLE_TIMEOUT: Duration = WAIT_LIMIT;

/// A connection to the bridge on `port` whose reads fail after
/// [`ANSWER_TIMEOUT`] without a byte.
pub fn connect(port: u16) -> std::io::Result<TcpStream> {
    let s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(ANSWER_TIMEOUT))?;
    Ok(s)
}

/// Sends `raw` on a new connection and reads the whole answer, head and body;
/// `None` when the connection is refused or cut. An answer that stops for
/// [`ANSWER_TIMEOUT`] before the connection ends fails the test.
pub fn exchange(port: u16, raw: &str) -> Option<String> {
    let mut s = connect(port).ok()?;
    s.write_all(raw.as_bytes()).ok()?;
    let mut out = Vec::new();
    match s.read_to_end(&mut out) {
        Ok(_) => String::from_utf8(out).ok(),
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => panic!(
            "no end of the answer to {:?} within {ANSWER_TIMEOUT:?}: {}",
            raw.lines().next().unwrap_or_default(),
            String::from_utf8_lossy(&out)
        ),
        Err(_) => None,
    }
}

/// An answer: its status, its header lines and its body.
pub struct Reply {
    pub status: u16,
    pub headers: String,
    pub body: String,
}

impl Reply {
    /// The value of header `name`, in whatever case the answer writes it.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.lines().find_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }
}

/// The [`Reply`] of a whole answer; `None` when its head is cut short.
pub fn parse_reply(text: &str) -> Option<Reply> {
    let (head, body) = text.split_once("\r\n\r\n")?;
    let (status_line, headers) = head.split_once("\r\n").unwrap_or((head, ""));
    let status = status_line.split(' ').nth(1)?.parse().ok()?;
    Some(Reply { status, headers: headers.to_string(), body: body.to_string() })
}

/// Sends `raw` on a new connection: its answer.
pub fn send(port: u16, raw: &str) -> Reply {
    let out = exchange(port, raw).expect("the bridge answers");
    parse_reply(&out).unwrap_or_else(|| panic!("not an answer: {out}"))
}

/// The answer to [`request`]`(port, path)`, with its headers.
pub fn get_reply(port: u16, path: &str) -> Reply {
    send(port, &request(port, path))
}

/// The status and body of [`request`]`(port, path)`.
pub fn get(port: u16, path: &str) -> (u16, String) {
    try_get(port, path).expect("the bridge answers")
}

/// [`get`], or `None` when the connection is refused or cut: during a start,
/// the port may belong to another test's bridge that has just ended.
pub fn try_get(port: u16, path: &str) -> Option<(u16, String)> {
    let reply = parse_reply(&exchange(port, &request(port, path))?)?;
    Some((reply.status, reply.body))
}

/// How long a test waits for what it expects before it fails: an index
/// built, a build started or ended, a file written, a thread at its wait.
/// Far longer than any of it takes on a loaded machine, it only turns what
/// never comes into a failure that names it, instead of a run that hangs. A
/// test whose contract is a bound of its own, such as "long before its
/// patience runs out", waits for that bound instead.
pub const WAIT_LIMIT: Duration = Duration::from_secs(300);

/// Asks `ready` every 10 ms until it gives a value, for `limit` at most: the
/// value, or `None` once `limit` has passed without one.
pub fn poll<T>(limit: Duration, mut ready: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(value) = ready() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits until `done` holds, asking every 10 ms, for `limit` at most; then
/// the test fails with `what`.
pub fn until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    assert!(poll(limit, || done().then_some(())).is_some(), "{what} (waited {limit:?})");
}

/// The body of the `200` answer to `path`, asked again every 10 ms while it
/// is `409`, as while the index it needs is built, for [`WAIT_LIMIT`] at
/// most. Any other answer fails the test.
pub fn answered(port: u16, path: &str) -> String {
    let mut last = String::new();
    let body = poll(WAIT_LIMIT, || {
        let (status, body) = get(port, path);
        match status {
            200 => Some(body),
            409 => {
                last = body;
                None
            }
            _ => panic!("{path}: {status} {body}"),
        }
    });
    body.unwrap_or_else(|| panic!("{path} was still {last} after {WAIT_LIMIT:?}"))
}

/// `fen` as the value of a query parameter: its spaces and slashes escaped.
pub fn fen_param(fen: &str) -> String {
    fen.replace(' ', "%20").replace('/', "%2F")
}

/// A body without its `"generation":"…"` member, which differs between copies
/// of a database.
pub fn without_generation(body: &str) -> String {
    match body.find(r#""generation":""#) {
        Some(at) => {
            let end = at + 14 + body[at + 14..].find('"').unwrap() + 1;
            let end = if body[end..].starts_with(',') { end + 1 } else { end };
            format!("{}{}", &body[..at], &body[end..])
        }
        None => body.to_string(),
    }
}

/// The length of the JSON value `text` starts with: a string, an object or an
/// array to its closing mark, any other value to the `,`, `}` or `]` after it.
/// The bridge writes JSON without spaces.
fn value_len(text: &str) -> usize {
    let bytes = text.as_bytes();
    match bytes.first() {
        Some(b'"') => {
            let mut at = 1;
            while bytes[at] != b'"' {
                at += if bytes[at] == b'\\' { 2 } else { 1 };
            }
            at + 1
        }
        Some(b'{' | b'[') => {
            let (mut depth, mut at) = (0, 0);
            loop {
                match bytes[at] {
                    b'"' => {
                        at += value_len(&text[at..]);
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return at + 1;
                        }
                    }
                    _ => {}
                }
                at += 1;
            }
        }
        _ => text.find([',', '}', ']']).unwrap_or(text.len()),
    }
}

/// The values between the brackets `open` and `close` that `text` is, each
/// as written, with its key when `keyed`.
fn items(text: &str, open: char, close: char, keyed: bool) -> Vec<(&str, &str)> {
    let mut rest = text.strip_prefix(open).unwrap_or_else(|| panic!("not {open}…{close}: {text}"));
    let mut out = Vec::new();
    if rest.starts_with(close) {
        return out;
    }
    loop {
        let mut key = "";
        if keyed {
            let len = value_len(rest);
            assert!(rest.starts_with('"') && rest[len..].starts_with(':'), "no key at {rest}");
            key = &rest[1..len - 1];
            rest = &rest[len + 1..];
        }
        let len = value_len(rest);
        out.push((key, &rest[..len]));
        rest = &rest[len..];
        match rest.chars().next() {
            Some(',') => rest = &rest[1..],
            Some(c) if c == close => return out,
            _ => panic!("no {close} closes {text}"),
        }
    }
}

/// The members of the JSON object `object`: each key, as written between its
/// quotes, and its value, as written.
pub fn members(object: &str) -> Vec<(&str, &str)> {
    items(object, '{', '}', true)
}

/// The value of member `key` of the JSON object `object`, as written: a
/// string with its quotes.
pub fn member<'a>(object: &'a str, key: &str) -> &'a str {
    find_member(object, key).unwrap_or_else(|| panic!("no {key} in {object}"))
}

fn find_member<'a>(object: &'a str, key: &str) -> Option<&'a str> {
    members(object).into_iter().find(|(k, _)| *k == key).map(|(_, v)| v)
}

/// The text of the string member `key` of the JSON object `object`, its
/// escapes as written.
pub fn string_member<'a>(object: &'a str, key: &str) -> &'a str {
    let value = member(object, key);
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or_else(|| panic!("{key} is no string in {object}"))
}

/// The objects of the array member `key` of the JSON object `object`, each as
/// its text.
pub fn objects<'a>(object: &'a str, key: &str) -> Vec<&'a str> {
    items(member(object, key), '[', ']', false).into_iter().map(|(_, v)| v).collect()
}

/// Whether the JSON value `actual` matches `expected`: an object has each
/// member of `expected` with a matching value, whatever their order and
/// whatever other members it has; an array has as many elements, each
/// matching; any other value is written alike.
fn matches(actual: &str, expected: &str) -> bool {
    match expected.as_bytes().first() {
        Some(b'{') => {
            actual.starts_with('{')
                && members(expected).iter().all(|(k, v)| find_member(actual, k).is_some_and(|a| matches(a, v)))
        }
        Some(b'[') => {
            let expected = items(expected, '[', ']', false);
            actual.starts_with('[') && {
                let actual = items(actual, '[', ']', false);
                actual.len() == expected.len() && actual.iter().zip(&expected).all(|(a, e)| matches(a.1, e.1))
            }
        }
        _ => actual == expected,
    }
}

/// Whether the JSON object `object` has `wanted`, members as JSON writes them
/// (`"state":"ready","records":3`), each with its value, whatever their order
/// and whatever other members it has: an answer of `docs/api.md` may gain
/// members. A value that is an object is matched by its members the same way,
/// an array element by element.
pub fn has_members(object: &str, wanted: &str) -> bool {
    matches(object, &format!("{{{wanted}}}"))
}

/// The first object of the JSON text `json`, itself or one at any depth in
/// it, that has `wanted` as [`has_members`] reads them.
pub fn object_with<'a>(json: &'a str, wanted: &str) -> Option<&'a str> {
    let wanted = format!("{{{wanted}}}");
    let mut at = 0;
    while at < json.len() {
        match json.as_bytes()[at] {
            b'"' => {
                at += value_len(&json[at..]);
                continue;
            }
            b'{' => {
                let object = &json[at..at + value_len(&json[at..])];
                if matches(object, &wanted) {
                    return Some(object);
                }
            }
            _ => {}
        }
        at += 1;
    }
    None
}

/// Whether an object of the JSON text `json` has `wanted`: see
/// [`object_with`].
pub fn has_object(json: &str, wanted: &str) -> bool {
    object_with(json, wanted).is_some()
}

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
    fixture_of(name, &rows(extra))
}

/// The fixture's rows of the document, then `extra`.
pub fn rows(extra: &[&str]) -> Vec<String> {
    block("fixture")
        .into_iter()
        .filter(|l| !l.starts_with('#'))
        .chain(extra.iter().copied())
        .map(String::from)
        .collect()
}

/// A row of the fixture's form, its columns as the document's `fixture` block
/// names them.
#[derive(Clone, Copy)]
pub struct FixtureRow<'a> {
    pub number: usize,
    /// `game`, `text` (a guiding text), `analysis` or `deleted` (a game
    /// marked deleted).
    pub kind: &'a str,
    /// A name, `Last, First`; `-` for none, as for `black` and `annotator`.
    pub white: &'a str,
    pub black: &'a str,
    /// A game's event; the title of a guiding text or an analysis.
    pub event: &'a str,
    /// As PGN writes it, `??` for an unknown part.
    pub date: &'a str,
    /// `5`, `1(2)` with a sub-round, `-` for none.
    pub round: &'a str,
    pub result: &'a str,
    /// `C52`, `-` for none.
    pub eco: &'a str,
    /// Full moves.
    pub moves: u16,
    /// 0 when unknown.
    pub white_elo: u16,
    pub black_elo: u16,
    pub annotator: &'a str,
}

impl<'a> FixtureRow<'a> {
    pub fn parse(line: &'a str) -> FixtureRow<'a> {
        let f: Vec<&str> = line.split('|').map(str::trim).collect();
        assert_eq!(f.len(), 13, "a fixture row has 13 columns: {line}");
        FixtureRow {
            number: f[0].parse().unwrap(),
            kind: f[1],
            white: f[2],
            black: f[3],
            event: f[4],
            date: f[5],
            round: f[6],
            result: f[7],
            eco: f[8],
            moves: f[9].parse().unwrap(),
            white_elo: f[10].parse().unwrap(),
            black_elo: f[11].parse().unwrap(),
            annotator: f[12],
        }
    }

    /// The date as both ChessBase formats pack it: the year from bit 9, the
    /// month from bit 5 and the day, each 0 when unknown.
    pub fn packed_date(&self) -> u32 {
        let d: Vec<u32> = self.date.split('.').map(|p| p.parse().unwrap_or(0)).collect();
        (d[0] << 9) | (d[1] << 5) | d[2]
    }

    /// The round and the sub-round, 0 for none.
    pub fn round_numbers(&self) -> (u16, u16) {
        match self.round {
            "-" => (0, 0),
            r => match r.split_once('(') {
                Some((r, s)) => (r.parse().unwrap(), s.trim_end_matches(')').parse().unwrap()),
                None => (r.parse().unwrap(), 0),
            },
        }
    }

    /// The result as both ChessBase formats store it.
    pub fn result_code(&self) -> u8 {
        match self.result {
            "0-1" => 0,
            "1/2-1/2" => 1,
            "1-0" => 2,
            _ => 3,
        }
    }

    /// The ECO code as both ChessBase formats store it: 128 times its place
    /// from A00, which is 1; 0 for none.
    pub fn eco_code(&self) -> u16 {
        match self.eco.as_bytes() {
            [l, d1, d2] => (u16::from(l - b'A') * 100 + u16::from(d1 - b'0') * 10 + u16::from(d2 - b'0') + 1) * 128,
            _ => 0,
        }
    }
}

/// The row in its form, as [`FixtureRow::parse`] reads it.
impl std::fmt::Display for FixtureRow<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let r = self;
        write!(
            f,
            "{} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {}",
            r.number,
            r.kind,
            r.white,
            r.black,
            r.event,
            r.date,
            r.round,
            r.result,
            r.eco,
            r.moves,
            r.white_elo,
            r.black_elo,
            r.annotator
        )
    }
}

/// `rows` parsed, which are numbered in order from 1, as the records of a
/// database are.
fn numbered(rows: &[String]) -> Vec<FixtureRow<'_>> {
    let parsed: Vec<FixtureRow> = rows.iter().map(|l| FixtureRow::parse(l)).collect();
    for (i, row) in parsed.iter().enumerate() {
        assert_eq!(row.number, i + 1, "fixture rows are numbered in order");
    }
    parsed
}

/// [`fixture`] of `rows` in its form.
pub fn fixture_of(name: &str, rows: &[String]) -> TempDb {
    let (mut players, mut tournaments, mut titles) = (Names::default(), Names::default(), Names::default());
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    for row in numbered(rows) {
        let rec = b.game(e4);
        let (white, black, annotator) = (players.id(row.white), players.id(row.black), players.id(row.annotator));
        let ids: &[(usize, i64)] = match row.kind {
            "text" => {
                rec[0] |= 2;
                &[(0x20, annotator), (0x28, titles.id(row.event))]
            }
            "analysis" => {
                rec[2] = 2;
                &[(0x18, titles.id(row.event)), (0x28, annotator)]
            }
            kind => {
                if kind == "deleted" {
                    rec[0] |= 0x80;
                }
                &[(0x18, white), (0x20, black), (0x28, tournaments.id(row.event)), (0x30, annotator)]
            }
        };
        for &(at, id) in ids {
            put(rec, at, &id.to_le_bytes());
        }
        if row.kind == "text" || row.kind == "analysis" {
            continue;
        }
        put(rec, 0xbc, &row.packed_date().to_le_bytes());
        let (round, sub) = row.round_numbers();
        put(rec, 0x5a, &round.to_le_bytes());
        put(rec, 0x5c, &sub.to_le_bytes());
        rec[0x58] = row.result_code();
        put(rec, 0x80, &row.eco_code().to_le_bytes());
        put(rec, 0x8a, &row.moves.to_le_bytes());
        put(rec, 0x60, &row.white_elo.to_le_bytes());
        put(rec, 0x70, &row.black_elo.to_le_bytes());
    }
    b.lid(lid(&players.0, &tournaments.0, &titles.0));
    b.write(name)
}

/// `rows` of the fixture's form as a PGN file, `db.pgn`: each a game with the
/// tags its row names and a main line of its row's full moves, `1. e4` and
/// then knights back and forth. A PGN file holds games only.
pub fn pgn_fixture(name: &str, rows: &[String]) -> TempDb {
    let mut text = String::new();
    for row in rows.iter().map(|l| FixtureRow::parse(l)) {
        assert_eq!(row.kind, "game", "a PGN file holds games only");
        let round = row.round.replace('(', ".").replace(')', "");
        let elo = |elo: u16| if elo == 0 { "-".to_string() } else { elo.to_string() };
        let (white_elo, black_elo) = (elo(row.white_elo), elo(row.black_elo));
        let tags = [
            ("Event", row.event),
            ("Date", row.date),
            ("Round", round.as_str()),
            ("White", row.white),
            ("Black", row.black),
            ("Result", row.result),
            ("ECO", row.eco),
            ("WhiteElo", white_elo.as_str()),
            ("BlackElo", black_elo.as_str()),
            ("Annotator", row.annotator),
        ];
        for (tag, value) in tags {
            if value != "-" {
                text.push_str(&format!("[{tag} \"{value}\"]\n"));
            }
        }
        text.push('\n');
        let moves = usize::from(row.moves);
        for ply in 0..(2 * moves).saturating_sub(1) {
            let san = match ply {
                0 => "e4",
                p if p % 2 == 1 => ["Nf6", "Ng8"][(p - 1) / 2 % 2],
                p => ["Nf3", "Ng1"][(p - 2) / 2 % 2],
            };
            if ply % 2 == 0 {
                text.push_str(&format!("{}. ", ply / 2 + 1));
            }
            text.push_str(san);
            text.push(' ');
        }
        text.push_str(row.result);
        text.push_str("\n\n");
    }
    cbformat::fixture::pgn_file(name, text.as_bytes())
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
    let rows = rows(extra);
    for row in numbered(&rows) {
        let annotator = ids.annotator(&mut b, row.annotator);
        if row.kind == "text" {
            put3(b.text(&[(0, row.event.as_bytes())]), 0x0d, annotator);
            continue;
        }
        assert_ne!(row.kind, "analysis", "the classic format has no analyses");
        let (white, black) = (ids.player(&mut b, row.white), ids.player(&mut b, row.black));
        let event = ids.tournament(&mut b, row.event);
        let rec = b.game(&e4);
        if row.kind == "deleted" {
            rec[0] |= 0x80;
        }
        for (at, id) in [(0x09, white), (0x0c, black), (0x0f, event), (0x12, annotator)] {
            put3(rec, at, id);
        }
        put3(rec, 0x18, row.packed_date());
        let (round, sub) = row.round_numbers();
        rec[0x1d] = u8::try_from(round).unwrap();
        rec[0x1e] = u8::try_from(sub).unwrap();
        rec[0x1b] = row.result_code();
        rec[0x23..0x25].copy_from_slice(&row.eco_code().to_be_bytes());
        rec[0x2d] = u8::try_from(row.moves).unwrap();
        rec[0x1f..0x21].copy_from_slice(&row.white_elo.to_be_bytes());
        rec[0x21..0x23].copy_from_slice(&row.black_elo.to_be_bytes());
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

/// `games` standard games of legal moves drawn from `seed`, 20 to 160 plies
/// each, with results by turns and ratings spread: their first moves drawn
/// from a few, so that games share their openings and then part ways, as a
/// real database's do, reaching many positions and structures.
pub fn random_games(name: &str, games: usize, seed: u64) -> TempDb {
    let mut b = Builder::new();
    add_random_games(&mut b, games, seed);
    b.write(name)
}

/// Adds the games of [`random_games`] to `b`.
pub fn add_random_games(b: &mut Builder, games: usize, seed: u64) {
    let mut x = seed | 1;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for g in 0..games {
        let mut board = Board::startpos();
        let mut words = vec![MOVES];
        let plies = 20 + next() % 141;
        for ply in 0..plies {
            let moves = board.legal_moves();
            if moves.is_empty() {
                break;
            }
            let few = if ply < 8 { moves.len().min(3) } else { moves.len() };
            let mv = moves[(next() % few as u64) as usize];
            words.push(cbformat::replay::word_of(&board, mv).unwrap());
            board.play_checked(mv).unwrap();
        }
        words.push(END_OF_LINE);
        let at = b.moves(1, &words);
        let rec = b.game(at);
        rec[0x58] = (g % 3) as u8;
        let elo = 1500 + (next() % 1300) as i16;
        rec[0x60..0x62].copy_from_slice(&elo.to_le_bytes());
    }
}

/// The bytes of the index file or move stream at `path` without its build
/// id, which every build draws afresh, and its header's CRC over it: what two
/// builds of the same games must write alike.
pub fn built_bytes(path: &std::path::Path) -> Vec<u8> {
    let mut bytes = std::fs::read(path).unwrap();
    let id = if path.extension().is_some_and(|e| e == "idx") { 116 } else { 40 };
    bytes[id..id + 8].fill(0);
    bytes[124..128].fill(0);
    bytes
}

/// A database of `records` records of which only the first, a game of 1.e4
/// whose players are both `x`, is written: the rest of the header file is a
/// hole, read as records of zeros, which a file system with sparse files
/// keeps without writing it.
pub fn sparse(name: &str, records: u64) -> TempDb {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    b.game(e4);
    b.lid(lid(&["x".to_string()], &[], &[]));
    let db = b.write(name);
    let file = std::fs::OpenOptions::new().write(true).open(db.dir().join("db.2cbh")).unwrap();
    file.set_len((records + 1) * 192).unwrap();
    db
}

/// Plays `uci` on `board`, castling written as the king's two-square step,
/// which chesscore takes as the king taking its rook.
pub fn play(board: &mut Board, uci: &str) {
    let mut mv: Move = uci.parse().unwrap();
    let king = board.piece_at(mv.from).map(|p| p.0) == Some(chesscore::Piece::King);
    if king && mv.from.file().abs_diff(mv.to.file()) == 2 {
        mv.to = Square::new(if mv.to.file() == 6 { 7 } else { 0 }, mv.from.rank());
    }
    board.play_checked(mv).unwrap();
}

/// The board after `ucis` from the standard start, each move played as
/// [`play`] plays it.
pub fn board_after(ucis: &str) -> Board {
    let mut board = Board::startpos();
    for uci in ucis.split_whitespace() {
        play(&mut board, uci);
    }
    board
}

/// A folder of its own for a test's index files, named after `name`, which
/// no other test of its binary uses: empty, and not made yet.
pub fn index_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bridge-index-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// How long a test run again in a child process may take before the child
/// is killed and the test fails: longer than a child waits for what it
/// expects ([`WAIT_LIMIT`]), so that a child that waits in vain says what
/// for.
pub const CHILD_LIMIT: Duration = Duration::from_secs(900);

/// Whether this process is a child that runs a test's body: one started
/// with the variable `marker` set ([`ChildTest`]).
pub fn is_child(marker: &str) -> bool {
    std::env::var_os(marker).is_some()
}

/// Whether this is the child that runs the test's body. The parent runs the
/// test `name` again in a child process of its own, with `marker` and `env`
/// set ([`ChildTest`]), and checks it passed.
pub fn in_child(name: &str, marker: &str, env: &[(&str, &str)]) -> bool {
    if is_child(marker) {
        return true;
    }
    ChildTest::start(name, marker, env).end().passed();
    false
}

/// The test `name` of this binary run again, alone, in a child process with
/// the variable `marker` set, which tells the child it is one, and `env`:
/// for a test that needs what is read once per process, such as the search
/// budget (`OSCHESS_BRIDGE_SEARCH_MIB`) or the workers
/// (`OSCHESS_BRIDGE_THREADS`), otherwise than its binary's other tests have
/// it, or needs the process to itself. The child writes a log file of its
/// own, as children run beside each other. A child still running when this
/// is dropped, as a failed test unwinds, is killed.
pub struct ChildTest {
    name: String,
    child: Child,
    log: PathBuf,
    started: Instant,
}

impl ChildTest {
    pub fn start(name: &str, marker: &str, env: &[(&str, &str)]) -> ChildTest {
        static STARTED: AtomicUsize = AtomicUsize::new(0);
        let n = STARTED.fetch_add(1, Ordering::Relaxed);
        let log = std::env::temp_dir().join(format!("bridge-child-{}-{n}-{name}.log", std::process::id()));
        let file = std::fs::File::create(&log).unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([name, "--exact", "--nocapture", "--test-threads=1"])
            .env(marker, "1")
            .envs(env.iter().copied())
            .stdout(file.try_clone().unwrap())
            .stderr(file)
            .spawn()
            .unwrap();
        ChildTest { name: name.to_string(), child, log, started: Instant::now() }
    }

    /// Waits for the child to end, until [`CHILD_LIMIT`] after it started,
    /// when it is killed: how it ended, and what it wrote.
    pub fn end(&mut self) -> Ended {
        self.end_within(CHILD_LIMIT)
    }

    /// [`ChildTest::end`] until `limit` after the child started: for a child
    /// whose work takes longer the busier the computer is, which the test
    /// measures (#246).
    pub fn end_within(&mut self, limit: Duration) -> Ended {
        let left = limit.saturating_sub(self.started.elapsed());
        let status = poll(left, || self.child.try_wait().unwrap());
        if status.is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let output = String::from_utf8_lossy(&std::fs::read(&self.log).unwrap_or_default()).into_owned();
        Ended { name: self.name.clone(), status, output, limit }
    }
}

impl Drop for ChildTest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.log);
    }
}

/// How a child's run of its test ended, and what it wrote.
pub struct Ended {
    name: String,
    /// `None` when the child was killed at `limit`.
    status: Option<ExitStatus>,
    output: String,
    limit: Duration,
}

impl Ended {
    /// Checks that the child ran its one test and passed: what it wrote.
    pub fn passed(self) -> String {
        let Some(status) = self.status else {
            panic!("{} did not end within {:?}:\n{}", self.name, self.limit, self.output)
        };
        assert!(status.success() && self.output.contains("1 passed"), "{}: {status}\n{}", self.name, self.output);
        self.output
    }
}

/// The bridge as a separate process under an address-space limit of
/// `limit_kib` KiB, serving the database at `path` with its home in `home`
/// and `env` set: nothing a database or an index file claims may make it
/// allocate beyond its budget or abort. Killed when dropped.
#[cfg(unix)]
pub struct Limited {
    child: Child,
    pub port: u16,
    pub id: String,
}

#[cfg(unix)]
impl Limited {
    /// Starts the bridge on a port found free. Another test's bridge may take
    /// that port first, and this one then fails to bind and ends: the bridge
    /// counts as started only while it runs and lists this database, and
    /// otherwise starts again on another port.
    pub fn start(path: &Path, home: &Path, limit_kib: u64, env: &[(&str, &str)]) -> Limited {
        std::fs::create_dir_all(home).unwrap();
        std::fs::write(home.join("token"), TOKEN).unwrap();
        for _ in 0..10 {
            let port = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
            std::fs::write(home.join("bridge.toml"), format!("port = {port}\n")).unwrap();
            let child = Command::new("sh")
                .arg("-c")
                .arg(format!("ulimit -v {limit_kib} && exec \"$0\" --database \"$1\""))
                .arg(env!("CARGO_BIN_EXE_oschess-bridge"))
                .arg(path)
                .env("OSCHESS_BRIDGE_HOME", home)
                .env("MALLOC_ARENA_MAX", "2")
                .envs(env.iter().copied())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let mut limited = Limited { child, port, id: id_of(path) };
            if limited.serves() {
                return limited;
            }
        }
        panic!("the bridge did not start");
    }

    /// Whether this bridge runs and serves its database, waiting for it to
    /// start, [`WAIT_LIMIT`] at most.
    fn serves(&mut self) -> bool {
        let listed = format!(r#""id":"{}""#, self.id);
        let seen = poll(WAIT_LIMIT, || {
            if self.ended().is_some() {
                return Some(false);
            }
            let (_, body) = try_get(self.port, "/v1/databases")?;
            Some(body.contains(&listed) && self.ended().is_none())
        });
        seen == Some(true)
    }

    /// How the bridge ended; `None` while it runs.
    pub fn ended(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().unwrap()
    }
}

#[cfg(unix)]
impl Drop for Limited {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
