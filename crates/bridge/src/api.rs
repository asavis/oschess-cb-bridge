//! The v1 endpoints of `docs/api.md`.

use std::collections::HashMap;
use std::sync::{Condvar, Mutex};

use cbformat::Error;
use cbformat::pgn;
use cbformat::v2::{Eco, ROUND_TEXT_BYTES, RecordKind, round_text};

use crate::access::{Policy, Verdict, cors};
use crate::budget;
use crate::catalog::{Catalog, Entry, State};
use crate::engine::{self, Engine, Limit, Search};
use crate::http::{Request, Response};
use crate::json::{self, Obj};
use crate::reply::{bad_parameter, error, error_with, not_found, ok};
use crate::search::query::Sort;
use crate::search::{self, SearchError, Selection, SuggestField};
use crate::store::{Head, Store, with_store};

pub const API_VERSION: i64 = 1;
pub const MAX_LIMIT: u32 = 500;
const DEFAULT_LIMIT: u32 = 200;
/// Reads of one game before a database that keeps changing is reported.
const GAME_ATTEMPTS: usize = 3;
/// The largest move or annotation record, content or spare area, served as
/// PGN. The largest record of any kind in a Mega Database is about 1.2 MB, a
/// guiding text; a record near the reader's 64 MiB limit would take gigabytes
/// to render.
pub const MAX_GAME_BYTES: usize = 2 << 20;
/// The largest game answer: its PGN written as JSON. Real games stay far
/// below; names or comments of control characters can grow sixfold in JSON.
pub const MAX_GAME_RESPONSE: usize = 8 << 20;
/// An upper bound for one list row in JSON: nine text fields of at most
/// [`MAX_FIELD_CHARS`] characters, each character at most six bytes escaped,
/// plus the keys and numbers.
const MAX_ROW_BYTES: usize = 9 * MAX_FIELD_CHARS * 6 + 512;
/// The most plies of a main line a row carries (#81): what an oschess player
/// tree indexes.
pub const MAX_LINE_PLIES: u8 = 60;
/// An upper bound for a row's `line`: a SAN is at most 7 characters (`exd8=Q+`,
/// `Qh4xe1#`), each followed by a space, plus the key.
const MAX_LINE_BYTES: usize = MAX_LINE_PLIES as usize * 8 + 16;
/// The move buffer of a window with lines: one move record at a time, within
/// [`MAX_GAME_BYTES`] and its frame.
const LINE_BUFFER_BYTES: usize = MAX_GAME_BYTES + 64;
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
    /// The engine of `bridge.toml`, or none.
    pub engine: Engine,
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
        ["v1", "databases", id, "games"] => with_entry(app, id, |e| games(app, e, req)),
        ["v1", "databases", id, "games", number] => with_entry(app, id, |e| game(app, e, number, req)),
        ["v1", "databases", id, "suggest"] => with_entry(app, id, |e| suggest(e, req)),
        ["v1", "databases", id, "explorer"] => with_entry(app, id, |e| crate::explorer::route(app, e, req)),
        ["v1", "engine", "analyze"] => analyze(app, req),
        _ => not_found(),
    }
}

fn with_entry(app: &App, id: &str, f: impl FnOnce(&Entry) -> Response) -> Response {
    match app.catalog.get(id) {
        Some(entry) => f(&entry),
        None => not_found(),
    }
}

