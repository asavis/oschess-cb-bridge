//! `cbtool profile`: the bridge's flows against one database, timed (#83).
//!
//! The database is served by a bridge in this process, on a free loopback
//! port, and asked over HTTP as oschess asks it, so every time includes the
//! request, the answer's JSON and the socket. Only timings and counts are
//! printed: never a name, a game, a query or a path, so the output can go into
//! an issue as it is. A name the flows need, such as a player to search for,
//! is taken from the bridge's own suggestions and never shown.
//!
//! "cold" is the first request of its kind after the bridge started, when its
//! caches are empty; the operating system may still hold the files in memory.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bridge::access::{DEFAULT_ORIGINS, Policy};
use bridge::api::App;
use bridge::catalog::{Catalog, id_of};
use bridge::engine::{Engine, EngineConfig};
use bridge::server;
use cbformat::game::{Head, RecordKind};
use cbformat::view::Base;
use chesscore::Board;

use super::AnyResult;

const TOKEN: &str = "profileprofileprofileprofileprofileprofileP";
/// Runs of each warm measurement, of which the median is shown.
const RUNS: usize = 5;
/// Records scanned for the most annotated game.
const ANNOTATED_SCAN: u32 = 50_000;
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
            other => return Err(format!("unexpected argument {other:?}").into()),
        }
    }
    let db = db.ok_or("no database given")?;
    let index = index.ok_or("--index <dir> is required: the position index is built there")?;
    Ok(Options { db, index, engine })
}

/// A bridge serving one database in this process.
struct Served {
    port: u16,
    id: String,
}

fn serve(o: &Options) -> AnyResult<Served> {
    let listeners = server::bind(0)?;
    let port = listeners[0].local_addr()?.port();
    let engine = match &o.engine {
        Some(exe) => Engine::new(EngineConfig::new(exe.clone(), None, None)),
        None => Engine::none(),
    };
    let app = App {
        version: "profile",
        policy: Policy { port, origins: DEFAULT_ORIGINS.iter().map(|o| o.to_string()).collect(), token: TOKEN.into() },
        catalog: Catalog::new([o.db.clone()]),
        between_reads: None,
        engine,
    };
    app.catalog.explorer.set_dir(o.index.clone());
    let app = Arc::new(app);
    std::thread::spawn(move || server::serve(listeners, app));
    Ok(Served { port, id: id_of(&o.db) })
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
        match self.request(path, keep) {
            Err(_) if reused => {
                self.conn = None;
                self.request(path, keep)
            }
            other => other,
        }
    }

    fn request(&mut self, path: &str, keep: bool) -> AnyResult<(u16, Vec<u8>)> {
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
            loop {
                line.clear();
                conn.read_line(&mut line)?;
                let size = usize::from_str_radix(line.trim(), 16)?;
                let mut chunk = vec![0; size + 2];
                conn.read_exact(&mut chunk)?;
                if size == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..size]);
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

    /// `get`, which must answer 200.
    fn ok(&mut self, path: &str) -> AnyResult<Vec<u8>> {
        match self.get(path, true)? {
            (200, body) => Ok(body),
            (status, _) => Err(format!("a flow was answered {status}").into()),
        }
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn timed<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t = Instant::now();
    let out = f();
    (out, ms(t.elapsed()))
}

/// Median, fastest and slowest of `times`.
fn spread(times: &mut [f64]) -> (f64, f64, f64) {
    times.sort_by(f64::total_cmp);
    (times[times.len() / 2], times[0], times[times.len() - 1])
}

/// One printed row: the flow, its case, its times in milliseconds and counts.
fn row(flow: &str, case: &str, times: &mut [f64], counts: &str) {
    if times.is_empty() {
        println!("{flow:<12} {case:<30}   0          -          -          -  {counts}");
        return;
    }
    let (median, min, max) = spread(times);
    println!("{flow:<12} {case:<30} {:>3} {median:>10.1} {min:>10.1} {max:>10.1}  {counts}", times.len());
}

