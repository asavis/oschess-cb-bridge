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
//!
//! With `--background`, the first bridge keeps its indexes as a bridge that
//! serves does (#149): the database, the largest one ready, has its position
//! index built unasked from the start, while the flows run. Each sort and
//! search row says whether the build still ran when it was done, and the time
//! from the bridge's start to the index ready, with the mode the build ran in
//! (`OSCHESS_BRIDGE_BACKGROUND_MODE`) and how long it gave way to the flows at
//! most at a time, replaces the build the first explorer request starts.
//!
//! The profile starts a bridge after another, each once the one before has
//! ended, and records their lives among its rows (#239): `bridge N started`
//! before the `N`th bridge's process exists, and `bridge N ended` once that
//! process has been reaped, numbered from 1.

mod json;

use std::cell::OnceCell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use bridge::access::{DEFAULT_ORIGINS, Policy};
use bridge::catalog::id_of;
use bridge::engine::{Engine, EngineConfig};
use bridge::explorer::fragment::Filter;
use bridge::explorer::runs::{PassTime, Timings};
use bridge::explorer::{self, masks};
use bridge::search::heads;
use bridge::server;
use bridge::sources::Sources;
use bridge::{folders, start};
use cbformat::game::{Head, RecordKind};
use cbformat::pgnfile::lex::Lexer;
use cbformat::pgnfile::line::main_line;
use cbformat::view::Base;
use chesscore::Board;
use json::Value;

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
/// How long the flows wait for the index built unasked, with `--background`.
const BACKGROUND_WAIT: Duration = Duration::from_secs(600);
/// How long a bridge is given to finish writing its files before it ends.
const WRITES_WAIT: Duration = Duration::from_secs(60);
/// The searches by a position fragment and by material (#272), by name and
/// parameters: a Carlsbad pawn skeleton, a knight on d5 against the pawn on
/// d6, rook endgames, and a bishop on h7 with its mirrors.
const FRAGMENTS: [(&str, &str); 4] = [
    ("Carlsbad skeleton", "look=Pd4,Pe3,pc6,pd5&nowhite=c2,c4&noblack=e6,e5"),
    ("Nd5 against d6", "look=Nd5,pd6"),
    ("rook endgames", "material=Q0,q0,B0,b0,N0,n0,R1..2,r1..2"),
    ("Bh7, mirrored", "look=Bh7&mirror=both"),
];
/// The rounds of the comparison of the searches with masks and without: in
/// each, a new bridge of each kind, in turns, so that every search is cold.
const FRAGMENT_ROUNDS: usize = 3;

struct Options {
    db: PathBuf,
    index: PathBuf,
    engine: Option<PathBuf>,
    /// The first bridge builds the position index unasked (#149).
    background: bool,
}

fn options(args: &[String]) -> AnyResult<Options> {
    let mut db = None;
    let (mut index, mut engine, mut background) = (None, None, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--index" => index = it.next().map(PathBuf::from),
            "--engine" => engine = it.next().map(PathBuf::from),
            "--background" => background = true,
            _ if db.is_none() => db = Some(PathBuf::from(a)),
            _ => return Err("unexpected argument".into()),
        }
    }
    let db = db.ok_or("no database given")?;
    let index = index.ok_or("--index <dir> is required: the indexes are built there")?;
    Ok(Options { db, index, engine, background })
}

/// `cbtool profile-serve <db> <index> [--background] [--no-masks] [<engine>]`:
/// the bridge `profile` asks, in a process of its own. It prints `port <n>`
/// and serves until killed, or until its input ends: when the `profile` that
/// started it ends, however it ends. Once it has built the database's position
/// index, it prints `built`, then where the build's time went
/// ([`Timings::line`]). With `--background`, it keeps its indexes as a bridge
/// that serves does (#149). With `--no-masks`, a search by a fragment or by
/// material replays every game, without the masks that rule games out first
/// (#272), so that the two can be compared.
pub(crate) fn serve(args: &[String]) -> AnyResult<bool> {
    let [db, index, rest @ ..] = args else {
        return Err("profile-serve <db> <index> [--background] [--no-masks] [<engine>]".into());
    };
    let (mut background, mut no_masks, mut rest) = (false, false, rest);
    while let [flag, after @ ..] = rest {
        match flag.as_str() {
            "--background" => background = true,
            "--no-masks" => no_masks = true,
            _ => break,
        }
        rest = after;
    }
    let listeners = server::bind(0)?;
    let port = listeners[0].local_addr()?.port();
    let engine = match rest.first() {
        Some(exe) => Engine::new(EngineConfig::new(PathBuf::from(exe), None, None)),
        None => Engine::none(),
    };
    let policy = Policy { port, origins: DEFAULT_ORIGINS.iter().map(|o| o.to_string()).collect(), token: TOKEN.into() };
    // Set up as a start sets a bridge up, serving the one database. The
    // `--index` folder stands for the data folder: every index the bridge
    // builds goes there, never into the real data folder, whose indexes would
    // make the first answers warm.
    let sources = Sources { fixed: vec![PathBuf::from(db)], ..Sources::default() };
    let app = start::setup(Path::new(index), "profile", policy, sources, engine);
    app.catalog.explorer.set_masks_off(no_masks);
    let mut out = std::io::stdout();
    writeln!(out, "port {port}")?;
    out.flush()?;
    let app = Arc::new(app);
    std::thread::spawn(|| {
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        std::process::exit(0);
    });
    if background {
        bridge::explorer::keeper::start(&app);
    }
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

/// A bridge serving one database in a child process, killed when dropped, the
/// connection the flows ask it on, and the lines it prints after its port,
/// each with when it came.
struct Served {
    /// Which bridge of the profile it is, from 1, for the record.
    n: usize,
    child: Child,
    /// Held open for the bridge's life: the bridge ends when it closes.
    _input: ChildStdin,
    c: Client,
    /// It keeps its indexes as a bridge that serves does (#149).
    background: bool,
    lines: mpsc::Receiver<(Instant, String)>,
    built: OnceCell<(Instant, Timings)>,
}

impl Served {
    /// When the bridge told it had built the position index, and where the
    /// build's time went, once it tells, waiting `wait` at most.
    fn built(&self, wait: Duration) -> Option<&(Instant, Timings)> {
        let until = Instant::now() + wait;
        while self.built.get().is_none() {
            let Ok((at, line)) = self.lines.recv_timeout(until.saturating_duration_since(Instant::now())) else {
                break;
            };
            if let Some(t) = line.strip_prefix("built ").and_then(Timings::parse) {
                let _ = self.built.set((at, t));
            }
        }
        self.built.get()
    }

    /// With `--background`, the state of the position index's build now, by
    /// the bridge's own word: built once it has said so, else running while
    /// `/v1/status` lists the build in a phase other than `waiting`. A build
    /// not started yet, as while the keeper waits for the database to be
    /// quiet, or waiting for its turn, reads nothing.
    fn build(&mut self) -> Option<Build> {
        if !self.background {
            return None;
        }
        if self.built(Duration::ZERO).is_some() {
            return Some(Build::Built);
        }
        Some(match self.c.get("/v1/status", true) {
            Ok((200, body)) if phases(&Value::of(&body)).iter().any(|p| *p != "waiting") => Build::Running,
            _ => Build::Idle,
        })
    }
}

/// The position index's build as a row sees it (#149).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Build {
    Idle,
    Running,
    Built,
}

