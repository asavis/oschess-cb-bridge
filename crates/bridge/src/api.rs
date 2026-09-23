//! The v1 endpoints of `docs/api.md`.

use cbformat::Error;
use cbformat::pgn;
use cbformat::v2::{Database, Eco, Record, RecordKind};

use crate::access::{Policy, Verdict, cors};
use crate::catalog::{Catalog, Entry, State};
use crate::http::{Request, Response};
use crate::json::{self, Obj};
use crate::reply::{bad_parameter, error, error_with, not_found, ok};

pub const API_VERSION: i64 = 1;
pub const MAX_LIMIT: u32 = 500;
const DEFAULT_LIMIT: u32 = 200;
/// Reads of one game before a database that keeps changing is reported.
const GAME_ATTEMPTS: usize = 3;

pub struct App {
    pub version: &'static str,
    pub policy: Policy,
    pub catalog: Catalog,
    /// Called between the two header reads that bracket serving a game; tests
    /// use it to change the files at exactly that moment.
    pub between_reads: Option<Box<dyn Fn() + Send + Sync>>,
}

pub fn handle(app: &App, req: &Request) -> Response {
    match app.policy.check(req) {
        Verdict::Answer(response) => response,
        Verdict::Serve { origin } => cors(route(app, req), origin.as_deref()),
    }
}

fn route(app: &App, req: &Request) -> Response {
    let Some(segments) = req.segments() else { return bad_parameter("path", "The path is not UTF-8") };
    let s: Vec<&str> = segments.iter().map(String::as_str).collect();
    match s[..] {
        ["v1", "status"] => status(app),
        ["v1", "databases"] => databases(app),
        ["v1", "databases", id, "games"] => with_entry(app, id, |e| games(e, req)),
        ["v1", "databases", id, "games", number] => with_entry(app, id, |e| game(app, e, number)),
        _ => not_found(),
    }
}

fn with_entry(app: &App, id: &str, f: impl FnOnce(&Entry) -> Response) -> Response {
    match app.catalog.get(id) {
        Some(entry) => f(entry),
        None => not_found(),
    }
}

fn status(app: &App) -> Response {
    let states: Vec<State> = app.catalog.entries().iter().map(Entry::state).collect();
    let count = |s: State| states.iter().filter(|&&x| x == s).count() as i64;
    let dbs = Obj::new()
        .num("ready", count(State::Ready))
        .num("opening", 0)
        .num("missing", count(State::Missing))
        .num("cloudOnly", 0)
        .num("unsupported", count(State::Unsupported))
        .num("unreadable", count(State::Unreadable))
        .done();
    let bridge = Obj::new().str("version", app.version).num("api", API_VERSION).done();
    ok(Obj::new().raw("bridge", &bridge).raw("databases", &dbs).done())
}

fn databases(app: &App) -> Response {
    let items = app.catalog.entries().iter().map(|e| {
        let o = Obj::new().str("id", &e.id).str("name", &e.name).str("format", e.format.name());
        match e.open() {
            Ok(open) => o
                .str("state", State::Ready.name())
                .num("records", open.db.record_count())
                .str("generation", &format!("{:016x}", open.generation))
                .done(),
            Err(state) => o.str("state", state.name()).done(),
        }
    });
    ok(Obj::new().raw("databases", &json::array(items)).done())
}

fn unavailable(state: State) -> Response {
    error_with(409, "database_unavailable", "The database is not ready", |o| o.str("state", state.name()))
}

/// Whether a failed read is explained by the database changing under it.
fn changing(entry: &Entry, generation: u64, e: &Error) -> bool {
    matches!(e, Error::Io(..)) || entry.generation() != Some(generation)
}

fn database_changing() -> Response {
    error(503, "database_changing", "The database changed while it was read; retry")
}

