//! `cbtool profile`: the bridge's flows against one database, timed (#83).
//!
//! The database is served by a bridge in a child process, on a free loopback
//! port, and asked over HTTP as oschess asks it, so every time includes the
//! request, the answer's JSON and the socket. The child's own diagnostics,
//! which may name the database and its path, are discarded, and this command
//! prints only timings, counts and the status and code of a failed answer:
//! never a name, a game, a query or a path, so its output can go into an issue
//! as it is. A name the flows need, such as a player to search for, is taken
//! from the bridge's own suggestions and never shown.
//!
//! "cold" is the first request of its kind after the bridge started, when its
//! caches are empty; the operating system may still hold the files in memory.
//! A failed answer is counted as a failure and left out of the times, and any
//! failure makes the command exit with status 1.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use bridge::access::{DEFAULT_ORIGINS, Policy};
use bridge::api::App;
use bridge::catalog::{Catalog, id_of};
use bridge::engine::{Engine, EngineConfig};
use bridge::explorer::runs::{PassTime, Timings};
use bridge::server;
use cbformat::game::{Head, RecordKind};
use cbformat::pgnfile::lex::Lexer;
use cbformat::pgnfile::line::main_line;
use cbformat::view::Base;
use chesscore::Board;

use super::AnyResult;

const TOKEN: &str = "profileprofileprofileprofileprofileprofileP";
/// Runs of each warm measurement, of which the median is shown.
const RUNS: usize = 5;
/// Records scanned for the first and the most annotated game.
const ANNOTATED_SCAN: u32 = 50_000;
/// Lookups along the most played line.
const LOOKUPS: usize = 30;
/// The plies of the most played line whose games are listed (#148), and
/// what their first window takes at most on the Mega Database.
const LIST_LINE_PLIES: [usize; 4] = [0, 6, 12, 20];
const LIST_LINE_TARGET_MS: u32 = 150;
/// The plies of games sampled across the database whose games are listed,
/// and what their first window takes at most.
const LIST_SAMPLED_PLIES: [u32; 2] = [40, 80];
const LIST_SAMPLED_TARGET_MS: u32 = 50;
/// What the next window of a position's games takes at most, read from the
/// result kept.
const LIST_NEXT_TARGET_MS: u32 = 30;
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
const START_FEN: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
/// Bare kings in two corners: the most crowded structure of a large database.
const BARE_KINGS: &str = "7k/8/8/8/8/8/8/K7 w - - 0 1";

struct Options {
    db: PathBuf,
    index: PathBuf,
    engine: Option<PathBuf>,
}

fn options(args: &[String]) -> AnyResult<Options> {
    let mut db = None;
    let (mut index, mut engine) = (None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--index" => index = it.next().map(PathBuf::from),
            "--engine" => engine = it.next().map(PathBuf::from),
            _ if db.is_none() => db = Some(PathBuf::from(a)),
            _ => return Err("unexpected argument".into()),
        }
    }
    let db = db.ok_or("no database given")?;
    let index = index.ok_or("--index <dir> is required: the indexes are built there")?;
    Ok(Options { db, index, engine })
}