/// What a row's counts say of the build, from its state before the row's
/// first request and after its last: only a row the build ran through from
/// its start to its end was taken during the build. Nothing without
/// `--background`.
fn during(before: Option<Build>, after: Option<Build>) -> &'static str {
    match (before, after) {
        (None, _) | (_, None) => "",
        (Some(Build::Running), Some(Build::Running)) => ", during the build",
        (Some(Build::Built), _) => ", after the build",
        (Some(Build::Idle), Some(Build::Idle)) => ", before the build",
        _ => ", partly during the build",
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        record(self.n, "ended");
    }
}

/// A line of the record of the profile's bridges, `bridge <n> <event>`, on
/// standard output with the rows. A line that cannot be written is left out,
/// as a drop must not panic.
fn record(n: usize, event: &str) {
    let _ = writeln!(std::io::stdout(), "bridge {n} {event}");
}

/// The `n`th bridge of the profile, serving `o`'s database; one that keeps
/// its indexes when `background`. Its start is recorded before its process
/// exists, and its end once the process has been reaped, here when it does
/// not start and else when it is dropped.
fn spawn(o: &Options, n: usize, background: bool) -> AnyResult<Served> {
    spawn_with(o, n, background, false)
}

/// [`spawn`], the bridge searching by fragments without masks when
/// `no_masks` (#272).
fn spawn_with(o: &Options, n: usize, background: bool, no_masks: bool) -> AnyResult<Served> {
    let mut command = Command::new(std::env::current_exe()?);
    command.arg("profile-serve").arg(&o.db).arg(&o.index);
    if background {
        command.arg("--background");
    }
    if no_masks {
        command.arg("--no-masks");
    }
    if let Some(exe) = &o.engine {
        command.arg(exe);
    }
    record(n, "started");
    let child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
    let mut child = child.inspect_err(|_| record(n, "ended"))?;
    let input = child.stdin.take();
    let mut line = String::new();
    let mut out = child.stdout.take().map(BufReader::new);
    let read = out.as_mut().is_some_and(|out| out.read_line(&mut line).is_ok());
    let port = line.trim().strip_prefix("port ").and_then(|p| p.parse().ok()).filter(|_| read);
    let (Some(port), Some(out), Some(input)) = (port, out, input) else {
        let _ = child.kill();
        let _ = child.wait();
        record(n, "ended");
        return Err("the bridge did not start".into());
    };
    // Its later lines, until it ends.
    let (send, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in out.lines().map_while(Result::ok) {
            if send.send((Instant::now(), line)).is_err() {
                return;
            }
        }
    });
    Ok(Served { n, child, _input: input, c: Client::new(port), background, lines, built: OnceCell::new() })
}