fn games(entry: &Entry, req: &Request) -> Response {
    let offset = match req.param("offset").map(str::parse::<u64>) {
        None => 0,
        Some(Ok(n)) => n,
        Some(Err(_)) => return bad_parameter("offset", "offset must be a whole number"),
    };
    let limit = match req.param("limit").map(str::parse::<u32>) {
        None => DEFAULT_LIMIT,
        Some(Ok(n)) if (1..=MAX_LIMIT).contains(&n) => n,
        Some(_) => return bad_parameter("limit", "limit must be between 1 and 500"),
    };
    let descending = match req.param("sort").unwrap_or("number") {
        "number" | "number-asc" => false,
        "number-desc" => true,
        _ => return bad_parameter("sort", "only sort=number is served yet; the other keys come with search"),
    };
    if req.param("q").is_some_and(|q| !q.trim().is_empty()) {
        return bad_parameter("q", "search is not served yet");
    }
    let open = match entry.open() {
        Ok(open) => open,
        Err(state) => return unavailable(state),
    };
    let total = u64::from(open.db.record_count());
    let count = total.saturating_sub(offset).min(u64::from(limit)) as u32;
    let rows = if count == 0 {
        Ok(Vec::new())
    } else {
        // Numbers in the window, in ascending order; reversed for descending.
        let first = if descending { total - offset - u64::from(count) + 1 } else { offset + 1 } as u32;
        window(&open.db, first, count)
    };
    let mut rows = match rows {
        Ok(rows) => rows,
        Err(e) if changing(entry, open.generation, &e) => return database_changing(),
        Err(e) => return error(500, "internal", &e.to_string()),
    };
    if descending {
        rows.reverse();
    }
    ok(Obj::new()
        .str("generation", &format!("{:016x}", open.generation))
        .num("total", total as i64)
        .num("offset", offset as i64)
        .str("sort", if descending { "number-desc" } else { "number-asc" })
        .raw("rows", &json::array(rows))
        .done())
}

/// Rows `first..first + count` in one header read.
fn window(db: &Database, first: u32, count: u32) -> cbformat::Result<Vec<String>> {
    let records = db.records(first, first + count - 1)?;
    records.iter().map(|r| row(db, r)).collect()
}

fn row(db: &Database, r: &Record) -> cbformat::Result<String> {
    let e = db.entities();
    let player = |id: i64| -> cbformat::Result<String> { Ok(e.player(id)?.map(|p| p.pgn()).unwrap_or_default()) };
    let tournament = e.tournament(r.tournament())?;
    let kind = match r.kind() {
        RecordKind::Game => "game",
        RecordKind::Text => "text",
        RecordKind::Analysis => "analysis",
        RecordKind::Unknown(_) => "unknown",
    };
    let is_game = !matches!(r.kind(), RecordKind::Text);
    let names = |id: i64| if is_game { player(id) } else { Ok(String::new()) };
    let round = match (r.round(), r.subround()) {
        (n, _) if n <= 0 => String::new(),
        (n, s) if s <= 0 => n.to_string(),
        (n, s) => format!("{n}({s})"),
    };
    let flags = Obj::new().bool("deleted", r.is_deleted()).bool("chess960", matches!(r.eco(), Eco::Chess960(_))).done();
    Ok(Obj::new()
        .num("number", r.id())
        .str("kind", kind)
        .str("white", &names(r.white())?)
        .num("whiteElo", r.white_elo().max(0))
        .str("black", &names(r.black())?)
        .num("blackElo", r.black_elo().max(0))
        .str("result", r.result().pgn())
        .num("moves", r.move_count().max(0))
        .str("eco", &r.eco().pgn().unwrap_or_default())
        .str("event", tournament.as_ref().map_or("", |t| &t.title))
        .str("site", tournament.as_ref().map_or("", |t| &t.place))
        .str("date", &r.played_date().pgn())
        .str("round", &round)
        .str("annotator", &player(r.annotator())?)
        .raw("flags", &flags)
        .done())
}

fn game(app: &App, entry: &Entry, number: &str) -> Response {
    let Some(number) = number.parse::<u32>().ok().filter(|&n| n > 0) else { return not_found() };
    for _ in 0..GAME_ATTEMPTS {
        let open = match entry.open() {
            Ok(open) => open,
            Err(state) => return unavailable(state),
        };
        if number > open.db.record_count() {
            return not_found();
        }
        let before = match open.db.record(number) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if !matches!(before.kind(), RecordKind::Game) {
            return error(422, "not_a_game", "Guiding texts and analyses are not served as PGN");
        }
        let rendered = open.db.moves_of(&before).and_then(|data| pgn::game_from(&open.db, &before, &data.moves()?));
        if let Some(hook) = &app.between_reads {
            hook();
        }
        let same_header = open.db.record(number).is_ok_and(|after| after.bytes() == before.bytes());
        if !same_header || entry.generation() != Some(open.generation) {
            continue;
        }
        return match rendered {
            Ok(text) => ok(Obj::new()
                .str("generation", &format!("{:016x}", open.generation))
                .num("number", number)
                .str("pgn", &text)
                .done()),
            Err(Error::Io(..)) => database_changing(),
            Err(e) => error_with(422, "unreadable_game", "The game's records are damaged", |o| {
                o.str("reason", &e.to_string())
            }),
        };
    }
    database_changing()
}