/// `cbtool profile-serve <db> <index> [<engine>]`: the bridge `profile` asks,
/// in a process of its own. It prints `port <n>` and serves until killed;
/// once it has built the database's position index, it prints `built`, then
/// where the build's time went ([`Timings::line`]).
pub(crate) fn serve(args: &[String]) -> AnyResult<bool> {
    let [db, index, rest @ ..] = args else { return Err("profile-serve <db> <index> [<engine>]".into()) };
    let listeners = server::bind(0)?;
    let port = listeners[0].local_addr()?.port();
    let engine = match rest.first() {
        Some(exe) => Engine::new(EngineConfig::new(PathBuf::from(exe), None, None)),
        None => Engine::none(),
    };
    let policy = Policy { port, origins: DEFAULT_ORIGINS.iter().map(|o| o.to_string()).collect(), token: TOKEN.into() };
    let app = App { engine, ..App::new("profile", policy, Catalog::new([PathBuf::from(db)])) };
    // The `--index` folder stands for the data folder: every index the bridge
    // builds goes there, never into the real data folder, whose indexes would
    // make the first answers warm.
    app.catalog.use_data_dir(Path::new(index));
    let mut out = std::io::stdout();
    writeln!(out, "port {port}")?;
    out.flush()?;
    let app = Arc::new(app);
    let watched = Arc::clone(&app);
    let id = id_of(Path::new(db));
    std::thread::spawn(move || {
        loop {
            if let Some(t) = watched.catalog.explorer.timings(&id) {
                let mut out = std::io::stdout();
                let _ = writeln!(out, "built {}", t.line()).and_then(|()| out.flush());
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    server::serve(listeners, app)?;
    Ok(true)
}

/// A bridge serving one database in a child process, killed when dropped,
/// and the lines it prints after its port.
struct Served {
    child: Child,
    port: u16,
    id: String,
    lines: mpsc::Receiver<String>,
}

impl Served {
    /// Where the time of the bridge's build of the position index went, once
    /// it tells, waiting `wait` at most.
    fn built(&self, wait: Duration) -> Option<Timings> {
        let until = Instant::now() + wait;
        loop {
            let line = self.lines.recv_timeout(until.saturating_duration_since(Instant::now())).ok()?;
            if let Some(t) = line.strip_prefix("built ").and_then(Timings::parse) {
                return Some(t);
            }
        }
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn(o: &Options) -> AnyResult<Served> {
    let mut command = Command::new(std::env::current_exe()?);
    command.arg("profile-serve").arg(&o.db).arg(&o.index);
    if let Some(exe) = &o.engine {
        command.arg(exe);
    }
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let mut line = String::new();
    let mut out = child.stdout.take().map(BufReader::new);
    let read = out.as_mut().is_some_and(|out| out.read_line(&mut line).is_ok());
    let port = line.trim().strip_prefix("port ").and_then(|p| p.parse().ok()).filter(|_| read);
    let (Some(port), Some(out)) = (port, out) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("the bridge did not start".into());
    };
    // Its later lines, until it ends.
    let (send, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in out.lines().map_while(Result::ok) {
            if send.send(line).is_err() {
                return;
            }
        }
    });
    Ok(Served { child, port, id: id_of(&o.db), lines })
}

/// A failed answer, by its status and the bridge's error code only: a code is
/// a fixed identifier, and the rest of the answer may hold a name or a path.
fn failure(status: u16, body: &[u8]) -> String {
    let code = strings(body, "code").into_iter().next().unwrap_or_default();
    let code: String = code.chars().filter(|c| c.is_ascii_lowercase() || *c == '_').collect();
    if code.is_empty() { format!("{status}") } else { format!("{status} {code}") }
}

/// One HTTP/1.1 connection, kept open between requests unless told otherwise.
struct Client {
    port: u16,
    conn: Option<BufReader<TcpStream>>,
}

impl Client {
    fn new(port: u16) -> Client {
        Client { port, conn: None }
    }

    /// The status and body of `GET path`; a new connection each time when
    /// `keep` is false. A kept connection the bridge closed meanwhile, as it
    /// does after 5 s idle, is opened again once.
    fn get(&mut self, path: &str, keep: bool) -> AnyResult<(u16, Vec<u8>)> {
        let reused = keep && self.conn.is_some();
        match self.request(path, keep, &mut |_| true) {
            Err(_) if reused => {
                self.conn = None;
                self.request(path, keep, &mut |_| true)
            }
            other => other,
        }
    }

    /// `get` on a new connection, handing each line of a streamed body to
    /// `line` as it arrives; `line` returning false ends the request.
    fn stream(&mut self, path: &str, line: &mut dyn FnMut(&[u8]) -> bool) -> AnyResult<u16> {
        self.conn = None;
        let status = self.request(path, false, line)?.0;
        self.conn = None;
        Ok(status)
    }

    fn request(&mut self, path: &str, keep: bool, each: &mut dyn FnMut(&[u8]) -> bool) -> AnyResult<(u16, Vec<u8>)> {
        if !keep {
            self.conn = None;
        }
        let conn = match &mut self.conn {
            Some(c) => c,
            None => self.conn.insert(BufReader::new(TcpStream::connect(("127.0.0.1", self.port))?)),
        };
        let raw = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {TOKEN}\r\nOrigin: {}\r\nConnection: {}\r\n\r\n",
            self.port,
            DEFAULT_ORIGINS[0],
            if keep { "keep-alive" } else { "close" }
        );
        conn.get_mut().write_all(raw.as_bytes())?;
        let mut line = String::new();
        conn.read_line(&mut line)?;
        let status: u16 = line.split(' ').nth(1).and_then(|s| s.parse().ok()).ok_or("no status line")?;
        let (mut length, mut chunked, mut close) = (None, false, !keep);
        loop {
            line.clear();
            conn.read_line(&mut line)?;
            let header = line.trim_end();
            if header.is_empty() {
                break;
            }
            let (name, value) = header.split_once(':').unwrap_or((header, ""));
            let value = value.trim();
            match name.to_ascii_lowercase().as_str() {
                "content-length" => length = value.parse::<usize>().ok(),
                "transfer-encoding" => chunked = value.eq_ignore_ascii_case("chunked"),
                "connection" => close |= value.eq_ignore_ascii_case("close"),
                _ => {}
            }
        }
        let mut body = Vec::new();
        if chunked {
            // Lines are handed on as their chunks arrive, so that a stream is
            // timed line by line.
            let mut pending = Vec::new();
            'chunks: loop {
                line.clear();
                conn.read_line(&mut line)?;
                let size = usize::from_str_radix(line.trim(), 16)?;
                let mut chunk = vec![0; size + 2];
                conn.read_exact(&mut chunk)?;
                if size == 0 {
                    break;
                }
                pending.extend_from_slice(&chunk[..size]);
                body.extend_from_slice(&chunk[..size]);
                while let Some(at) = pending.iter().position(|&b| b == b'\n') {
                    let rest = pending.split_off(at + 1);
                    if !each(&pending[..at]) {
                        close = true;
                        break 'chunks;
                    }
                    pending = rest;
                }
            }
        } else if let Some(n) = length {
            body.resize(n, 0);
            conn.read_exact(&mut body)?;
        } else {
            conn.read_to_end(&mut body)?;
            close = true;
        }
        if close {
            self.conn = None;
        }
        Ok((status, body))
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// The times of the answers that succeeded, and the failures.
#[derive(Default)]
struct Samples {
    times: Vec<f64>,
    failures: Vec<String>,
}

impl Samples {
    /// Times one `GET path`, which must answer 200; its body when it did.
    fn get(&mut self, c: &mut Client, path: &str, keep: bool) -> Option<Vec<u8>> {
        let t = Instant::now();
        match c.get(path, keep) {
            Ok((200, body)) => {
                self.times.push(ms(t.elapsed()));
                Some(body)
            }
            Ok((status, body)) => {
                self.failures.push(failure(status, &body));
                None
            }
            Err(_) => {
                self.failures.push("no answer".into());
                None
            }
        }
    }
}

/// The printed table, which remembers whether any flow failed.
#[derive(Default)]
struct Table {
    failed: bool,
}

impl Table {
    /// One row: the flow, its case, the times of its successful answers in
    /// milliseconds, and counts; failures are counted and named by status.
    fn row(&mut self, flow: &str, case: &str, s: &mut Samples, counts: &str) {
        let mut counts = counts.to_string();
        if !s.failures.is_empty() {
            self.failed = true;
            let first = &s.failures[0];
            counts =
                format!("{} FAILED ({first}){}{counts}", s.failures.len(), if counts.is_empty() { "" } else { ", " });
        }
        let t = &mut s.times;
        if t.is_empty() {
            println!("{flow:<12} {case:<30}   0          -          -          -  {counts}");
            return;
        }
        t.sort_by(f64::total_cmp);
        let (median, min, max) = (t[t.len() / 2], t[0], t[t.len() - 1]);
        println!("{flow:<12} {case:<30} {:>3} {median:>10.1} {min:>10.1} {max:>10.1}  {counts}", t.len());
    }

    /// A row of one measured time.
    fn once(&mut self, flow: &str, case: &str, took: f64, counts: &str) {
        self.row(flow, case, &mut Samples { times: vec![took], failures: Vec::new() }, counts);
    }

    fn failure(&mut self, flow: &str, case: &str, why: &str) {
        self.row(flow, case, &mut Samples { times: Vec::new(), failures: vec![why.into()] }, "");
    }
}

/// A number member `"key":123` of a JSON text.
fn number(json: &[u8], key: &str) -> Option<u64> {
    let text = std::str::from_utf8(json).ok()?;
    let at = text.find(&format!("\"{key}\":"))? + key.len() + 3;
    let digits: String = text[at..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The rows of a games window.
fn row_count(json: &[u8]) -> usize {
    String::from_utf8_lossy(json).matches("{\"number\":").count()
}

/// Every string member `"key":"…"` of a JSON text, unescaped.
fn strings(json: &[u8], key: &str) -> Vec<String> {
    let text = String::from_utf8_lossy(json);
    let pat = format!("\"{key}\":\"");
    let mut out = Vec::new();
    let mut rest = &text[..];
    while let Some(at) = rest.find(&pat) {
        rest = &rest[at + pat.len()..];
        let mut value = String::new();
        let mut chars = rest.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => match chars.next() {
                    Some('n') => value.push('\n'),
                    Some('t') => value.push('\t'),
                    Some('r') => value.push('\r'),
                    Some('b') => value.push('\u{8}'),
                    Some('f') => value.push('\u{c}'),
                    Some('u') => {
                        let hex = |chars: &mut std::str::Chars<'_>| {
                            let h: String = chars.by_ref().take(4).collect();
                            u32::from_str_radix(&h, 16).ok()
                        };
                        let Some(mut code) = hex(&mut chars) else { break };
                        if (0xd800..0xdc00).contains(&code) {
                            // A surrogate pair: `\uD8xx\uDCxx`.
                            let low = match (chars.next(), chars.next()) {
                                (Some('\\'), Some('u')) => hex(&mut chars).filter(|l| (0xdc00..0xe000).contains(l)),
                                _ => None,
                            };
                            code = low.map_or(0xfffd, |low| 0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00));
                        }
                        value.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                    }
                    Some(other) => value.push(other),
                    None => break,
                },
                c => value.push(c),
            }
        }
        out.push(value);
    }
    out
}

/// The objects of the first array member `key` of a JSON text, each as its
/// text: a scan that keeps to strings and nesting. Empty when there is none.
fn objects(json: &[u8], key: &str) -> Vec<String> {
    let text = String::from_utf8_lossy(json);
    let open = format!("\"{key}\":[");
    let Some(at) = text.find(&open) else { return Vec::new() };
    let rest = &text[at + open.len()..];
    let (mut out, mut depth, mut from, mut in_string, mut escaped) = (Vec::new(), 0u32, 0, false, false);
    for (at, c) in rest.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            _ if in_string => {}
            '{' => {
                if depth == 0 {
                    from = at;
                }
                depth += 1;
            }
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    out.push(rest[from..=at].to_string());
                }
            }
            ']' if depth == 0 => break,
            _ => {}
        }
    }
    out
}