/// A number member `"key":123` of a JSON text.
fn number(json: &[u8], key: &str) -> Option<u64> {
    let text = std::str::from_utf8(json).ok()?;
    let at = text.find(&format!("\"{key}\":"))? + key.len() + 3;
    let digits: String = text[at..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
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

pub(crate) fn run(args: &[String]) -> AnyResult<bool> {
    let o = options(args)?;
    std::fs::create_dir_all(&o.index)?;
    println!("{:<12} {:<30} {:>3} {:>10} {:>10} {:>10}  counts", "flow", "case", "n", "median ms", "min ms", "max ms");

    // Opening: the bridge starts, then its first answers open the database.
    let (served, start) = timed(|| serve(&o));
    let served = served?;
    let base = format!("/v1/databases/{}", served.id);
    let mut c = Client::new(served.port);
    let (status, first) = timed(|| c.ok("/v1/status"));
    status?;
    row("opening", "bridge start", &mut [start], "");
    row("opening", "first status", &mut [first], "");
    let (list, first_list) = timed(|| c.ok("/v1/databases"));
    let records = number(&list?, "records").ok_or("the database is not ready")?;
    row("opening", "first database list", &mut [first_list], &format!("{records} records"));
    for (case, path) in [("status", "/v1/status"), ("database list", "/v1/databases")] {
        let mut times: Vec<f64> = (0..RUNS).map(|_| timed(|| c.ok(path)).1).collect();
        row("opening", case, &mut times, "");
    }

    // Sorts: the first order of each key over all records, then cached.
    for key in SORT_KEYS {
        let path = format!("{base}/games?sort={key}&limit=500");
        let (body, cold) = timed(|| c.ok(&path));
        let total = number(&body?, "total").unwrap_or(0);
        let mut warm: Vec<f64> = (0..RUNS).map(|_| timed(|| c.ok(&path)).1).collect();
        row("sort", &format!("{key} cold"), &mut [cold], &format!("{total} rows"));
        row("sort", &format!("{key} cached"), &mut warm, "");
    }

    // Windows of 500 rows at the start, the middle and the end, with and
    // without the main line.
    let last = records.saturating_sub(500);
    for (place, offset) in [("start", 0), ("middle", records / 2), ("end", last)] {
        for (form, extra) in [("", ""), (" line=60", "&line=60")] {
            let path = format!("{base}/games?offset={offset}&limit=500{extra}");
            let mut times: Vec<f64> = (0..RUNS).map(|_| timed(|| c.ok(&path)).1).collect();
            row("window", &format!("{place}{form}"), &mut times, "500 rows");
        }
    }

    // Player suggestions for prefixes of one to three letters. The first
    // request of a field builds its name tables.
    let mut player = None;
    for (i, prefix) in ["m", "mo", "mor"].iter().enumerate() {
        let path = format!("{base}/suggest?field=player&prefix={prefix}");
        let (body, first) = timed(|| c.ok(&path));
        let body = body?;
        if i == 0 {
            player = strings(&body, "value").into_iter().next();
            row("suggest", "player, first ever", &mut [first], "");
        }
        let mut times: Vec<f64> = (0..RUNS).map(|_| timed(|| c.ok(&path)).1).collect();
        row(
            "suggest",
            &format!("player, {} letters", prefix.len()),
            &mut times,
            &format!("{} names", strings(&body, "value").len()),
        );
    }
    let event = strings(&c.ok(&format!("{base}/suggest?field=event&prefix=o"))?, "value").into_iter().next();

    // Searches by qualifier, cold and then cached. The names come from the
    // suggestions above and are not printed.
    let mut searches = vec![("date", "date:2020".to_string())];
    match &player {
        Some(p) => searches.extend(["player", "white", "black"].map(|q| (q, format!("{q}:\"{p}\"")))),
        None => row("search", "player", &mut [], "no player suggested for m"),
    }
    match &event {
        Some(e) => searches.push(("event", format!("event:\"{e}\""))),
        None => row("search", "event", &mut [], "no event suggested for o"),
    }
    for (name, q) in &searches {
        let path = format!("{base}/games?limit=500&q={}", encode(q));
        let (body, cold) = timed(|| c.ok(&path));
        let total = number(&body?, "total").unwrap_or(0);
        let mut warm: Vec<f64> = (0..RUNS).map(|_| timed(|| c.ok(&path)).1).collect();
        row("search", &format!("{name} cold"), &mut [cold], &format!("{total} matches"));
        row("search", &format!("{name} cached"), &mut warm, "");
    }

    // One game as PGN: the first game, and the most annotated one of the first
    // records, in the reading and the full form.
    let (annotated, notes) = most_annotated(&o)?;
    for (which, number) in [("first game", 1), ("most annotated", annotated)] {
        for (form, extra) in [("reading", ""), ("full", "?annotations=full")] {
            let path = format!("{base}/games/{number}{extra}");
            let (body, cold) = timed(|| c.ok(&path));
            let bytes = body?.len();
            let mut warm: Vec<f64> = (0..RUNS).map(|_| timed(|| c.ok(&path)).1).collect();
            let counts = if number == annotated {
                format!("{bytes} bytes, {notes} annotations")
            } else {
                format!("{bytes} bytes")
            };
            row("pgn", &format!("{which}, {form} cold"), &mut [cold], &counts);
            row("pgn", &format!("{which}, {form}"), &mut warm, "");
        }
    }

    // The position index: its build, a lookup per move along the most played
    // line, and opening it again in a new bridge.
    let explorer = |c: &mut Client, fen: &str| c.get(&format!("{base}/explorer?fen={}", encode(fen)), true);
    let t = Instant::now();
    let mut polls = 0u64;
    loop {
        let (status, body) = explorer(&mut c, START_FEN)?;
        if status == 200 {
            break;
        }
        if status != 409 {
            return Err(format!("the index answered {status}: {}", String::from_utf8_lossy(&body)).into());
        }
        polls += 1;
        std::thread::sleep(Duration::from_millis(250));
    }
    row("index", "build to first answer", &mut [ms(t.elapsed())], &format!("{polls} polls"));
    let mut board = Board::startpos();
    let mut times = Vec::new();
    for _ in 0..30 {
        let fen = board.fen();
        let t = Instant::now();
        let (status, body) = explorer(&mut c, &fen)?;
        if status != 200 {
            break;
        }
        times.push(ms(t.elapsed()));
        let Some(uci) = strings(&body, "uci").into_iter().next() else { break };
        let Some(mv) = board.legal_moves().into_iter().find(|m| m.to_string() == uci) else { break };
        board.play_unchecked(mv);
    }
    let plies = times.len();
    row("index", "lookup per move", &mut times, &format!("{plies} plies"));
    let (again, start) = timed(|| serve(&o));
    let again = again?;
    let mut fresh = Client::new(again.port);
    let (opened, first) =
        timed(|| fresh.get(&format!("/v1/databases/{}/explorer?fen={}", again.id, encode(START_FEN)), true));
    let status = opened?.0;
    row("index", "open, first answer", &mut [start + first], &format!("status {status}"));

    // The engine, when one is given: the first line, and the lines a second.
    if o.engine.is_some() {
        let t = Instant::now();
        let (status, body) = c.get("/v1/engine/analyze?movetime=5000&stream=profile", true)?;
        let total = ms(t.elapsed());
        let lines = body.split(|&b| b == b'\n').filter(|l| !l.is_empty()).count();
        row("engine", "5 s search", &mut [total], &format!("status {status}, {lines} lines"));
    }

    // HTTP: a small answer over one kept connection, and over a new one each.
    for (case, keep) in [("keep-alive", true), ("new connection", false)] {
        let mut times: Vec<f64> = (0..200).map(|_| timed(|| c.get("/v1/status", keep).map(|_| ())).1).collect();
        row("http", &format!("status, {case}"), &mut times, "");
    }
    Ok(true)
}

/// The game with the most annotations among the first records, and how many
/// it has; game 1 when none has any.
fn most_annotated(o: &Options) -> AnyResult<(u32, usize)> {
    let db = Base::open(&o.db)?;
    let last = db.record_count().min(ANNOTATED_SCAN);
    let mut best = (1, 0);
    let mut first = 1;
    while first <= last {
        let upto = last.min(first + 4095);
        for h in db.headers(first, upto)? {
            if h.kind() != RecordKind::Game || h.is_deleted() {
                continue;
            }
            if let Ok(Some(a)) = db.annotations_of(&h) {
                let n: usize = a.blocks.iter().map(|b| b.annotations.len()).sum();
                if n > best.1 {
                    best = (h.id(), n);
                }
            }
        }
        first = upto + 1;
    }
    Ok(best)
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
    }

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(encode("player:\"Tal, M\""), "player%3A%22Tal%2C%20M%22");
        assert_eq!(encode("é"), "%C3%A9");
    }
}
