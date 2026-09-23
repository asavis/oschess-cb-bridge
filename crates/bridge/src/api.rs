//! The v1 endpoints of `docs/api.md`.

use std::collections::HashMap;
use std::sync::{Condvar, Mutex};

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
/// The largest move record, content or spare area, served as PGN. The largest
/// record of any kind in a Mega Database is about 1.2 MB, a guiding text; a
/// move record near the reader's 64 MiB limit would take gigabytes to render.
pub const MAX_GAME_BYTES: usize = 2 << 20;
/// Games rendered at once; the others wait. With [`MAX_GAME_BYTES`] this keeps
/// rendering within a few hundred megabytes whatever the requests.
const MAX_RENDERS: usize = 4;

/// A counting gate: at most `MAX_RENDERS` holders at a time.
struct Gate {
    held: Mutex<usize>,
    freed: Condvar,
}

static RENDERS: Gate = Gate { held: Mutex::new(0), freed: Condvar::new() };

impl Gate {
    fn enter(&self) -> GateGuard<'_> {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        while *held >= MAX_RENDERS {
            held = self.freed.wait(held).unwrap_or_else(|e| e.into_inner());
        }
        *held += 1;
        GateGuard(self)
    }
}

struct GateGuard<'a>(&'a Gate);

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        *self.0.held.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        self.0.freed.notify_one();
    }
}

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
    // `first + count` itself may not fit when the window ends at `u32::MAX`.
    let records = db.records(first, first + (count - 1))?;
    let mut names = Names::new(db);
    records.iter().map(|r| row(&mut names, r)).collect()
}

/// The longest text, in characters, a list row carries in one field. Longer
/// names are cut and end with `…`; the game's PGN has them in full. With 500
/// rows this bounds a window to a few megabytes, whatever an entity holds.
pub const MAX_FIELD_CHARS: usize = 200;

fn clip(text: String) -> String {
    match text.char_indices().nth(MAX_FIELD_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

/// Entity names for one window, each looked up once: rows of a tournament
/// share their players and event, and one entity can be up to a megabyte.
struct Names<'a> {
    db: &'a Database,
    players: HashMap<i64, String>,
    tournaments: HashMap<i64, (String, String)>,
    titles: HashMap<i64, String>,
}

impl<'a> Names<'a> {
    fn new(db: &'a Database) -> Self {
        Names { db, players: HashMap::new(), tournaments: HashMap::new(), titles: HashMap::new() }
    }

    fn player(&mut self, id: i64) -> cbformat::Result<String> {
        if let Some(name) = self.players.get(&id) {
            return Ok(name.clone());
        }
        let name = clip(self.db.entities().player(id)?.map(|p| p.pgn()).unwrap_or_default());
        self.players.insert(id, name.clone());
        Ok(name)
    }

    /// The tournament's title and place.
    fn tournament(&mut self, id: i64) -> cbformat::Result<(String, String)> {
        if let Some(t) = self.tournaments.get(&id) {
            return Ok(t.clone());
        }
        let t = self.db.entities().tournament(id)?.map_or_else(Default::default, |t| (clip(t.title), clip(t.place)));
        self.tournaments.insert(id, t.clone());
        Ok(t)
    }

    /// A guiding text's or an analysis's title, from its game tag.
    fn title(&mut self, id: i64) -> cbformat::Result<String> {
        if let Some(t) = self.titles.get(&id) {
            return Ok(t.clone());
        }
        let t = clip(self.db.entities().title(id)?.unwrap_or_default());
        self.titles.insert(id, t.clone());
        Ok(t)
    }
}

/// One list row. Guiding texts and analyses have header layouts of their own
/// (only the first eight bytes are shared with games): their row carries the
/// title in `event` and the author in `annotator`, and no game fields.
fn row(names: &mut Names<'_>, r: &Record) -> cbformat::Result<String> {
    let base = Obj::new().num("number", r.id());
    let other = |base: Obj, kind: &str, title: String, author: String| {
        base.str("kind", kind)
            .str("white", "")
            .num("whiteElo", 0)
            .str("black", "")
            .num("blackElo", 0)
            .str("result", "*")
            .num("moves", 0)
            .str("eco", "")
            .str("event", &title)
            .str("site", "")
            .str("date", "????.??.??")
            .str("round", "")
            .str("annotator", &author)
            .raw("flags", &Obj::new().bool("deleted", r.is_deleted()).bool("chess960", false).done())
            .done()
    };
    match r.kind() {
        RecordKind::Text => Ok(other(base, "text", names.title(r.text_title())?, names.player(r.text_author())?)),
        RecordKind::Analysis => {
            Ok(other(base, "analysis", names.title(r.analysis_title())?, names.player(r.analysis_author())?))
        }
        RecordKind::Unknown(_) => Ok(other(base, "unknown", String::new(), String::new())),
        RecordKind::Game => {
            let (event, site) = names.tournament(r.tournament())?;
            let round = match (r.round(), r.subround()) {
                (n, _) if n <= 0 => String::new(),
                (n, s) if s <= 0 => n.to_string(),
                (n, s) => format!("{n}({s})"),
            };
            let flags =
                Obj::new().bool("deleted", r.is_deleted()).bool("chess960", matches!(r.eco(), Eco::Chess960(_))).done();
            Ok(base
                .str("kind", "game")
                .str("white", &names.player(r.white())?)
                .num("whiteElo", r.white_elo().max(0))
                .str("black", &names.player(r.black())?)
                .num("blackElo", r.black_elo().max(0))
                .str("result", r.result().pgn())
                .num("moves", r.move_count().max(0))
                .str("eco", &r.eco().pgn().unwrap_or_default())
                .str("event", &event)
                .str("site", &site)
                .str("date", &r.played_date().pgn())
                .str("round", &round)
                .str("annotator", &names.player(r.annotator())?)
                .raw("flags", &flags)
                .done())
        }
    }
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
        let rendered = {
            let _render = RENDERS.enter();
            open.db
                .moves_of_within(&before, MAX_GAME_BYTES)
                .and_then(|data| pgn::game_from(&open.db, &before, &data.moves()?))
        };
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