/// Whether a `topGames` entry is `row`, member for member, and then `year`.
fn row_and_year(entry: &str, row: &str) -> bool {
    row.strip_suffix('}').and_then(|open| entry.strip_prefix(open)).is_some_and(|rest| rest.starts_with(",\"year\":"))
}

/// A query parameter's value, percent-encoded.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => char::from(b).to_string(),
            b => format!("%{b:02X}"),
        })
        .collect()
}

/// The legal move the explorer names in UCI, castling as the king's step.
fn find_move(board: &Board, uci: &str) -> Option<chesscore::Move> {
    board.legal_moves().into_iter().find(|&m| bridge::explorer::uci(board, m) == uci)
}

/// The games, plies and bytes of the move stream at `path`, from its header.
fn stream_counts(path: &Path) -> String {
    use bridge::explorer::stream::{HEADER_LEN, Header};
    let mut head = [0u8; HEADER_LEN];
    let read = std::fs::File::open(path).and_then(|mut f| {
        f.read_exact(&mut head)?;
        f.metadata().map(|m| m.len())
    });
    match (read, Header::decode(&head)) {
        (Ok(bytes), Some(h)) => format!("stream {} games, {} plies, {bytes} bytes", h.games, h.plies),
        _ => "no move stream".into(),
    }
}