fn status(app: &App) -> Response {
    let entries = app.catalog.entries();
    let states: Vec<State> = entries.iter().map(|e| e.state()).collect();
    let count = |s: State| states.iter().filter(|&&x| x == s).count() as i64;
    let dbs = Obj::new()
        .num("ready", count(State::Ready))
        .num("opening", count(State::Opening))
        .num("missing", count(State::Missing))
        .num("cloudOnly", count(State::CloudOnly))
        .num("downloading", count(State::Downloading))
        .num("unsupported", count(State::Unsupported))
        .num("unreadable", count(State::Unreadable))
        .done();
    let bridge = Obj::new().str("version", app.version).num("api", API_VERSION).done();
    let engine = match (app.engine.name(), app.engine.defaults()) {
        (Some(name), Some((threads, hash_mb))) => {
            let limits = engine::limits();
            // The defaults are within the limits already (`EngineConfig::new`).
            let range = |default: u32, max: u32| {
                Obj::new().num("default", i64::from(default)).num("max", i64::from(max)).done()
            };
            Obj::new()
                .str("name", &name)
                .raw("threads", &range(threads, limits.max_threads))
                .raw("hash", &range(hash_mb, limits.max_hash_mb))
                .done()
        }
        _ => "null".to_string(),
    };
    let mut body = Obj::new().raw("bridge", &bridge).raw("databases", &dbs).raw("engine", &engine);
    // The downloads running or queued, together.
    let downloads: Vec<_> = entries.iter().filter_map(|e| e.progress()).collect();
    if !downloads.is_empty() {
        let (present, total) = downloads.iter().fold((0, 0), |(p, t), d| (p + d.present(), t + d.total));
        body = body.raw("download", &progress(present, total));
    }
    // The position indexes being checked or built.
    let building = app.catalog.explorer.building();
    if !building.is_empty() {
        let items = building.iter().map(|(id, phase, done, total)| {
            Obj::new().str("id", id).str("phase", phase).num("done", *done as i64).num("total", *total as i64).done()
        });
        body = body.raw("indexing", &json::array(items));
    }
    ok(body.done())
}

/// `GET /v1/engine/analyze`: the engine's lines for a position, streamed.
fn analyze(app: &App, req: &Request) -> Response {
    if !app.engine.is_configured() {
        return error(409, "no_engine", "No engine is configured in the bridge");
    }
    let number = |name: &'static str, default: Option<u32>| -> Result<Option<u32>, Response> {
        match req.param(name) {
            None => Ok(default),
            Some(v) => v.parse().map(Some).map_err(|_| bad_parameter(name, &format!("{name} is a whole number"))),
        }
    };
    let multipv = match number("multipv", Some(1)) {
        Ok(n) => n.unwrap_or(1),
        Err(r) => return r,
    };
    let limit = match (number("depth", None), number("movetime", None)) {
        (Err(r), _) | (_, Err(r)) => return r,
        (Ok(None), Ok(None)) => Limit::Infinite,
        (Ok(Some(d)), Ok(None)) => Limit::Depth(d),
        (Ok(None), Ok(Some(t))) => Limit::MovetimeMs(t),
        (Ok(Some(_)), Ok(Some(_))) => return bad_parameter("movetime", "Give depth or movetime, not both"),
    };
    let stream = req.param("stream").unwrap_or_default();
    if stream.len() > 64 || !stream.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return bad_parameter("stream", "stream is at most 64 letters, digits, - and _");
    }
    let (threads, hash_mb) = match (number("threads", None), number("hash", None)) {
        (Err(r), _) | (_, Err(r)) => return r,
        (Ok(threads), Ok(hash_mb)) => (threads, hash_mb),
    };
    let search = Search::new(req.param("fen"), req.param("moves").unwrap_or_default(), multipv, limit)
        .and_then(|s| s.with_resources(threads, hash_mb, engine::limits()));
    let search = match search {
        Ok(search) => search,
        Err((parameter, message)) => return bad_parameter(parameter, &message),
    };
    let (engine, stream) = (app.engine.clone(), stream.to_string());
    Response::stream(200, move |sink| engine.analyze(&search, &stream, sink))
}

fn progress(present: u64, total: u64) -> String {
    Obj::new().num("present", present as i64).num("total", total as i64).done()
}