/// A failed answer, by its status and the bridge's error code only: a code is
/// a fixed identifier, and the rest of the answer may hold a name or a path.
fn failure(status: u16, body: &[u8]) -> String {
    let answer = Value::of(body);
    let code = answer.get("error").get("code").str().unwrap_or_default();
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

/// The whole number member `key` of an answer, such as a list's `total` or a
/// position's `games`.
fn count(body: &[u8], key: &str) -> Option<u64> {
    Value::of(body).get(key).u64()
}

/// The records of database `id` in the database list.
fn records_of(list: &[u8], id: &str) -> Option<u64> {
    let list = Value::of(list);
    let entry = list.get("databases").items().iter().find(|d| d.get("id").str() == Some(id))?;
    entry.get("records").u64()
}

/// The names a suggestion answer suggests.
fn values(body: &[u8]) -> Vec<String> {
    let answer = Value::of(body);
    answer.get("suggestions").items().iter().filter_map(|s| s.get("value").str().map(str::to_string)).collect()
}

/// The phases of the index builds `/v1/status` lists.
fn phases(status: &Value) -> Vec<&str> {
    status.get("indexing").items().iter().filter_map(|b| b.get("phase").str()).collect()
}

/// Whether a `topGames` entry is `row`, member for member, and its `year`.
fn row_and_year(entry: &Value, row: &Value) -> bool {
    let mut rest = entry.members().cloned().unwrap_or_default();
    rest.remove("year").is_some() && row.members() == Some(&rest)
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
            Ok((409, body)) if Value::of(&body).get("error").get("state").str() == Some("indexing") => {
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

/// Whether a file is being written in the index folder `dir`: every index,
/// heads and names file is written as a partial file, renamed once whole.
fn writing(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    entries.flatten().any(|e| e.file_name().to_str().is_some_and(|n| n.ends_with(".partial")))
}

/// Whether names files may still come beside the heads file `heads` in the
/// index folder `dir`. A bridge writes a name table's file only when it read
/// the table beside a heads file it had (#108), and it builds a heads file
/// only for a database of [`heads::MIN_RECORDS`] records or more: with no
/// heads file, and none being written, none will come.
fn names_may_come(dir: &Path, heads: &Path) -> bool {
    heads.exists() || writing(dir)
}

pub(crate) fn run(args: &[String]) -> AnyResult<bool> {
    let o = options(args)?;
    if !fresh(&o.index) {
        return Err("--index must name a new or empty folder, so that the index build is measured".into());
    }
    std::fs::create_dir_all(&o.index)?;
    println!("{:<12} {:<30} {:>3} {:>10} {:>10} {:>10}  counts", "flow", "case", "n", "median ms", "min ms", "max ms");
    let id = id_of(&o.db);
    let mut p = Profile { base: format!("/v1/databases/{id}"), id, o, table: Table::default() };

    // The first bridge: the flows that open the database and fill its caches,
    // and the position index's build.
    let launched = Instant::now();
    let mut served = spawn(&p.o, 1, p.o.background)?;
    let Some(records) = p.opening(&mut served, ms(launched.elapsed())) else { return Ok(false) };
    p.sorts(&mut served);
    p.windows(&mut served, records);
    let player = p.suggestions(&mut served);
    let searches = p.searches(&mut served, player);
    p.pgn(&mut served)?;
    p.index(&mut served, launched, records);
    let served = p.fragments(served)?;

    // New bridges, their caches empty, on the files the ones before wrote:
    // each replaces the one before (#191).
    let mut again = p.new_bridge(served)?;
    p.with_heads(&mut again, &searches);
    let mut last = p.with_names(again, &searches)?;
    if p.o.engine.is_some() {
        last = p.engines(last)?;
    }
    p.http(&mut last);
    Ok(!p.table.failed)
}

/// What the flows share: the options, the table printed, and the database,
/// which has the same id in every bridge.
struct Profile {
    o: Options,
    table: Table,
    id: String,
    /// `/v1/databases/{id}`.
    base: String,
}

/// The searches the flows time, by name and query.
type Searches = Vec<(&'static str, String)>;

/// The positions whose games are listed, by case: its name, what its first
/// window takes at most, and its positions in FEN.
type Positions = Vec<(String, u32, Vec<String>)>;

/// `path` asked once, while the bridge's caches are cold for it, then
/// [`RUNS`] times cached: the samples of each, and the cold answer when it
/// succeeded.
fn cold_then_cached(c: &mut Client, path: &str) -> (Samples, Option<Vec<u8>>, Samples) {
    let mut cold = Samples::default();
    let body = cold.get(c, path, true);
    let mut cached = Samples::default();
    for _ in 0..RUNS {
        cached.get(c, path, true);
    }
    (cold, body, cached)
}

impl Profile {
    /// A new bridge in place of `before`, which ends first: one that builds
    /// no index unasked, its caches empty, and the index folder holding what
    /// the bridges before it wrote.
    fn respawn(&self, before: Served) -> AnyResult<Served> {
        let n = before.n + 1;
        self.end(before);
        spawn(&self.o, n, false)
    }

    /// Ends `bridge` once it has written its files, as its index builds and
    /// the name tables it read write them, waiting [`WRITES_WAIT`] at most
    /// (#191). Every bridge keeps its indexes in the `--index` folder and
    /// sweeps it as it starts, of the partial files that only the process
    /// writing them can tell from abandoned ones: one bridge runs at a time,
    /// and the next one finds the files whole.
    fn end(&self, bridge: Served) {
        let dirs = [self.index_folder(), folders::pgn_dir(&self.o.index)];
        let waited = Instant::now();
        while dirs.iter().any(|d| writing(d)) && waited.elapsed() < WRITES_WAIT {
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(bridge);
    }

    /// The position `fen` in the explorer.
    fn explorer(&self, fen: &str) -> String {
        format!("{}/explorer?fen={}", self.base, encode(fen))
    }

    /// Where the bridges keep the position index and the heads and names
    /// files: the index folder of their data folder, `--index`.
    fn index_folder(&self) -> PathBuf {
        folders::index_dir(&self.o.index)
    }

    /// A row of `path` asked once, cold.
    fn cold(&mut self, served: &mut Served, flow: &str, case: &str, path: &str, counts: &str) {
        let mut cold = Samples::default();
        cold.get(&mut served.c, path, true);
        self.table.row(flow, case, &mut cold, counts);
    }

    /// The rows of a sort or a search, `path`: its first answer, cold, with
    /// the answer's `total` counted as `what`, then the cached ones, each
    /// saying what it saw of the build.
    fn sort_or_search(&mut self, served: &mut Served, flow: &str, name: &str, path: &str, what: &str) {
        let before = served.build();
        let (mut cold, body, mut cached) = cold_then_cached(&mut served.c, path);
        let during = during(before, served.build());
        let total = body.and_then(|b| count(&b, "total")).map_or(String::new(), |t| format!("{t} {what}"));
        self.table.row(flow, &format!("{name} cold"), &mut cold, &format!("{total}{during}"));
        self.table.row(flow, &format!("{name} cached"), &mut cached, during.trim_start_matches(", "));
    }

    /// Opening: the bridge started in `start` ms, then its first answers open
    /// the database. Its records; `None` when it is not ready.
    fn opening(&mut self, served: &mut Served, start: f64) -> Option<u64> {
        let mut first = Samples::default();
        let status = first.get(&mut served.c, "/v1/status", true);
        self.table.once("opening", "bridge process start", start, "");
        // With `--background`, the phases of the build already listed.
        let indexing = match status {
            Some(body) if self.o.background => format!("indexing: {}", phases(&Value::of(&body)).join(" ")),
            _ => String::new(),
        };
        self.table.row("opening", "first status", &mut first, &indexing);
        let mut first_list = Samples::default();
        let records = first_list.get(&mut served.c, "/v1/databases", true).and_then(|list| records_of(&list, &self.id));
        let Some(records) = records else {
            self.table.row("opening", "first database list", &mut first_list, "the database is not ready");
            return None;
        };
        self.table.row("opening", "first database list", &mut first_list, &format!("{records} records"));
        for (case, path) in [("status", "/v1/status"), ("database list", "/v1/databases")] {
            let mut s = Samples::default();
            for _ in 0..RUNS {
                s.get(&mut served.c, path, true);
            }
            self.table.row("opening", case, &mut s, "");
        }
        Some(records)
    }

    /// Sorts: the first order of each key over all records, then cached.
    fn sorts(&mut self, served: &mut Served) {
        for key in SORT_KEYS {
            let path = format!("{}/games?sort={key}&limit=500", self.base);
            self.sort_or_search(served, "sort", key, &path, "rows");
        }
    }

    /// Windows of 500 rows at the start, the middle and the end of the
    /// `records`, with and without the main line.
    fn windows(&mut self, served: &mut Served, records: u64) {
        let last = records.saturating_sub(500);
        for (place, offset) in [("start", 0), ("middle", records / 2), ("end", last)] {
            for (form, extra) in [("", ""), (" line=60", "&line=60")] {
                let path = format!("{}/games?offset={offset}&limit=500{extra}", self.base);
                let mut s = Samples::default();
                let mut rows = None;
                for _ in 0..RUNS {
                    if let Some(body) = s.get(&mut served.c, &path, true) {
                        rows.get_or_insert_with(|| Value::of(&body).get("rows").items().len());
                    }
                }
                let counts = rows.map_or(String::new(), |r| format!("{r} rows"));
                self.table.row("window", &format!("{place}{form}"), &mut s, &counts);
            }
        }
    }

    /// Player suggestions for prefixes of one to three letters. The first
    /// request of a field builds its name tables. The first player suggested
    /// for `m`.
    fn suggestions(&mut self, served: &mut Served) -> Option<String> {
        let mut player = None;
        for (i, prefix) in ["m", "mo", "mor"].iter().enumerate() {
            let path = format!("{}/suggest?field=player&prefix={prefix}", self.base);
            let mut first = Samples::default();
            let mut body = first.get(&mut served.c, &path, true);
            if i == 0 {
                player = body.as_deref().and_then(|b| values(b).into_iter().next());
                self.table.row("suggest", "player, first ever", &mut first, "");
            }
            // A longer prefix's first answer is not timed apart, but its failure
            // counts with the cached ones.
            let failures = if i == 0 { Vec::new() } else { first.failures };
            let mut s = Samples { times: Vec::new(), failures };
            for _ in 0..RUNS {
                let answer = s.get(&mut served.c, &path, true);
                body = body.or(answer);
            }
            let names = body.map_or(String::new(), |b| format!("{} names", values(&b).len()));
            self.table.row("suggest", &format!("player, {} letters", prefix.len()), &mut s, &names);
        }
        player
    }

    /// Searches by qualifier, cold and then cached. The names come from the
    /// suggestions, `player`'s above and an event's asked for here, and are
    /// not printed.
    fn searches(&mut self, served: &mut Served, player: Option<String>) -> Searches {
        let mut events = Samples::default();
        let event = events
            .get(&mut served.c, &format!("{}/suggest?field=event&prefix=o", self.base), true)
            .and_then(|b| values(&b).into_iter().next());
        let mut searches = vec![("date", "date:2020".to_string())];
        match &player {
            Some(p) => searches.extend(["player", "white", "black"].map(|q| (q, format!("{q}:\"{p}\"")))),
            None => self.table.row("search", "player", &mut Samples::default(), "no player suggested for m"),
        }
        match &event {
            Some(e) => searches.push(("event", format!("event:\"{e}\""))),
            None => self.table.row("search", "event", &mut events, "no event suggested for o"),
        }
        for (name, q) in &searches {
            let path = format!("{}/games?limit=500&q={}", self.base, encode(q));
            self.sort_or_search(served, "search", name, &path, "matches");
        }
        searches
    }

    /// One game as PGN: the first game, and the most annotated one of the
    /// first records, in the reading and the full form.
    fn pgn(&mut self, served: &mut Served) -> AnyResult<()> {
        let (first_game, annotated, notes) = scan_games(&self.o)?;
        for (which, number) in [("first game", first_game), ("most annotated", annotated)] {
            for (form, extra) in [("reading", ""), ("full", "?annotations=full")] {
                let path = format!("{}/games/{number}{extra}", self.base);
                let (mut cold, body, mut warm) = cold_then_cached(&mut served.c, &path);
                let bytes = body.map(|b| b.len());
                let mut counts = bytes.map_or(String::new(), |b| format!("{b} bytes"));
                if number == annotated && bytes.is_some() {
                    counts = format!("{counts}, {notes} annotations");
                }
                self.table.row("pgn", &format!("{which}, {form} cold"), &mut cold, &counts);
                self.table.row("pgn", &format!("{which}, {form}"), &mut warm, "");
            }
        }
        Ok(())
    }

    /// The position index: its build in the new folder, a lookup per move
    /// along the most played line, deep and crowded positions, the games of
    /// positions, and the notable games' rows. The index folder is measured
    /// while the build writes it (#147): at its largest, and once the build
    /// is done. With `--background`, the build the bridge started unasked is
    /// waited for instead, from the bridge's start, `launched`, to the index
    /// ready (#149).
    fn index(&mut self, served: &mut Served, launched: Instant, records: u64) {
        let folder = self.index_folder();
        if self.o.background {
            background_build(&mut self.table, served, &self.id, launched, &folder);
        } else {
            let start = self.explorer(START_FEN);
            requested_build(&mut self.table, served, &self.id, &start, &folder);
        }
        let (mut listed, notable) = self.lookups(served);
        listed.extend(self.deep(served, records));
        self.crowded(served);
        list_positions(&mut self.table, &mut served.c, &self.base, &listed);
        self.notable_rows(served, &notable);
    }

    /// Searches by a position fragment and by material (#272): the build of
    /// the masks the first one starts, until `/v1/status` no longer reports
    /// it, and the file's size; how many games the masks leave each search
    /// to replay; and each search's first window with the masks and without
    /// them, on new bridges in turns ([`FRAGMENT_ROUNDS`]). Both must find
    /// the same games. The last bridge is handed back.
    fn fragments(&mut self, mut served: Served) -> AnyResult<Served> {
        let base = self.base.clone();
        let list = |params: &str| format!("{base}/games?limit=100&{params}");
        let t = Instant::now();
        let started = match served.c.get(&list(FRAGMENTS[0].1), true) {
            Ok((409, body)) if Value::of(&body).get("error").get("state").str() == Some("indexing") => Ok(()),
            Ok((status, body)) => Err(failure(status, &body)),
            Err(_) => Err("no answer".to_string()),
        };
        let built = started.and_then(|()| {
            loop {
                match served.c.get("/v1/status", true) {
                    Ok((200, body)) if phases(&Value::of(&body)).contains(&"masks") => {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Ok((200, _)) => break Ok(()),
                    Ok((status, body)) => break Err(failure(status, &body)),
                    Err(_) => break Err("no answer".to_string()),
                }
            }
        });
        if let Err(why) = built {
            self.table.failure("fragment", "masks build", &why);
            return Ok(served);
        }
        let index = explorer::paths(&self.index_folder(), &self.id).0;
        let path = masks::path_of(&index);
        let size = std::fs::metadata(&path).map_or(0, |m| m.len());
        self.table.once("fragment", "masks build", ms(t.elapsed()), &format!("{:.1} MB", size as f64 / 1e6));
        self.survivors(&index);
        let mut with: Vec<Samples> = FRAGMENTS.iter().map(|_| Samples::default()).collect();
        let mut without: Vec<Samples> = FRAGMENTS.iter().map(|_| Samples::default()).collect();
        let mut totals: Vec<Option<u64>> = vec![None; FRAGMENTS.len()];
        let mut differ = Vec::new();
        for round in 0..FRAGMENT_ROUNDS * 2 {
            let no_masks = round % 2 == 1;
            let n = served.n + 1;
            self.end(served);
            served = spawn_with(&self.o, n, false, no_masks)?;
            // Its index, kept on disk, answers at once.
            let mut open = Samples::default();
            open.get(&mut served.c, &self.explorer(START_FEN), true);
            for (i, (name, params)) in FRAGMENTS.iter().enumerate() {
                let samples = if no_masks { &mut without[i] } else { &mut with[i] };
                let total = samples.get(&mut served.c, &list(params), true).and_then(|body| count(&body, "total"));
                if let Some(total) = total
                    && *totals[i].get_or_insert(total) != total
                {
                    differ.push(*name);
                }
            }
        }
        for (i, (name, _)) in FRAGMENTS.iter().enumerate() {
            let games = totals[i].map_or(String::new(), |n| format!("{n} games"));
            self.table.row("fragment", &format!("{name}, masks"), &mut with[i], &games);
            self.table.row("fragment", &format!("{name}, no masks"), &mut without[i], &games);
        }
        if !differ.is_empty() {
            self.table.failure("fragment", "with masks and without", &format!("other games: {}", differ.join(", ")));
        }
        Ok(served)
    }

    /// For each search by a fragment, the games whose masks it may match,
    /// which it replays, read from the files of the index at `index` in this
    /// process.
    fn survivors(&mut self, index: &Path) {
        let stream = explorer::stream::Stream::open(&explorer::stream::path_of(index));
        let opened = stream.ok().and_then(|s| masks::Masks::open(&masks::path_of(index), &s).map(|m| (s, m)));
        let Some((stream, kept)) = opened else {
            self.table.failure("fragment", "survivors", "the masks do not open");
            return;
        };
        let header = stream.header;
        let records = (header.last_record + 1).saturating_sub(header.first_record) as usize;
        for (name, params) in FRAGMENTS {
            let named: HashMap<&str, &str> = params.split('&').filter_map(|p| p.split_once('=')).collect();
            let Ok(Some(filter)) = Filter::parse(|n| named.get(n).copied()) else {
                self.table.failure("fragment", &format!("{name} survivors"), "refused");
                continue;
            };
            let t = Instant::now();
            let mut survivors = 0usize;
            for block in 0..records.div_ceil(masks::BLOCK) {
                let Ok(rows) = kept.block(block) else {
                    self.table.failure("fragment", &format!("{name} survivors"), "a damaged block");
                    return;
                };
                survivors += rows.chunks(masks::ROW).filter(|row| filter.may_match(&masks::Row::decode(row))).count();
            }
            self.table.once(
                "fragment",
                &format!("{name} survivors"),
                ms(t.elapsed()),
                &format!(
                    "{survivors} of {records} records ({:.1}%) left to replay",
                    survivors as f64 * 100.0 / records.max(1) as f64
                ),
            );
        }
    }

    /// A lookup per move along the most played line: the positions of the
    /// line whose games are listed, and the notable games the lookups named.
    fn lookups(&mut self, served: &mut Served) -> (Positions, Vec<Value>) {
        let mut board = Board::startpos();
        let mut lookups = Samples::default();
        let mut played = 0;
        let mut stop = "";
        let mut notable = Vec::new();
        // The positions of the line whose games are listed, by ply.
        let mut listed: Positions =
            LIST_LINE_PLIES.iter().map(|p| (format!("line ply {p}"), LIST_LINE_TARGET_MS, Vec::new())).collect();
        for _ in 0..LOOKUPS {
            if let Some(at) = LIST_LINE_PLIES.iter().position(|&p| p == played) {
                listed[at].2.push(board.fen());
            }
            let Some(body) = lookups.get(&mut served.c, &self.explorer(&board.fen()), true) else { break };
            let answer = Value::of(&body);
            notable.extend_from_slice(answer.get("topGames").items());
            let Some(uci) = answer.get("moves").items().first().and_then(|m| m.get("uci").str()) else {
                stop = ", no move from the last position";
                break;
            };
            let Some(mv) = find_move(&board, uci) else {
                stop = ", a named move is not legal here";
                self.table.failed = true;
                break;
            };
            board.play_unchecked(mv);
            played += 1;
        }
        let counts = format!("{} lookups, {played} plies played{stop}", lookups.times.len());
        self.table.row("index", "lookup per move", &mut lookups, &counts);
        (listed, notable)
    }

    /// Deep positions (#133): positions of games from across the `records`
    /// at plies 30, 60 and 90, past the tree's pruning ply and past its
    /// depth; each must find at least its own game. Their positions at plies
    /// 40 and 80, whose games are listed.
    fn deep(&mut self, served: &mut Served, records: u64) -> Positions {
        let mut deep = Samples::default();
        let (mut asked, mut found) = (0, 0);
        let mut lexer = Lexer::new();
        let mut sampled: Positions = LIST_SAMPLED_PLIES
            .iter()
            .map(|p| (format!("sampled ply {p}"), LIST_SAMPLED_TARGET_MS, Vec::new()))
            .collect();
        for k in 1..=8u64 {
            let n = (records / 9 * k).max(1);
            let Some((200, body)) = served.c.get(&format!("{}/games/{n}", self.base), true).ok() else { continue };
            let game = Value::of(&body);
            let Some(pgn) = game.get("pgn").str() else { continue };
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
                let answer = deep.get(&mut served.c, &self.explorer(&fen), true);
                if answer.is_some_and(|a| count(&a, "games").is_some_and(|g| g > 0)) {
                    found += 1;
                }
            }
        }
        if found < asked {
            self.table.failed = true;
        }
        self.table.row("index", "deep lookup, plies 30/60/90", &mut deep, &format!("{found} of {asked} found"));
        sampled
    }

    /// The most crowded structure (#145): bare kings, which every game that
    /// ends in them holds, all home pawns gone, so their bucket is replayed
    /// whole.
    fn crowded(&mut self, served: &mut Served) {
        let (mut first, body, mut crowded) = cold_then_cached(&mut served.c, &self.explorer(BARE_KINGS));
        let games = body.and_then(|b| count(&b, "games")).map_or(String::new(), |g| format!("{g} games reach it"));
        self.table.row("index", "crowded structure, first", &mut first, &games);
        self.table.row("index", "crowded structure", &mut crowded, "");
    }

    /// Each notable game the lookups named is its `/games` row whole, then its
    /// year (#144): compared with its number's row, asked for once a number
    /// after the lookups, so that their times stay as they were.
    fn notable_rows(&mut self, served: &mut Served, notable: &[Value]) {
        let mut rows = Samples::default();
        let mut fetched: HashMap<u64, Option<Value>> = HashMap::new();
        let mut whole = 0;
        for entry in notable {
            let Some(n) = entry.get("number").u64() else { continue };
            let row = fetched.entry(n).or_insert_with(|| {
                let path = format!("{}/games?offset={}&limit=1", self.base, n.saturating_sub(1));
                let body = rows.get(&mut served.c, &path, true)?;
                Value::of(&body).get("rows").items().first().cloned()
            });
            if row.as_ref().is_some_and(|row| row_and_year(entry, row)) {
                whole += 1;
            }
        }
        if whole < notable.len() {
            self.table.failed = true;
        }
        let counts = format!("topGames entries with every row field: {whole} of {}", notable.len());
        self.table.row("index", "notable games' rows", &mut rows, &counts);
    }

    /// A new bridge in place of the `first`, timed from once the first has
    /// ended to its first answer from the position index the first one built.
    fn new_bridge(&mut self, first: Served) -> AnyResult<Served> {
        let n = first.n + 1;
        self.end(first);
        let t = Instant::now();
        let mut again = spawn(&self.o, n, false)?;
        let mut open = Samples::default();
        open.get(&mut again.c, &self.explorer(START_FEN), true);
        let took = ms(t.elapsed());
        if open.failures.is_empty() {
            self.table.once("index", "new bridge, first answer", took, "");
        } else {
            self.table.row("index", "new bridge, first answer", &mut open, "");
        }
        Ok(again)
    }

    /// The heads file (#106): the first bridge built it after its first sort.
    /// A new one, `again`, its caches empty, answers its first sorts,
    /// suggestion and searches from it.
    fn with_heads(&mut self, again: &mut Served, searches: &[(&str, String)]) {
        let heads = heads::path(&self.index_folder(), &self.id);
        let present = if heads.exists() { "from the heads file" } else { "no heads file" };
        for key in SORT_KEYS {
            let path = format!("{}/games?sort={key}&limit=500", self.base);
            self.cold(again, "sort+heads", &format!("{key} cold"), &path, present);
        }
        let path = format!("{}/suggest?field=player&prefix=m", self.base);
        self.cold(again, "suggest+heads", "player, first ever", &path, present);
        for (name, q) in searches {
            let path = format!("{}/games?limit=500&q={}", self.base, encode(q));
            self.cold(again, "search+heads", &format!("{name} cold"), &path, present);
        }
    }

    /// The names files (#108): the second bridge, `again`, wrote its name
    /// tables beside the heads file. The bridges that replace it read them
    /// from there for their first sort by white, suggestion and player
    /// search. The last of them. The names files are waited for a minute at
    /// most, and only while they may still come ([`names_may_come`]).
    fn with_names(&mut self, again: Served, searches: &[(&str, String)]) -> AnyResult<Served> {
        let folder = self.index_folder();
        let heads = heads::path(&folder, &self.id);
        let names: Vec<PathBuf> = ["players", "tournaments"].iter().map(|k| heads.with_extension(k)).collect();
        let waited = Instant::now();
        while !names.iter().all(|p| p.exists())
            && names_may_come(&folder, &heads)
            && waited.elapsed() < Duration::from_secs(60)
        {
            std::thread::sleep(Duration::from_millis(100));
        }
        let have = if names.iter().all(|p| p.exists()) { "from the names files" } else { "no names files" };
        let mut third = self.respawn(again)?;
        for key in ["white", "tournament"] {
            let path = format!("{}/games?sort={key}&limit=500", self.base);
            self.cold(&mut third, "sort+names", &format!("{key} cold"), &path, have);
        }
        let mut fourth = self.respawn(third)?;
        let path = format!("{}/suggest?field=player&prefix=m", self.base);
        self.cold(&mut fourth, "suggest+names", "player, first after start", &path, have);
        let Some((_, q)) = searches.iter().find(|(name, _)| *name == "player") else { return Ok(fourth) };
        let mut fifth = self.respawn(fourth)?;
        let path = format!("{}/games?limit=500&q={}", self.base, encode(q));
        self.cold(&mut fifth, "search+names", "player, first after start", &path, have);
        Ok(fifth)
    }

    /// The engine, on new bridges in place of `before`, whose engine has not
    /// run: on the first, the first line of a 5-second search and the lines
    /// a second after it; on the second, whose engine is warmed up first
    /// (#110) as the web app does when an analysis page opens, the warm-up,
    /// then the first line. The second.
    fn engines(&mut self, before: Served) -> AnyResult<Served> {
        let mut cold = self.respawn(before)?;
        engine(&mut self.table, &mut cold.c, "first line");
        let mut warmed = self.respawn(cold)?;
        let mut warm = Samples::default();
        warm.get(&mut warmed.c, "/v1/engine/warm", true);
        self.table.row("engine", "warm-up to ready", &mut warm, "");
        engine(&mut self.table, &mut warmed.c, "first line after a warm-up");
        Ok(warmed)
    }

    /// HTTP: a small answer over one kept connection, and over a new one each.
    fn http(&mut self, served: &mut Served) {
        for (case, keep) in [("keep-alive", true), ("new connection", false)] {
            let mut s = Samples::default();
            for _ in 0..200 {
                s.get(&mut served.c, "/v1/status", keep);
            }
            self.table.row("http", &format!("status, {case}"), &mut s, "");
        }
    }
}

/// The build the first explorer request starts, `path`, timed to its first
/// answer, with the index folder sampled while the build writes it, and the
/// build's phases.
fn requested_build(table: &mut Table, served: &mut Served, id: &str, path: &str, folder: &Path) {
    let building = std::sync::atomic::AtomicBool::new(true);
    let t = Instant::now();
    let mut polls = 0u64;
    let (built, peak) = std::thread::scope(|s| {
        let sampler = s.spawn(|| {
            let mut peak = 0;
            while building.load(std::sync::atomic::Ordering::Relaxed) {
                peak = peak.max(folder_bytes(folder));
                std::thread::sleep(Duration::from_millis(20));
            }
            peak.max(folder_bytes(folder))
        });
        let built = build_index(&mut served.c, path, &mut polls);
        building.store(false, std::sync::atomic::Ordering::Relaxed);
        (built, sampler.join().unwrap_or(0))
    });
    match built {
        Ok(()) => {
            let took = ms(t.elapsed());
            let stream = stream_counts(&folder.join(format!("{id}.moves")));
            let index = std::fs::metadata(folder.join(format!("{id}.idx"))).map_or(0, |m| m.len());
            let sizes =
                format!("index {index} bytes, folder at most {peak} bytes, {} bytes after", folder_bytes(folder));
            table.once("index", "build to first answer", took, &format!("{polls} polls, {stream}, {sizes}"));
            match served.built(Duration::from_secs(10)) {
                Some((_, t)) => build_phases(table, t),
                None => table.failure("index", "build phases", "not told"),
            }
        }
        Err(why) => table.failure("index", "build to first answer", &why),
    }
}

/// The build the bridge started unasked, with `--background` (#149): the time
/// from the bridge's start, `launched`, to the index ready, the mode it ran
/// in and its patience, and the build's phases.
fn background_build(table: &mut Table, served: &Served, id: &str, launched: Instant, folder: &Path) {
    match served.built(BACKGROUND_WAIT) {
        Some((at, t)) => {
            let stream = stream_counts(&folder.join(format!("{id}.moves")));
            let index = std::fs::metadata(folder.join(format!("{id}.idx"))).map_or(0, |m| m.len());
            // The bridge has this process's environment, and so its mode.
            let mode = bridge::machine::background_mode().0;
            let patience = bridge::explorer::schedule::PATIENCE.as_millis();
            let counts = format!(
                "from the bridge's start, no position asked, mode {mode}, giving way {patience} ms at most; \
                 {stream}, index {index} bytes"
            );
            table.once("index", "background build to ready", ms(at.duration_since(launched)), &counts);
            build_phases(table, t);
        }
        None => table.failure("index", "background build to ready", "not built"),
    }
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
            let total = first.get(c, &path, true).and_then(|body| count(&body, "total"));
            for _ in 0..RUNS {
                next.get(c, &format!("{path}&offset=200"), true);
            }
            let games = match c.get(&format!("{base}/explorer?fen={}", encode(fen)), true) {
                Ok((200, body)) => count(&body, "games"),
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
        let answer = Value::of(line);
        let has = |key| answer.members().is_some_and(|m| m.contains_key(key));
        if has("info") {
            first.get_or_insert(at);
            last = Some(at);
            lines += 1;
            true
        } else if has("bestmove") {
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

    /// The answers are read by member name (#191): a member written first
    /// reads as one written last, and a count is never taken from a nested
    /// member of the same name.
    #[test]
    fn answers_are_read_by_member_name() {
        let window = br#"{"rows":[{"white":"A","number":1,"total":3},{"number":7}],"total":9,"offset":0}"#;
        assert_eq!(count(window, "total"), Some(9));
        assert_eq!(count(window, "none"), None);
        assert_eq!(Value::of(window).get("rows").items().len(), 2);
        let list = br#"{"databases":[{"records":5,"id":"other"},{"state":"ready","records":12,"id":"x"}]}"#;
        assert_eq!(records_of(list, "x"), Some(12));
        assert_eq!(records_of(list, "y"), None);
        let suggested =
            br#"{"suggestions":[{"label":"Tal","value":"Tal, Mikhail","games":3},{"value":"Caf\u00e9"}],"field":"player"}"#;
        assert_eq!(values(suggested), ["Tal, Mikhail", "Caf\u{e9}"]);
        let status =
            br#"{"indexing":[{"total":9,"phase":"waiting","id":"a"},{"phase":"reading","id":"b"}],"bridge":{}}"#;
        assert_eq!(phases(&Value::of(status)), ["waiting", "reading"]);
        assert!(phases(&Value::of(br#"{"bridge":{"api":1}}"#)).is_empty());
    }

    #[test]
    fn notable_games_are_rows_and_their_year() {
        let answer = Value::of(
            br#"{"topGames":[{"number":7,"white":"A \"}{[\\","flags":{"deleted":false},"year":null},{"number":9,"year":1951}],"index":{"records":9}}"#,
        );
        let top = answer.get("topGames").items();
        assert_eq!(top.len(), 2);
        let row = Value::of(br#"{"number":7,"white":"A \"}{[\\","flags":{"deleted":false}}"#);
        assert!(row_and_year(&top[0], &row));
        // The same members in another order.
        let reordered = Value::of(br#"{"year":null,"flags":{"deleted":false},"white":"A \"}{[\\","number":7}"#);
        assert!(row_and_year(&reordered, &row));
        // A member left out, changed or added, or no year.
        let other = |text: &[u8]| !row_and_year(&top[0], &Value::of(text));
        assert!(other(br#"{"number":7,"white":"A \"}{[\\"}"#));
        assert!(other(br#"{"number":7,"white":"A \"}{[\\","flags":{"deleted":true}}"#));
        assert!(other(br#"{"number":7,"white":"A \"}{[\\","flags":{"deleted":false},"site":""}"#));
        assert!(!row_and_year(&row, &row));
        assert!(!row_and_year(&top[1], &row));
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
        let reordered = br#"{"error":{"state":"indexing","message":"PRIVATE_SENTINEL","code":"index_failed"}}"#;
        assert_eq!(failure(409, reordered), "409 index_failed");
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

    /// A bridge ends once no file of its index folders is partial (#191).
    #[test]
    fn a_partial_file_is_one_being_written() {
        let dir = std::env::temp_dir().join(format!("cbtool-profile-writing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!writing(&dir));
        std::fs::create_dir_all(&dir).unwrap();
        for whole in ["0123456789abcdef.idx", "0123456789abcdef.heads", "0123456789abcdef.annotators"] {
            std::fs::write(dir.join(whole), b"x").unwrap();
        }
        assert!(!writing(&dir));
        std::fs::write(dir.join("0123456789abcdef.annotators.partial"), b"x").unwrap();
        assert!(writing(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The names files are waited for only while a heads file stands or a
    /// file is being written, which may be the heads file (#239): a database
    /// too small for one gets neither, and the wait ends at once.
    #[test]
    fn names_files_may_come_only_beside_a_heads_file() {
        let dir = std::env::temp_dir().join(format!("cbtool-profile-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let heads = heads::path(&dir, "0123456789abcdef");
        assert!(!names_may_come(&dir, &heads));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("0123456789abcdef.idx"), b"x").unwrap();
        assert!(!names_may_come(&dir, &heads));
        let partial = dir.join("0123456789abcdef.heads.partial");
        std::fs::write(&partial, b"x").unwrap();
        assert!(names_may_come(&dir, &heads));
        std::fs::remove_file(&partial).unwrap();
        std::fs::write(&heads, b"x").unwrap();
        assert!(names_may_come(&dir, &heads));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