/// A row for each phase of the index's build (#147): the stream pass, the
/// tree's passes and the deep section's, each pass's replay and write, then
/// the index file's end and the renames.
fn build_phases(table: &mut Table, t: &Timings) {
    let passes = |all: &[PassTime]| {
        let (replay, write): (Duration, Duration) =
            (all.iter().map(|p| p.replay).sum(), all.iter().map(|p| p.write).sum());
        let each: Vec<String> = all.iter().map(|p| format!("{:.0}+{:.0}", ms(p.replay), ms(p.write))).collect();
        let counts = format!(
            "passes {}, replay {:.0} ms, write {:.0} ms; each replay+write ms: {}",
            all.len(),
            ms(replay),
            ms(write),
            each.join(" ")
        );
        (ms(replay + write), counts)
    };
    table.once("index", "build: stream pass", ms(t.reading), "games read, move stream written");
    let (tree, counts) = passes(&t.tree);
    table.once("index", "build: tree passes", tree, &counts);
    let (deep, counts) = passes(&t.deep);
    table.once("index", "build: deep passes", deep, &counts);
    table.once("index", "build: index file end", ms(t.closing), "header written, file synced");
    table.once("index", "build: renames", ms(t.renaming), &format!("phases {:.0} ms in all", ms(t.total())));
}

/// Asks `path`, an explorer request, until it is answered: `Ok` once it is,
/// counting the answers that the index is being built in `polls`.
fn build_index(c: &mut Client, path: &str, polls: &mut u64) -> Result<(), String> {
    loop {
        match c.get(path, true) {
            Ok((200, _)) => return Ok(()),
            // `409 database_unavailable` with `state: "indexing"` while the
            // index is built; `503 index_unavailable` when its build failed.
            Ok((409, body)) if strings(&body, "state").iter().any(|s| s == "indexing") => {
                *polls += 1;
                std::thread::sleep(Duration::from_millis(250));
            }
            Ok((status, body)) => return Err(failure(status, &body)),
            Err(_) => return Err("no answer".to_string()),
        }
    }
}

/// The bytes the files in `dir` and its folders take together; 0 for a
/// folder that cannot be listed.
fn folder_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => folder_bytes(&e.path()),
            _ => e.metadata().map_or(0, |m| m.len()),
        })
        .sum()
}