fn databases(app: &App) -> Response {
    let entries = app.catalog.entries();
    let items = entries.iter().map(|e| {
        let o = Obj::new().str("id", &e.id).str("name", &e.name).str("format", e.format.name());
        match e.open() {
            Ok(open) => o
                .str("state", State::Ready.name())
                .num("records", open.db.record_count())
                .str("generation", &format!("{:016x}", open.generation))
                .done(),
            Err(state) => {
                let o = o.str("state", state.name());
                match (state, e.progress()) {
                    (State::Downloading, Some(p)) => {
                        o.num("size", p.total as i64).raw("progress", &progress(p.present(), p.total)).done()
                    }
                    (State::Opening, _) => match e.opening() {
                        Some(p) => o.raw("progress", &progress(p.present(), p.total)).done(),
                        None => o.done(),
                    },
                    (State::CloudOnly | State::Downloading, _) => o.num("size", e.size() as i64).done(),
                    _ => o.done(),
                }
            }
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

/// The answer when the response budget cannot hold another large body now.
fn busy() -> Response {
    error(503, "busy", "Too many large answers are being sent; retry")
}

fn database_changing() -> Response {
    error(503, "database_changing", "The database changed while it was read; retry")
}

fn games(app: &App, entry: &Entry, req: &Request) -> Response {
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
    let sort_param = match req.param("sort") {
        None => None,
        Some(text) => match Sort::parse(text) {
            Some(sort) => Some(sort),
            None => return bad_parameter("sort", "unknown sort key"),
        },
    };
    let line = match req.param("line").map(str::parse::<u8>) {
        None => None,
        Some(Ok(n)) if (1..=MAX_LINE_PLIES).contains(&n) => Some(n),
        Some(_) => return bad_parameter("line", "line must be between 1 and 60"),
    };
    let open = match entry.open_to_read() {
        Ok(open) => open,
        Err(state) => return unavailable(state),
    };
    let stream = match req.param("stream") {
        None => None,
        Some(s) if valid_stream(s) => Some(s),
        Some(_) => return bad_parameter("stream", "stream must be 1 to 64 characters of A-Z, a-z, 0-9, - and _"),
    };
    let (selection, sort) = match search::select(&*open.db, &open.indexes, req.param("q"), stream, sort_param) {
        Ok(found) => found,
        Err(e) => return search_error(entry, open.generation, e),
    };
    // Reserved before the rows are built and held until the answer is written.
    let size = match line {
        None => limit as usize * MAX_ROW_BYTES,
        Some(_) => limit as usize * (MAX_ROW_BYTES + MAX_LINE_BYTES) + LINE_BUFFER_BYTES,
    };
    let Some(hold) = budget::reserve(size) else { return busy() };
    let Some(mut lines) = Lines::new(line) else { return busy() };
    let (total, rows) = match &selection {
        Selection::All { descending } => {
            let total = u64::from(open.db.record_count());
            let count = total.saturating_sub(offset).min(u64::from(limit)) as u32;
            let rows = if count == 0 {
                Ok(Vec::new())
            } else {
                // Numbers in the window, in ascending order; reversed for descending.
                let first = if *descending { total - offset - u64::from(count) + 1 } else { offset + 1 } as u32;
                with_store!(&*open.db, db => window(db, first, count, &mut lines)).map(|mut rows| {
                    if *descending {
                        rows.reverse();
                    }
                    rows
                })
            };
            (total, rows)
        }
        Selection::Numbers(numbers) => {
            let start = usize::try_from(offset).unwrap_or(usize::MAX).min(numbers.len());
            let end = start.saturating_add(limit as usize).min(numbers.len());
            let rows = with_store!(&*open.db, db => rows_of(db, &numbers[start..end], &mut lines));
            (numbers.len() as u64, rows)
        }
    };
    let rows = match rows {
        Ok(rows) => rows,
        Err(e) if changing(entry, open.generation, &e) => return database_changing(),
        Err(e) => return error(500, "internal", &e.to_string()),
    };
    // Lines are read from the move records as well: a database that changed
    // while they were read is reported, as a game's read reports it.
    if lines.is_some() {
        if let Some(hook) = &app.between_reads {
            hook();
        }
        if entry.generation() != Some(open.generation) {
            return database_changing();
        }
    }
    let body = Obj::new()
        .str("generation", &format!("{:016x}", open.generation))
        .num("total", total as i64)
        .num("offset", offset as i64)
        .str("sort", &sort.name())
        .raw("rows", &json::array(rows))
        .done();
    ok(body).holding(hold)
}

fn suggest(entry: &Entry, req: &Request) -> Response {
    let field = match req.param("field") {
        Some("player") => SuggestField::Player,
        Some("event") => SuggestField::Event,
        Some("annotator") => SuggestField::Annotator,
        _ => return bad_parameter("field", "field must be player, event or annotator"),
    };
    let Some(prefix) = req.param("prefix").filter(|p| !p.trim().is_empty()) else {
        return bad_parameter("prefix", "prefix must not be empty");
    };
    let limit = match req.param("limit").map(str::parse::<usize>) {
        None => 20,
        Some(Ok(n)) if (1..=20).contains(&n) => n,
        Some(_) => return bad_parameter("limit", "limit must be between 1 and 20"),
    };
    let open = match entry.open() {
        Ok(open) => open,
        Err(state) => return unavailable(state),
    };
    let list = match search::suggest(&*open.db, &open.indexes, field, prefix, limit) {
        Ok(list) => list,
        Err(e) => return search_error(entry, open.generation, e),
    };
    // A name escapes to at most 6 bytes a byte in JSON, and its label is shorter.
    let size = list.iter().map(|s| s.name.len() * 12 + 128).sum::<usize>() + 256;
    let Some(hold) = budget::reserve(size) else { return busy() };
    let items = list
        .iter()
        .map(|s| Obj::new().str("value", &s.name).str("label", &clip(s.name.clone())).num("games", s.games).done());
    let field = req.param("field").unwrap_or_default();
    ok(Obj::new().str("field", field).raw("suggestions", &json::array(items)).done()).holding(hold)
}

/// A client's stream name: 1 to 64 characters of `A-Z`, `a-z`, `0-9`, `-` and `_`.
fn valid_stream(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The answer to a search that could not finish.
fn search_error(entry: &Entry, generation: u64, e: SearchError) -> Response {
    match e {
        SearchError::Unsupported(qualifier) => {
            error_with(400, "unsupported_qualifier", "ChessBase databases do not have this qualifier", |o| {
                o.str("qualifier", &qualifier)
            })
        }
        SearchError::Superseded => error(409, "superseded", "A newer search on this database replaced this one"),
        SearchError::TooLarge => {
            error(422, "database_too_large", "The database is too large to search or sort within the memory budget")
        }
        SearchError::Busy => error(503, "busy", "Search memory is taken by other searches; retry"),
        SearchError::Read(e) if changing(entry, generation, &e) => database_changing(),
        SearchError::Read(e) => error(500, "internal", &e.to_string()),
    }
}

/// Rows `first..first + count` in one header read.
fn window<S: Store>(db: &S, first: u32, count: u32, lines: &mut Option<Lines>) -> cbformat::Result<Vec<String>> {
    // `first + count` itself may not fit when the window ends at `u32::MAX`.
    let records = db.records(first, first + (count - 1))?;
    let mut names = Names::new(db);
    records.iter().map(|r| row(&mut names, lines, r)).collect()
}

/// The rows of the records `numbers`, in that order.
fn rows_of<S: Store>(db: &S, numbers: &[u32], lines: &mut Option<Lines>) -> cbformat::Result<Vec<String>> {
    let mut names = Names::new(db);
    numbers.iter().map(|&n| db.record(n).and_then(|r| row(&mut names, lines, &r))).collect()
}

/// The `line` a window's game rows carry, and the buffer their move records
/// are read into, one at a time.
struct Lines {
    plies: u8,
    buf: Vec<u8>,
}

impl Lines {
    /// `Some(None)` for a window without lines; `None` when the buffer
    /// cannot be had.
    fn new(plies: Option<u8>) -> Option<Option<Lines>> {
        let Some(plies) = plies else { return Some(None) };
        let mut buf = Vec::new();
        buf.try_reserve_exact(LINE_BUFFER_BYTES).ok()?;
        Some(Some(Lines { plies, buf }))
    }
}

/// The longest text, in characters, a list row carries in one field. Longer
/// names are cut and end with `…`; the game's PGN has them in full. With 500
/// rows this bounds a window to a few megabytes, whatever an entity holds.
pub const MAX_FIELD_CHARS: usize = 200;

pub(crate) fn clip(text: String) -> String {
    match text.char_indices().nth(MAX_FIELD_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

/// Entity names for one window, each looked up once: rows of a tournament
/// share their players and event, and one entity can be up to a megabyte.
struct Names<'a, S: Store> {
    db: &'a S,
    players: HashMap<i64, String>,
    tournaments: HashMap<i64, (String, String)>,
    /// Where annotators are not players.
    annotators: HashMap<i64, String>,
    titles: HashMap<i64, String>,
}

impl<'a, S: Store> Names<'a, S> {
    fn new(db: &'a S) -> Self {
        Names {
            db,
            players: HashMap::new(),
            tournaments: HashMap::new(),
            annotators: HashMap::new(),
            titles: HashMap::new(),
        }
    }

    fn player(&mut self, id: i64) -> cbformat::Result<String> {
        if let Some(name) = self.players.get(&id) {
            return Ok(name.clone());
        }
        let name = clip(self.db.player(id)?.map(|p| p.pgn()).unwrap_or_default());
        self.players.insert(id, name.clone());
        Ok(name)
    }

    /// An annotator or author: a player where annotators are players.
    fn annotator(&mut self, id: i64) -> cbformat::Result<String> {
        if S::ANNOTATORS_ARE_PLAYERS {
            return self.player(id);
        }
        if let Some(name) = self.annotators.get(&id) {
            return Ok(name.clone());
        }
        let name = clip(self.db.annotator(id)?.unwrap_or_default());
        self.annotators.insert(id, name.clone());
        Ok(name)
    }

    /// The tournament's title and place.
    fn tournament(&mut self, id: i64) -> cbformat::Result<(String, String)> {
        if let Some(t) = self.tournaments.get(&id) {
            return Ok(t.clone());
        }
        let t = self.db.tournament(id)?.map_or_else(Default::default, |t| (clip(t.title), clip(t.place)));
        self.tournaments.insert(id, t.clone());
        Ok(t)
    }

    /// A guiding text's or an analysis's title, by its key ([`Head::other`]).
    fn title(&mut self, key: i64) -> cbformat::Result<String> {
        if let Some(t) = self.titles.get(&key) {
            return Ok(t.clone());
        }
        let t = clip(self.db.title(key)?.unwrap_or_default());
        self.titles.insert(key, t.clone());
        Ok(t)
    }
}

/// One list row. Guiding texts and analyses have header layouts of their own
/// (only the first eight bytes are shared with games): their row carries the
/// title in `event` and the author in `annotator`, and no game fields.
fn row<S: Store>(names: &mut Names<'_, S>, lines: &mut Option<Lines>, r: &S::Head) -> cbformat::Result<String> {
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
        kind @ (RecordKind::Text | RecordKind::Analysis) => {
            let (title, author) = r.other().unwrap_or((-1, -1));
            let kind = if kind == RecordKind::Text { "text" } else { "analysis" };
            Ok(other(base, kind, names.title(title)?, names.annotator(author)?))
        }
        RecordKind::Unknown(_) => Ok(other(base, "unknown", String::new(), String::new())),
        RecordKind::Game => {
            let (event, site) = names.tournament(r.tournament())?;
            // The fields as search matches them and PGN writes them (#68).
            let (n, s) = r.round();
            let mut buf = [0; ROUND_TEXT_BYTES];
            let round = round_text(n, s, &mut buf);
            let flags =
                Obj::new().bool("deleted", r.is_deleted()).bool("chess960", matches!(r.eco(), Eco::Chess960(_))).done();
            let (white_elo, black_elo) = r.elo();
            let row = base
                .str("kind", "game")
                .str("white", &names.player(r.white())?)
                .num("whiteElo", white_elo.max(0))
                .str("black", &names.player(r.black())?)
                .num("blackElo", black_elo.max(0))
                .str("result", r.result().pgn())
                .num("moves", r.move_count().max(0))
                .str("eco", &r.eco().pgn().unwrap_or_default())
                .str("event", &event)
                .str("site", &site)
                .str("date", &r.played_date().pgn())
                .str("round", round)
                .str("annotator", &names.annotator(r.annotator())?)
                .raw("flags", &flags);
            Ok(match lines {
                None => row,
                Some(lines) => match names.db.main_line(r, lines.plies, &mut lines.buf)? {
                    Some(line) => row.str("line", &line),
                    None => row.raw("line", "null"),
                },
            }
            .done())
        }
    }
}

/// One reading of a game.
enum Attempt {
    NotFound,
    NotAGame,
    /// The header changed while the game was read, or could not be read.
    Changed,
    Rendered(cbformat::Result<pgn::Rendered>),
}

/// Reads game `number` of `db` as PGN between two reads of its header. The
/// move and annotation records are both read between them, so a change to
/// either is detected the same way.
fn attempt<S: Store>(app: &App, db: &S, number: u32, options: &pgn::Options) -> Attempt {
    if number > db.record_count() {
        return Attempt::NotFound;
    }
    let Ok(before) = db.record(number) else { return Attempt::Changed };
    if !matches!(before.kind(), RecordKind::Game) {
        return Attempt::NotAGame;
    }
    let rendered = {
        let _render = RENDERS.enter();
        db.render(&before, options)
    };
    if let Some(hook) = &app.between_reads {
        hook();
    }
    match db.record(number).is_ok_and(|after| after.bytes() == before.bytes()) {
        true => Attempt::Rendered(rendered),
        false => Attempt::Changed,
    }
}

fn game(app: &App, entry: &Entry, number: &str, req: &Request) -> Response {
    let Some(number) = number.parse::<u32>().ok().filter(|&n| n > 0) else { return not_found() };
    // Languages ChessBase has no number for are passed over; English is the default.
    let mut options = pgn::Options::with_languages(req.param("lang").unwrap_or("en").split(','));
    options.full = match req.param("annotations") {
        None | Some("reading") => false,
        Some("full") => true,
        Some(_) => return bad_parameter("annotations", "annotations must be reading or full"),
    };
    for _ in 0..GAME_ATTEMPTS {
        let open = match entry.open_to_read() {
            Ok(open) => open,
            Err(state) => return unavailable(state),
        };
        let rendered = match with_store!(&*open.db, db => attempt(app, db, number, &options)) {
            Attempt::NotFound => return not_found(),
            Attempt::NotAGame => {
                return error(422, "not_a_game", "Guiding texts and analyses are not served as PGN");
            }
            Attempt::Changed => continue,
            Attempt::Rendered(rendered) => rendered,
        };
        if entry.generation() != Some(open.generation) {
            continue;
        }
        return match rendered {
            Ok(rendered) => {
                let text = rendered.pgn;
                // The PGN, and at most 192 bytes of keys, numbers and the status.
                let size = json::string_len(&text) + 192;
                if size > MAX_GAME_RESPONSE {
                    let reason =
                        format!("the game's answer would be {size} bytes, over the {MAX_GAME_RESPONSE}-byte limit");
                    return error_with(422, "unreadable_game", "The game is too large to serve", |o| {
                        o.str("reason", &reason)
                    });
                }
                // Reserved before the answer is built and held until it is written.
                let Some(hold) = budget::reserve(size) else { return busy() };
                let body = Obj::new()
                    .str("generation", &format!("{:016x}", open.generation))
                    .num("number", number)
                    .str("pgn", &text);
                let body = match rendered.annotations {
                    pgn::AnnotationStatus::None => body.str("annotations", "none"),
                    pgn::AnnotationStatus::Complete => body.str("annotations", "complete"),
                    pgn::AnnotationStatus::Incomplete { type_code } => {
                        body.str("annotations", "incomplete").num("unreadableAnnotation", type_code)
                    }
                };
                ok(body.done()).holding(hold)
            }
            Err(Error::Io(..)) => database_changing(),
            Err(e) => error_with(422, "unreadable_game", "The game's records are damaged", |o| {
                o.str("reason", &e.to_string())
            }),
        };
    }
    database_changing()
}