/// Whether `dir` is missing or empty, so that the index is built in it; a
/// folder that cannot be listed may hold an index and is neither.
fn fresh(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

pub(crate) fn run(args: &[String]) -> AnyResult<bool> {
    let o = options(args)?;
    if !fresh(&o.index) {
        return Err("--index must name a new or empty folder, so that the index build is measured".into());
    }
    std::fs::create_dir_all(&o.index)?;
    let mut table = Table::default();
    println!("{:<12} {:<30} {:>3} {:>10} {:>10} {:>10}  counts", "flow", "case", "n", "median ms", "min ms", "max ms");

    // Opening: the bridge starts, then its first answers open the database.
    let t = Instant::now();
    let served = spawn(&o)?;
    let start = ms(t.elapsed());
    let base = format!("/v1/databases/{}", served.id);
    let mut c = Client::new(served.port);
    let mut first = Samples::default();
    first.get(&mut c, "/v1/status", true);
    table.once("opening", "bridge process start", start, "");
    table.row("opening", "first status", &mut first, "");
    let mut first_list = Samples::default();
    let records = first_list.get(&mut c, "/v1/databases", true).and_then(|list| number(&list, "records"));
    let Some(records) = records else {
        table.row("opening", "first database list", &mut first_list, "the database is not ready");
        return Ok(false);
    };
    table.row("opening", "first database list", &mut first_list, &format!("{records} records"));
    for (case, path) in [("status", "/v1/status"), ("database list", "/v1/databases")] {
        let mut s = Samples::default();
        for _ in 0..RUNS {
            s.get(&mut c, path, true);
        }
        table.row("opening", case, &mut s, "");
    }

    // Sorts: the first order of each key over all records, then cached.
    for key in SORT_KEYS {
        let path = format!("{base}/games?sort={key}&limit=500");
        let mut cold = Samples::default();
        let total = cold.get(&mut c, &path, true).and_then(|body| number(&body, "total"));
        let mut warm = Samples::default();
        for _ in 0..RUNS {
            warm.get(&mut c, &path, true);
        }
        table.row("sort", &format!("{key} cold"), &mut cold, &total.map_or(String::new(), |t| format!("{t} rows")));
        table.row("sort", &format!("{key} cached"), &mut warm, "");
    }

    // Windows of 500 rows at the start, the middle and the end, with and
    // without the main line.
    let last = records.saturating_sub(500);
    for (place, offset) in [("start", 0), ("middle", records / 2), ("end", last)] {
        for (form, extra) in [("", ""), (" line=60", "&line=60")] {
            let path = format!("{base}/games?offset={offset}&limit=500{extra}");
            let mut s = Samples::default();
            let mut rows = None;
            for _ in 0..RUNS {
                if let Some(body) = s.get(&mut c, &path, true) {
                    rows.get_or_insert(row_count(&body));
                }
            }
            let counts = rows.map_or(String::new(), |r| format!("{r} rows"));
            table.row("window", &format!("{place}{form}"), &mut s, &counts);
        }
    }

    // Player suggestions for prefixes of one to three letters. The first
    // request of a field builds its name tables.
    let mut player = None;
    for (i, prefix) in ["m", "mo", "mor"].iter().enumerate() {
        let path = format!("{base}/suggest?field=player&prefix={prefix}");
        let mut first = Samples::default();
        let mut body = first.get(&mut c, &path, true);
        if i == 0 {
            player = body.as_ref().and_then(|b| strings(b, "value").into_iter().next());
            table.row("suggest", "player, first ever", &mut first, "");
        }
        // A longer prefix's first answer is not timed apart, but its failure
        // counts with the cached ones.
        let failures = if i == 0 { Vec::new() } else { first.failures };
        let mut s = Samples { times: Vec::new(), failures };
        for _ in 0..RUNS {
            let answer = s.get(&mut c, &path, true);
            body = body.or(answer);
        }
        let names = body.map_or(String::new(), |b| format!("{} names", strings(&b, "value").len()));
        table.row("suggest", &format!("player, {} letters", prefix.len()), &mut s, &names);
    }
    let mut events = Samples::default();
    let event = events
        .get(&mut c, &format!("{base}/suggest?field=event&prefix=o"), true)
        .and_then(|b| strings(&b, "value").into_iter().next());

    // Searches by qualifier, cold and then cached. The names come from the
    // suggestions above and are not printed.
    let mut searches = vec![("date", "date:2020".to_string())];
    match &player {
        Some(p) => searches.extend(["player", "white", "black"].map(|q| (q, format!("{q}:\"{p}\"")))),
        None => table.row("search", "player", &mut Samples::default(), "no player suggested for m"),
    }
    match &event {
        Some(e) => searches.push(("event", format!("event:\"{e}\""))),
        None => table.row("search", "event", &mut events, "no event suggested for o"),
    }
    for (name, q) in &searches {
        let path = format!("{base}/games?limit=500&q={}", encode(q));
        let mut cold = Samples::default();
        let total = cold.get(&mut c, &path, true).and_then(|body| number(&body, "total"));
        let mut warm = Samples::default();
        for _ in 0..RUNS {
            warm.get(&mut c, &path, true);
        }
        table.row(
            "search",
            &format!("{name} cold"),
            &mut cold,
            &total.map_or(String::new(), |t| format!("{t} matches")),
        );
        table.row("search", &format!("{name} cached"), &mut warm, "");
    }

    // One game as PGN: the first game, and the most annotated one of the first
    // records, in the reading and the full form.
    let (first_game, annotated, notes) = scan_games(&o)?;
    for (which, number) in [("first game", first_game), ("most annotated", annotated)] {
        for (form, extra) in [("reading", ""), ("full", "?annotations=full")] {
            let path = format!("{base}/games/{number}{extra}");
            let mut cold = Samples::default();
            let bytes = cold.get(&mut c, &path, true).map(|b| b.len());
            let mut warm = Samples::default();
            for _ in 0..RUNS {
                warm.get(&mut c, &path, true);
            }
            let mut counts = bytes.map_or(String::new(), |b| format!("{b} bytes"));
            if number == annotated && bytes.is_some() {
                counts = format!("{counts}, {notes} annotations");
            }
            table.row("pgn", &format!("{which}, {form} cold"), &mut cold, &counts);
            table.row("pgn", &format!("{which}, {form}"), &mut warm, "");
        }
    }

    // The position index: its build in the new folder, a lookup per move
    // along the most played line, and opening it again in a new bridge. The
    // index folder is measured while the build writes it (#147): at its
    // largest, and once the build is done.
    let explorer = |fen: &str| format!("{base}/explorer?fen={}", encode(fen));
    let folder = o.index.join("index");
    let building = std::sync::atomic::AtomicBool::new(true);
    let t = Instant::now();
    let mut polls = 0u64;
    let (built, peak) = std::thread::scope(|s| {
        let sampler = s.spawn(|| {
            let mut peak = 0;
            while building.load(std::sync::atomic::Ordering::Relaxed) {
                peak = peak.max(folder_bytes(&folder));
                std::thread::sleep(Duration::from_millis(20));
            }
            peak.max(folder_bytes(&folder))
        });
        let built = build_index(&mut c, &explorer(START_FEN), &mut polls);
        building.store(false, std::sync::atomic::Ordering::Relaxed);
        (built, sampler.join().unwrap_or(0))
    });
    match built {
        Ok(()) => {
            let took = ms(t.elapsed());
            let stream = stream_counts(&folder.join(format!("{}.moves", served.id)));
            let index = std::fs::metadata(folder.join(format!("{}.idx", served.id))).map_or(0, |m| m.len());
            let sizes =
                format!("index {index} bytes, folder at most {peak} bytes, {} bytes after", folder_bytes(&folder));
            table.once("index", "build to first answer", took, &format!("{polls} polls, {stream}, {sizes}"));
            match served.built(Duration::from_secs(10)) {
                Some(t) => build_phases(&mut table, &t),
                None => table.failure("index", "build phases", "not told"),
            }
        }
        Err(why) => table.failure("index", "build to first answer", &why),
    }
    let mut board = Board::startpos();
    let mut lookups = Samples::default();
    let mut played = 0;
    let mut stop = "";
    let mut notable = Vec::new();
    // The positions of the line whose games are listed below, by ply.
    let mut listed: Vec<(String, u32, Vec<String>)> =
        LIST_LINE_PLIES.iter().map(|p| (format!("line ply {p}"), LIST_LINE_TARGET_MS, Vec::new())).collect();
    for _ in 0..LOOKUPS {
        if let Some(at) = LIST_LINE_PLIES.iter().position(|&p| p == played) {
            listed[at].2.push(board.fen());
        }
        let Some(body) = lookups.get(&mut c, &explorer(&board.fen()), true) else { break };
        notable.extend(objects(&body, "topGames"));
        let Some(uci) = strings(&body, "uci").into_iter().next() else {
            stop = ", no move from the last position";
            break;
        };
        let Some(mv) = find_move(&board, &uci) else {
            stop = ", a named move is not legal here";
            table.failed = true;
            break;
        };
        board.play_unchecked(mv);
        played += 1;
    }
    let counts = format!("{} lookups, {played} plies played{stop}", lookups.times.len());
    table.row("index", "lookup per move", &mut lookups, &counts);
    // Deep positions (#133): positions of games from across the database at
    // plies 30, 60 and 90, past the tree's pruning ply and past its depth;
    // each must find at least its own game. Their positions at plies 40 and
    // 80 are kept for the lists below.
    let mut deep = Samples::default();
    let (mut asked, mut found) = (0, 0);
    let mut lexer = Lexer::new();
    let mut sampled: Vec<(String, u32, Vec<String>)> =
        LIST_SAMPLED_PLIES.iter().map(|p| (format!("sampled ply {p}"), LIST_SAMPLED_TARGET_MS, Vec::new())).collect();
    for k in 1..=8u64 {
        let n = (records / 9 * k).max(1);
        let Some((200, body)) = c.get(&format!("{base}/games/{n}"), true).ok() else { continue };
        let Some(pgn) = strings(&body, "pgn").into_iter().next() else { continue };
        let (mut fens, mut ply) = (Vec::new(), 0u32);
        main_line(pgn.as_bytes(), &mut lexer, &mut |board, mv| {
            if [30, 60, 90].contains(&ply) {
                fens.push(board.fen());
            }
            if let Some(at) = LIST_SAMPLED_PLIES.iter().position(|&p| p == ply) {
                sampled[at].2.push(board.fen());
            }
            ply += 1;
            mv.is_some()
        });
        for fen in fens {
            asked += 1;
            if deep.get(&mut c, &explorer(&fen), true).is_some_and(|a| number(&a, "games").is_some_and(|g| g > 0)) {
                found += 1;
            }
        }
    }
    if found < asked {
        table.failed = true;
    }
    table.row("index", "deep lookup, plies 30/60/90", &mut deep, &format!("{found} of {asked} found"));
    // The most crowded structure (#145): bare kings, which every game that
    // ends in them holds, all home pawns gone, so their bucket is replayed
    // whole.
    let bare = explorer(BARE_KINGS);
    let (mut first, mut crowded) = (Samples::default(), Samples::default());
    let games = first.get(&mut c, &bare, true).and_then(|a| number(&a, "games"));
    for _ in 0..RUNS {
        crowded.get(&mut c, &bare, true);
    }
    let games = games.map_or(String::new(), |g| format!("{g} games reach it"));
    table.row("index", "crowded structure, first", &mut first, &games);
    table.row("index", "crowded structure", &mut crowded, "");
    listed.extend(sampled);
    list_positions(&mut table, &mut c, &base, &listed);
    // Each notable game the lookups named is its `/games` row whole, then its
    // year (#144): compared with its number's row, asked for once a number
    // after the lookups, so that their times stay as they were.
    let mut rows = Samples::default();
    let mut fetched: HashMap<u64, Option<String>> = HashMap::new();
    let mut whole = 0;
    for entry in &notable {
        let Some(n) = number(entry.as_bytes(), "number") else { continue };
        let row = fetched.entry(n).or_insert_with(|| {
            let body = rows.get(&mut c, &format!("{base}/games?offset={}&limit=1", n.saturating_sub(1)), true)?;
            objects(&body, "rows").into_iter().next()
        });
        if row.as_deref().is_some_and(|row| row_and_year(entry, row)) {
            whole += 1;
        }
    }
    if whole < notable.len() {
        table.failed = true;
    }
    let counts = format!("topGames entries with every row field: {whole} of {}", notable.len());
    table.row("index", "notable games' rows", &mut rows, &counts);
    drop(served);
    let t = Instant::now();
    let again = spawn(&o)?;
    let mut fresh_client = Client::new(again.port);
    let mut open = Samples::default();
    open.get(&mut fresh_client, &format!("/v1/databases/{}/explorer?fen={}", again.id, encode(START_FEN)), true);
    let took = ms(t.elapsed());
    if open.failures.is_empty() {
        table.once("index", "new bridge, first answer", took, "");
    } else {
        table.row("index", "new bridge, first answer", &mut open, "");
    }

    // The heads file (#106): the first bridge built it after its first sort.
    // The new one, its caches empty, answers its first sorts, suggestion and
    // searches from it.
    let heads = o.index.join("index").join(format!("{}.heads", again.id));
    let present = if heads.exists() { "from the heads file" } else { "no heads file" };
    let base = format!("/v1/databases/{}", again.id);
    for key in SORT_KEYS {
        let mut cold = Samples::default();
        cold.get(&mut fresh_client, &format!("{base}/games?sort={key}&limit=500"), true);
        table.row("sort+heads", &format!("{key} cold"), &mut cold, present);
    }
    let mut suggestion = Samples::default();
    suggestion.get(&mut fresh_client, &format!("{base}/suggest?field=player&prefix=m"), true);
    table.row("suggest+heads", "player, first ever", &mut suggestion, present);
    for (name, q) in &searches {
        let mut cold = Samples::default();
        cold.get(&mut fresh_client, &format!("{base}/games?limit=500&q={}", encode(q)), true);
        table.row("search+heads", &format!("{name} cold"), &mut cold, present);
    }

    // The names files (#108): the bridge above wrote its name tables beside
    // the heads file. A third bridge reads them from there for its first sort
    // by white, suggestion and player search.
    let names: Vec<PathBuf> = ["players", "tournaments"].iter().map(|k| heads.with_extension(k)).collect();
    let waited = Instant::now();
    while !names.iter().all(|p| p.exists()) && waited.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(100));
    }
    let have = if names.iter().all(|p| p.exists()) { "from the names files" } else { "no names files" };
    let third = spawn(&o)?;
    let mut third_client = Client::new(third.port);
    let base = format!("/v1/databases/{}", third.id);
    for key in ["white", "tournament"] {
        let mut cold = Samples::default();
        cold.get(&mut third_client, &format!("{base}/games?sort={key}&limit=500"), true);
        table.row("sort+names", &format!("{key} cold"), &mut cold, have);
    }
    let fourth = spawn(&o)?;
    let mut fourth_client = Client::new(fourth.port);
    let base = format!("/v1/databases/{}", fourth.id);
    let mut suggestion = Samples::default();
    suggestion.get(&mut fourth_client, &format!("{base}/suggest?field=player&prefix=m"), true);
    table.row("suggest+names", "player, first after start", &mut suggestion, have);
    if let Some((_, q)) = searches.iter().find(|(name, _)| *name == "player") {
        let fifth = spawn(&o)?;
        let mut fifth_client = Client::new(fifth.port);
        let mut cold = Samples::default();
        let path = format!("/v1/databases/{}/games?limit=500&q={}", fifth.id, encode(q));
        cold.get(&mut fifth_client, &path, true);
        table.row("search+names", "player, first after start", &mut cold, have);
    }

    // The engine, when one is given: the first line of a 5-second search, and
    // the lines a second after it.
    if o.engine.is_some() {
        engine(&mut table, &mut fresh_client, "first line");
        // A new bridge whose engine is warmed up first (#110), as the web app
        // does when an analysis page opens: the warm-up, then the first line.
        let warmed = spawn(&o)?;
        let mut warm_client = Client::new(warmed.port);
        let mut warm = Samples::default();
        warm.get(&mut warm_client, "/v1/engine/warm", true);
        table.row("engine", "warm-up to ready", &mut warm, "");
        engine(&mut table, &mut warm_client, "first line after a warm-up");
    }

    // HTTP: a small answer over one kept connection, and over a new one each.
    for (case, keep) in [("keep-alive", true), ("new connection", false)] {
        let mut s = Samples::default();
        for _ in 0..200 {
            s.get(&mut fresh_client, "/v1/status", keep);
        }
        table.row("http", &format!("status, {case}"), &mut s, "");
    }
    Ok(!table.failed)
}

/// The games of a position (#148), for each case's positions: the first
/// window of 200 by date, which finds the position's games, then the next
/// one, read from the result kept. The date order is cached by the sorts
/// before. Each list's `total` must be the explorer's `games` for the same
/// position. The counts name what each window takes at most on the Mega
/// Database.
fn list_positions(table: &mut Table, c: &mut Client, base: &str, cases: &[(String, u32, Vec<String>)]) {
    for (case, target, fens) in cases {
        let (mut first, mut next) = (Samples::default(), Samples::default());
        let (mut equal, mut totals) = (0, Vec::new());
        for fen in fens {
            let path = format!("{base}/games?fen={}&sort=date&limit=200", encode(fen));
            let total = first.get(c, &path, true).and_then(|body| number(&body, "total"));
            for _ in 0..RUNS {
                next.get(c, &format!("{path}&offset=200"), true);
            }
            let games = match c.get(&format!("{base}/explorer?fen={}", encode(fen)), true) {
                Ok((200, body)) => number(&body, "games"),
                _ => None,
            };
            if total.is_some() && total == games {
                equal += 1;
            }
            totals.push(total.map_or("-".to_string(), |t| t.to_string()));
        }
        let counts = match fens.len() {
            0 => "no such position".to_string(),
            n => {
                if equal < n {
                    table.failed = true;
                }
                format!("total = explorer's games for {equal} of {n}; totals {}; target {target} ms", totals.join(" "))
            }
        };
        table.row("list", &format!("{case}, first page"), &mut first, &counts);
        if !fens.is_empty() {
            table.row("list", &format!("{case}, next page"), &mut next, &format!("target {LIST_NEXT_TARGET_MS} ms"));
        }
    }
}

/// A 5-second search from the start position, read line by line: the time to
/// its first line, and the lines a second from then to its best move.
fn engine(table: &mut Table, c: &mut Client, first_case: &str) {
    let t = Instant::now();
    let (mut first, mut last, mut lines) = (None, None, 0u32);
    let mut error = None;
    let mut done = false;
    let status = c.stream("/v1/engine/analyze?movetime=5000&stream=profile", &mut |line| {
        let at = ms(t.elapsed());
        if line.starts_with(b"{\"info\"") {
            first.get_or_insert(at);
            last = Some(at);
            lines += 1;
            true
        } else if line.starts_with(b"{\"bestmove\"") {
            last = Some(at);
            done = true;
            false
        } else {
            error = Some(failure(200, line));
            false
        }
    });
    match (status, first, error) {
        (Ok(200), Some(first), None) if done => {
            let span = last.unwrap_or(first) - first;
            let rate = if span > 0.0 { f64::from(lines.saturating_sub(1)) * 1000.0 / span } else { 0.0 };
            table.once("engine", first_case, first, "");
            table.once(
                "engine",
                "5 s search to best move",
                last.unwrap_or(first),
                &format!("{lines} lines, {rate:.1} a second"),
            );
        }
        (Ok(200), _, Some(why)) => table.failure("engine", "5 s search", &why),
        (Ok(200), _, None) => table.failure("engine", "5 s search", "no line or no best move"),
        (Ok(status), _, _) => table.failure("engine", "5 s search", &format!("{status}")),
        (Err(_), _, _) => table.failure("engine", "5 s search", "no answer"),
    }
}

/// The first game among the first records, which need not be record 1 (a
/// database may open with guiding texts), the most annotated of them, and how
/// many annotations it has.
fn scan_games(o: &Options) -> AnyResult<(u32, u32, usize)> {
    // A reader's error names the file; this command names none.
    let unread = |_| "the database's first records could not be read";
    let db = Base::open(&o.db).map_err(unread)?;
    let last = db.record_count().min(ANNOTATED_SCAN);
    let mut first_game = None;
    let mut best = (1, 0);
    let mut first = 1;
    while first <= last {
        let upto = last.min(first + 4095);
        for h in db.headers(first, upto).map_err(unread)? {
            if h.kind() != RecordKind::Game || h.is_deleted() {
                continue;
            }
            first_game.get_or_insert(h.id());
            if let Ok(Some(a)) = db.annotations_of(&h) {
                let n: usize = a.blocks.iter().map(|b| b.annotations.len()).sum();
                if n > best.1 {
                    best = (h.id(), n);
                }
            }
        }
        first = upto + 1;
    }
    let first_game = first_game.ok_or("no game among the first records")?;
    if best.1 == 0 {
        best.0 = first_game;
    }
    Ok((first_game, best.0, best.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_members() {
        let json = br#"{"total":1234,"rows":[{"value":"Tal, Mikhail"},{"value":"Caf\u00e9 \"X\" \ud83d\ude00"}]}"#;
        assert_eq!(number(json, "total"), Some(1234));
        assert_eq!(number(json, "none"), None);
        assert_eq!(strings(json, "value"), ["Tal, Mikhail", "Caf\u{e9} \"X\" \u{1f600}"]);
        // A lone or broken surrogate is a replacement character, not a panic.
        assert_eq!(strings(br#"{"value":"\ud83d\u0041"}"#, "value"), ["\u{fffd}"]);
        assert_eq!(row_count(br#"{"total":9,"rows":[{"number":1,"a":2},{"number":7}]}"#), 2);
    }

    #[test]
    fn notable_games_are_rows_and_their_year() {
        let answer = br#"{"topGames":[{"number":7,"white":"A \"}{[\\","flags":{"deleted":false},"year":null},{"number":9,"year":1951}],"index":{"records":9}}"#;
        let top = objects(answer, "topGames");
        assert_eq!(
            top,
            [
                r#"{"number":7,"white":"A \"}{[\\","flags":{"deleted":false},"year":null}"#,
                r#"{"number":9,"year":1951}"#
            ]
        );
        assert!(objects(answer, "rows").is_empty());
        let row = r#"{"number":7,"white":"A \"}{[\\","flags":{"deleted":false}}"#;
        assert_eq!(objects(format!(r#"{{"rows":[{row}]}}"#).as_bytes(), "rows"), [row]);
        assert!(row_and_year(&top[0], row));
        // A member left out or changed, or no year after the row's members.
        assert!(!row_and_year(&top[0], r#"{"number":7,"white":"A \"}{[\\","flags":{"deleted":true}}"#));
        assert!(!row_and_year(&top[0], r#"{"number":7,"white":"A \"}{[\\","flags":{"deleted":false},"site":""}"#));
        assert!(!row_and_year(row, row));
    }

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(encode("player:\"Tal, M\""), "player%3A%22Tal%2C%20M%22");
        assert_eq!(encode("é"), "%C3%A9");
    }

    /// A failure is its status and the bridge's code, never the message, which
    /// can hold a name or a path.
    #[test]
    fn a_failure_shows_only_its_status_and_code() {
        let body = br#"{"error":{"code":"index_failed","message":"PRIVATE_SENTINEL at C:\\Users\\x"}}"#;
        assert_eq!(failure(409, body), "409 index_failed");
        assert_eq!(failure(503, b"<html>C:\\secret</html>"), "503");
        assert_eq!(failure(422, br#"{"error":{"code":"C:\\Users\\x"}}"#), "422 sersx");
    }

    #[test]
    fn castling_is_found_by_the_kings_step() {
        let board = Board::from_fen("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1").unwrap();
        let short = find_move(&board, "e1g1").unwrap();
        let long = find_move(&board, "e1c1").unwrap();
        assert_eq!(bridge::explorer::uci(&board, short), "e1g1");
        assert_eq!(bridge::explorer::uci(&board, long), "e1c1");
        assert!(find_move(&board, "e1e5").is_none());
    }

    /// The stream's counts come from its header; a missing or foreign file
    /// is named as such.
    #[test]
    fn the_move_streams_counts_are_read_from_its_header() {
        use bridge::explorer::stream::Header;
        let path = std::env::temp_dir().join(format!("cbtool-profile-stream-{}", std::process::id()));
        assert_eq!(stream_counts(&path), "no move stream");
        let h = Header {
            first_record: 1,
            last_record: 3,
            generation: 1,
            build_id: 2,
            games: 3,
            plies: 120,
            table_offset: 320,
            blocks: 1,
            table_crc: 0,
        };
        let mut bytes = h.encode().to_vec();
        bytes.resize(328, 0);
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(stream_counts(&path), "stream 3 games, 120 plies, 328 bytes");
        std::fs::write(&path, b"not a stream").unwrap();
        assert_eq!(stream_counts(&path), "no move stream");
        let _ = std::fs::remove_file(&path);
    }

    /// A folder's bytes are its files' and its folders' files'.
    #[test]
    fn a_folders_bytes_count_every_file_in_it() {
        let dir = std::env::temp_dir().join(format!("cbtool-profile-folder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(folder_bytes(&dir), 0);
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        std::fs::write(dir.join("a.idx"), [0u8; 100]).unwrap();
        std::fs::write(dir.join("inner").join("b"), [0u8; 20]).unwrap();
        assert_eq!(folder_bytes(&dir), 120);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_a_new_or_empty_index_folder_is_fresh() {
        let dir = std::env::temp_dir().join(format!("cbtool-profile-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(fresh(&dir));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(fresh(&dir));
        std::fs::write(dir.join("x.idx"), b"x").unwrap();
        assert!(!fresh(&dir));
        // A folder that can be entered but not listed may hold an index; as
        // root it can still be listed, and the case does not arise.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o111)).unwrap();
            if std::fs::read_dir(&dir).is_err() {
                assert!(!fresh(&dir));
            }
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
