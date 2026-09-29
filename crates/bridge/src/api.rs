//! The v1 endpoints of `docs/api.md`.

use std::fmt::Display;
use std::ops::RangeInclusive;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};

use cbformat::Error;
use cbformat::game::RecordKind;
use cbformat::pgn;
use chesscore::Board;

use crate::access::{Policy, Verdict, cors};
use crate::budget;
use crate::catalog::{Catalog, Entry, State};
use crate::engine::{self, Engine, Limit, Search};
use crate::explorer;
use crate::foreground;
use crate::http::{Request, Response};
use crate::json::{self, Obj};
use crate::reply::{bad_parameter, error, error_with, not_found, ok, unavailable};
use crate::rows::{LINE_BUFFER_BYTES, Lines, MAX_ROW_BYTES, Names, clip, row};
use crate::search::query::{Sort, Unsupported};
use crate::search::{self, SearchError, Selection, SuggestField};
use crate::snapshot::Database;
use crate::store::{Head, Store, with_store};
use crate::token;

pub const API_VERSION: i64 = 1;
pub const MAX_LIMIT: u32 = 500;
const DEFAULT_LIMIT: u32 = 200;
/// Reads of one game before a database that keeps changing is reported.
const GAME_ATTEMPTS: usize = 3;
/// The largest game answer: its PGN written as JSON. Real games stay far
/// below; names or comments of control characters can grow sixfold in JSON.
pub const MAX_GAME_RESPONSE: usize = 8 << 20;
/// The most plies of a main line a row carries (#81): what an oschess player
/// tree indexes.
pub const MAX_LINE_PLIES: u8 = 60;
/// An upper bound for a row's `line`: a SAN is at most 7 characters (`exd8=Q+`,
/// `Qh4xe1#`), each followed by a space, plus the key.
const MAX_LINE_BYTES: usize = MAX_LINE_PLIES as usize * 8 + 16;
/// Games rendered at once; the others wait. With
/// [`MAX_GAME_BYTES`](crate::store::MAX_GAME_BYTES) this keeps rendering
/// within a few hundred megabytes whatever the requests.
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
    /// Whether a request passed the policy since the start: a paired browser
    /// reached the bridge. The Windows app's first-run window stops waiting
    /// for it then.
    pub served: AtomicBool,
}

impl App {
    /// A bridge serving `catalog` under `policy`, with no engine and no hook
    /// between reads; a caller that needs either sets it with struct update
    /// syntax, `App { engine, ..App::new(..) }`.
    pub fn new(version: &'static str, policy: Policy, catalog: Catalog) -> App {
        App { version, policy, catalog, between_reads: None, engine: Engine::none(), served: AtomicBool::new(false) }
    }
}

pub fn handle(app: &App, req: &Request) -> Response {
    match app.policy.check(req) {
        Verdict::Answer(response) => response,
        Verdict::Serve { origin } => {
            app.served.store(true, Ordering::Relaxed);
            cors(route(app, req), origin.as_deref())
        }
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
        ["v1", "databases", id, "suggest"] => with_entry(app, id, |e| suggest(app, e, req)),
        ["v1", "databases", id, "explorer"] => with_entry(app, id, |e| crate::explorer::route(app, e, req)),
        ["v1", "engine", "analyze"] => analyze(app, req),
        ["v1", "engine", "warm"] => warm(app, req),
        _ => not_found(),
    }
}

/// The answer `f` gives about database `id`, `404` when it is not listed.
/// It is work a user waits for: background builds give way to it while it
/// runs (#149).
fn with_entry(app: &App, id: &str, f: impl FnOnce(&Entry) -> Response) -> Response {
    match app.catalog.get(id) {
        Some(entry) => {
            let _working = foreground::begin();
            f(&entry)
        }
        None => not_found(),
    }
}

fn status(app: &App) -> Response {
    let entries = app.catalog.entries();
    let states: Vec<State> = entries.iter().map(|e| Database::of(e).state).collect();
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
        return no_engine();
    }
    let AnalyzeQuery { search, stream } = match AnalyzeQuery::parse(req) {
        Ok(query) => query,
        Err(answer) => return answer,
    };
    let engine = app.engine.clone();
    Response::stream(200, move |sink| engine.analyze(&search, &stream, sink))
}

/// Starts the engine before an analysis asks for it (#110), with the same
/// `threads` and `hash` an analysis takes.
fn warm(app: &App, req: &Request) -> Response {
    if !app.engine.is_configured() {
        return no_engine();
    }
    let WarmQuery { search } = match WarmQuery::parse(req) {
        Ok(query) => query,
        Err(answer) => return answer,
    };
    match app.engine.warm(&search) {
        engine::Warmed::Ready => Response::json(200, Obj::new().str("engine", "ready").done()),
        engine::Warmed::Busy => Response::json(200, Obj::new().str("engine", "busy").done()),
        engine::Warmed::NoEngine => no_engine(),
        engine::Warmed::Failed(why) => error(502, "engine_failed", &why),
    }
}

fn no_engine() -> Response {
    error(409, "no_engine", "No engine is configured in the bridge")
}

fn progress(present: u64, total: u64) -> String {
    Obj::new().num("present", present as i64).num("total", total as i64).done()
}

fn databases(app: &App) -> Response {
    let entries = app.catalog.entries();
    let items = entries.iter().map(|e| database(&Database::of(e)));
    ok(Obj::new().raw("databases", &json::array(items)).done())
}

/// A database of `GET /v1/databases`, as the tray app shows it too.
fn database(d: &Database) -> String {
    let mut o = Obj::new().str("id", &d.id).str("name", &d.name).str("format", d.format).str("state", d.state.name());
    if let Some(records) = d.records {
        o = o.num("records", records);
    }
    if let Some(generation) = d.generation {
        o = o.str("generation", &format!("{generation:016x}"));
    }
    if let Some(size) = d.size {
        o = o.num("size", size as i64);
    }
    if let Some((present, total)) = d.progress {
        o = o.raw("progress", &progress(present, total));
    }
    o.done()
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
    let GamesQuery { offset, limit, sort: sort_param, line, board, stream, q } = match GamesQuery::parse(req) {
        Ok(query) => query,
        Err(answer) => return answer,
    };
    // The games of a position mark the database in use (#149); a list alone
    // does not.
    if board.is_some() {
        app.catalog.explorer.mark_in_use(&entry.id);
    }
    let open = match entry.open_to_read() {
        Ok(open) => open,
        Err(state) => return unavailable(state),
    };
    app.catalog.attach_heads(entry, &open);
    let (db, idx) = (&open.db, &open.indexes);
    let selected = match &board {
        Some(board) => {
            let loaded = match explorer::ready(app, entry, &open) {
                Ok(loaded) => loaded,
                Err(answer) => return answer,
            };
            let games = explorer::positions::Games { loaded: &loaded, board };
            search::select_position(db, idx, q, stream, sort_param, &games).map(|(s, sort, n)| (s, sort, Some(n)))
        }
        None => search::select(db, idx, q, stream, sort_param).map(|(s, sort)| (s, sort, None)),
    };
    let (selection, sort, games) = match selected {
        Ok(found) => found,
        Err(e) => return search_error(app, entry, open.generation, e),
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
        Err(e) => return internal(&entry.id, &e),
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
    let mut body = Obj::new()
        .str("generation", &format!("{:016x}", open.generation))
        .num("total", total as i64)
        .num("offset", offset as i64)
        .str("sort", &sort.name());
    // The position acknowledged, as the bridge writes it, with its games
    // before `q`.
    if let (Some(board), Some(games)) = (&board, games) {
        body = body.raw("position", &Obj::new().str("fen", &board.fen()).num("games", games as i64).done());
    }
    ok(body.raw("rows", &json::array(rows)).done()).holding(hold)
}

fn suggest(app: &App, entry: &Entry, req: &Request) -> Response {
    let SuggestQuery { field, field_name, prefix, limit } = match SuggestQuery::parse(req) {
        Ok(query) => query,
        Err(answer) => return answer,
    };
    let open = match entry.open() {
        Ok(open) => open,
        Err(state) => return unavailable(state),
    };
    app.catalog.attach_heads(entry, &open);
    let list = match search::suggest(&open.db, &open.indexes, field, prefix, limit) {
        Ok(list) => list,
        Err(e) => return search_error(app, entry, open.generation, e),
    };
    // A name escapes to at most 6 bytes a byte in JSON, and its label is shorter.
    let size = list.iter().map(|s| s.name.len() * 12 + 128).sum::<usize>() + 256;
    let Some(hold) = budget::reserve(size) else { return busy() };
    let items = list
        .iter()
        .map(|s| Obj::new().str("value", &s.name).str("label", &clip(s.name.clone())).num("games", s.games).done());
    ok(Obj::new().str("field", field_name).raw("suggestions", &json::array(items)).done()).holding(hold)
}

/// The answer to a search that could not finish.
fn search_error(app: &App, entry: &Entry, generation: u64, e: SearchError) -> Response {
    match e {
        SearchError::Unsupported(qualifier) => unsupported_qualifier(&qualifier),
        SearchError::Superseded => error(409, "superseded", "A newer search on this database replaced this one"),
        SearchError::TooLarge => {
            error(422, "database_too_large", "The database is too large to search or sort within the memory budget")
        }
        SearchError::Busy => error(503, "busy", "Search memory is taken by other searches; retry"),
        SearchError::Read(e) if changing(entry, generation, &e) => database_changing(),
        SearchError::Read(e) => internal(&entry.id, &e),
        SearchError::IndexDamaged => explorer::rebuilding(app, entry),
    }
}

/// The answer to a search text that uses `qualifier`, which only the oschess
/// Library has (`docs/search-grammar.md`).
fn unsupported_qualifier(qualifier: &str) -> Response {
    error_with(400, "unsupported_qualifier", "ChessBase databases do not have this qualifier", |o| {
        o.str("qualifier", qualifier)
    })
}

/// The answer to a read of database `id` that failed with a bug: `500
/// internal`, logged with the id and the error. The error's path is left
/// out of both (#117, #173).
fn internal(id: &str, e: &Error) -> Response {
    let why = crate::log::error(e);
    crate::log!("internal error on database {id}: {why}");
    error(500, "internal", &why)
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
    let GameQuery { number, options } = match GameQuery::parse(number, req) {
        Ok(query) => query,
        Err(answer) => return answer,
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
                o.str("reason", &crate::log::error(&e))
            }),
        };
    }
    database_changing()
}

// The parameters of each endpoint, checked before anything is read or
// started: a request refused for one of them marks no database in use and
// starts no download (#173). Where several are wrong, the first refused is
// the first checked here.

/// The parameters of `GET /v1/databases/{id}/games`.
struct GamesQuery<'r> {
    offset: u64,
    limit: u32,
    sort: Option<Sort>,
    line: Option<u8>,
    /// The position of `fen`, whose games alone are listed (#148).
    board: Option<Board>,
    stream: Option<&'r str>,
    q: Option<&'r str>,
}

impl<'r> GamesQuery<'r> {
    fn parse(req: &'r Request) -> Result<GamesQuery<'r>, Response> {
        let offset = match req.param("offset") {
            None => 0,
            Some(text) => text.parse::<u64>().map_err(|_| bad_parameter("offset", "offset must be a whole number"))?,
        };
        let limit = bounded(req, "limit", 1..=MAX_LIMIT)?.unwrap_or(DEFAULT_LIMIT);
        let sort = match req.param("sort") {
            None => None,
            Some(text) => Some(Sort::parse(text).ok_or_else(|| bad_parameter("sort", "unknown sort key"))?),
        };
        let line = bounded(req, "line", 1..=MAX_LINE_PLIES)?;
        // The games of a position (#148): its FEN checked as the explorer
        // checks it, `variant` with it only.
        let board = match req.param("fen") {
            None => None,
            Some(_) if req.param("variant").is_some_and(|v| v != "standard") => return Err(explorer::unsupported()),
            Some(fen) => Some(explorer::board(fen)?),
        };
        let stream = match req.param("stream") {
            Some(s) if s.is_empty() || !stream_name(s) => {
                return Err(bad_parameter("stream", "stream must be 1 to 64 characters of A-Z, a-z, 0-9, - and _"));
            }
            stream => stream,
        };
        // The search text's one refusal needs no database: a qualifier only
        // the Library has. The search reads the text again, as its grammar's
        // owner; every other way a search fails needs the database.
        let q = req.param("q");
        if let Some(Err(Unsupported(qualifier))) = q.map(search::query::parse) {
            return Err(unsupported_qualifier(&qualifier));
        }
        Ok(GamesQuery { offset, limit, sort, line, board, stream, q })
    }
}

/// The parameters of `GET /v1/databases/{id}/games/{number}`, with the
/// number itself: `404` unless it is a record's.
struct GameQuery {
    number: u32,
    options: pgn::Options,
}

impl GameQuery {
    fn parse(number: &str, req: &Request) -> Result<GameQuery, Response> {
        let Some(number) = number.parse::<u32>().ok().filter(|&n| n > 0) else { return Err(not_found()) };
        // Languages ChessBase has no number for are passed over; English is the default.
        let mut options = pgn::Options::with_languages(req.param("lang").unwrap_or("en").split(','));
        options.full = match req.param("annotations") {
            None | Some("reading") => false,
            Some("full") => true,
            Some(_) => return Err(bad_parameter("annotations", "annotations must be reading or full")),
        };
        Ok(GameQuery { number, options })
    }
}

/// The parameters of `GET /v1/databases/{id}/suggest`.
struct SuggestQuery<'r> {
    field: SuggestField,
    /// `field` as the request names it, which the answer repeats.
    field_name: &'r str,
    prefix: &'r str,
    limit: usize,
}

impl<'r> SuggestQuery<'r> {
    fn parse(req: &'r Request) -> Result<SuggestQuery<'r>, Response> {
        let field_name = req.param("field").unwrap_or_default();
        let field = match field_name {
            "player" => SuggestField::Player,
            "event" => SuggestField::Event,
            "annotator" => SuggestField::Annotator,
            _ => return Err(bad_parameter("field", "field must be player, event or annotator")),
        };
        let Some(prefix) = req.param("prefix").filter(|p| !p.trim().is_empty()) else {
            return Err(bad_parameter("prefix", "prefix must not be empty"));
        };
        let limit = bounded(req, "limit", 1..=20)?.unwrap_or(20);
        Ok(SuggestQuery { field, field_name, prefix, limit })
    }
}

/// The parameters of `GET /v1/engine/analyze`: the search, and the stream
/// that names the client's view.
struct AnalyzeQuery {
    search: Search,
    stream: String,
}

impl AnalyzeQuery {
    fn parse(req: &Request) -> Result<AnalyzeQuery, Response> {
        let multipv = whole(req, "multipv")?.unwrap_or(1);
        let limit = match (whole(req, "depth")?, whole(req, "movetime")?) {
            (None, None) => Limit::Infinite,
            (Some(d), None) => Limit::Depth(d),
            (None, Some(t)) => Limit::MovetimeMs(t),
            (Some(_), Some(_)) => return Err(bad_parameter("movetime", "Give depth or movetime, not both")),
        };
        let stream = req.param("stream").unwrap_or_default();
        if !stream_name(stream) {
            return Err(bad_parameter("stream", "stream is at most 64 letters, digits, - and _"));
        }
        let (threads, hash_mb) = resources(req)?;
        let search = Search::new(req.param("fen"), req.param("moves").unwrap_or_default(), multipv, limit)
            .and_then(|s| s.with_resources(threads, hash_mb, engine::limits()));
        Ok(AnalyzeQuery { search: checked(search)?, stream: stream.to_string() })
    }
}

/// The parameters of `GET /v1/engine/warm`: a search with the `threads` and
/// `hash` of the analysis to come.
struct WarmQuery {
    search: Search,
}

impl WarmQuery {
    fn parse(req: &Request) -> Result<WarmQuery, Response> {
        let (threads, hash_mb) = resources(req)?;
        let search = Search::new(None, "", 1, Limit::Infinite)
            .and_then(|s| s.with_resources(threads, hash_mb, engine::limits()));
        Ok(WarmQuery { search: checked(search)? })
    }
}

/// Parameter `name`: `None` when absent, else a whole number within `range`;
/// `400` naming it otherwise.
fn bounded<T: FromStr + PartialOrd + Display>(
    req: &Request,
    name: &str,
    range: RangeInclusive<T>,
) -> Result<Option<T>, Response> {
    match req.param(name).map(str::parse::<T>) {
        None => Ok(None),
        Some(Ok(n)) if range.contains(&n) => Ok(Some(n)),
        Some(_) => Err(bad_parameter(name, &format!("{name} must be between {} and {}", range.start(), range.end()))),
    }
}

/// Parameter `name` of the engine's endpoints: `None` when absent, else a
/// whole number, whose bounds [`Search`] checks; `400` naming it otherwise.
fn whole(req: &Request, name: &str) -> Result<Option<u32>, Response> {
    match req.param(name).map(str::parse::<u32>) {
        None => Ok(None),
        Some(Ok(n)) => Ok(Some(n)),
        Some(Err(_)) => Err(bad_parameter(name, &format!("{name} is a whole number"))),
    }
}

/// `threads` and `hash`, which an analysis and a warm-up take alike.
fn resources(req: &Request) -> Result<(Option<u32>, Option<u32>), Response> {
    Ok((whole(req, "threads")?, whole(req, "hash")?))
}

/// `search` as [`Search`] checked it: `400` naming the parameter at fault.
fn checked(search: Result<Search, (&'static str, String)>) -> Result<Search, Response> {
    search.map_err(|(parameter, message)| bad_parameter(parameter, &message))
}

/// Whether `s` can name a client's stream: at most 64 characters of `A-Z`,
/// `a-z`, `0-9`, `-` and `_` (`docs/api.md`, "Cancellation"). A list's
/// stream is not empty either; an analysis without one has the empty one.
fn stream_name(s: &str) -> bool {
    s.len() <= 64 && token::is_base64url(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The start position, and as a query writes it.
    const START: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
    const START_PARAM: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR+w+KQkq+-+0+1";

    /// The status and body of the answer a parser refused a request with.
    fn refused<T>(parsed: Result<T, Response>) -> (u16, String) {
        match parsed {
            Ok(_) => panic!("accepted"),
            Err(answer) => (answer.status, answer.body),
        }
    }

    /// `400 bad_request` naming `parameter`, whole.
    fn bad(parameter: &str, message: &str) -> (u16, String) {
        (400, format!(r#"{{"error":{{"code":"bad_request","message":"{message}","parameter":"{parameter}"}}}}"#))
    }

    /// A list's parameters are refused as they always were, the first
    /// checked first, with no database at hand (#173).
    #[test]
    fn a_list_refuses_each_parameter() {
        let refusal = |query: &str| refused(GamesQuery::parse(&Request::get(&format!("/?{query}"))));
        let stream = "stream must be 1 to 64 characters of A-Z, a-z, 0-9, - and _";
        let long = format!("stream={}", "a".repeat(65));
        for (query, parameter, message) in [
            ("offset=-1", "offset", "offset must be a whole number"),
            ("limit=0", "limit", "limit must be between 1 and 500"),
            ("limit=501", "limit", "limit must be between 1 and 500"),
            ("sort=elo", "sort", "unknown sort key"),
            ("line=0", "line", "line must be between 1 and 60"),
            ("line=256", "line", "line must be between 1 and 60"),
            ("fen=", "fen", "fen is not a valid position"),
            ("fen=nonsense", "fen", "fen is not a valid position"),
            ("stream=", "stream", stream),
            ("stream=bad!", "stream", stream),
            (long.as_str(), "stream", stream),
            ("stream=bad!&fen=nonsense&limit=0", "limit", "limit must be between 1 and 500"),
            ("stream=bad!&fen=nonsense", "fen", "fen is not a valid position"),
        ] {
            assert_eq!(refusal(query), bad(parameter, message), "{query}");
        }
        let unsupported =
            r#"{"error":{"code":"unsupported","message":"Chess960 positions are not indexed","variant":"chess960"}}"#;
        for query in ["fen=4k3/8/8/8/8/8/8/4KR1R+w+F+-+0+1", "fen=nonsense&variant=chess960"] {
            assert_eq!(refusal(query), (422, unsupported.to_string()), "{query}");
        }
        // The Library-only qualifiers of `docs/search-grammar.md`, in any
        // form, checked last.
        let with_a_position = format!("q=tag%3Ax&fen={START_PARAM}&stream=tab");
        for (query, qualifier) in [
            ("q=tag%3Ax", "tag"),
            ("q=-is%3Achapter", "is"),
            ("q=no%3Atag", "no"),
            ("q=created%3A2026", "created"),
            ("q=UPDATED%3A%3E1", "updated"),
            ("q=morphy+has%3Aeco+sort%3Adate", "has"),
            (with_a_position.as_str(), "tag"),
        ] {
            let refusal_body = format!(
                r#"{{"error":{{"code":"unsupported_qualifier","message":"ChessBase databases do not have this qualifier","qualifier":"{qualifier}"}}}}"#
            );
            assert_eq!(refusal(query), (400, refusal_body), "{query}");
        }
        assert_eq!(refusal("q=tag%3Ax&stream=bad!"), bad("stream", stream));
    }

    #[test]
    fn a_list_takes_its_parameters() {
        let req = Request::get(&format!(
            "/?offset=5&limit=500&sort=white-desc&line=60&fen={START_PARAM}&variant=standard&stream=Tab-1_x&q=player%3Amorphy"
        ));
        let Ok(query) = GamesQuery::parse(&req) else { panic!("refused") };
        assert_eq!(
            (query.offset, query.limit, query.sort, query.line, query.stream, query.q),
            (5, 500, Sort::parse("white-desc"), Some(60), Some("Tab-1_x"), Some("player:morphy"))
        );
        assert_eq!(query.board.map(|board| board.fen()).as_deref(), Some(START));
        // The defaults; `variant` alone narrows nothing.
        let req = Request::get("/?variant=chess960");
        let Ok(query) = GamesQuery::parse(&req) else { panic!("refused") };
        assert_eq!(
            (query.offset, query.limit, query.sort, query.line, query.stream, query.q),
            (0, DEFAULT_LIMIT, None, None, None, None)
        );
        assert!(query.board.is_none());
    }

    /// A list refused for any of its parameters, its `fen`, its `stream` or
    /// its `q` among them, does not mark its database in use; a list of a
    /// position that passes every check does, before the database is opened
    /// (#173).
    #[test]
    fn a_refused_list_marks_nothing_in_use() {
        let path = std::env::temp_dir().join(format!("bridge-api-in-use-{}", std::process::id())).join("Absent.2cbh");
        let policy = Policy { port: 0, origins: Vec::new(), token: String::new() };
        let app = App::new("test", policy, Catalog::new([path.clone()]));
        let id = crate::catalog::id_of(&path);
        let list = |query: &str| route(&app, &Request::get(&format!("/v1/databases/{id}/games?{query}")));
        for query in [
            "fen=nonsense".to_string(),
            format!("fen={START_PARAM}&stream=bad!"),
            format!("fen={START_PARAM}&limit=0"),
            format!("fen={START_PARAM}&variant=chess960"),
            format!("fen={START_PARAM}&q=tag%3Ax"),
        ] {
            let answer = list(&query);
            assert!(matches!(answer.status, 400 | 422), "{query}: {}", answer.body);
            assert!(!app.catalog.explorer.in_use(&id), "{query} marked the database in use");
        }
        let answer = list(&format!("fen={START_PARAM}&stream=tab&q=white%3Amorphy+sort%3Adate"));
        let missing =
            r#"{"error":{"code":"database_unavailable","message":"The database is not ready","state":"missing"}}"#;
        assert_eq!((answer.status, answer.body.as_str()), (409, missing));
        assert!(app.catalog.explorer.in_use(&id));
    }

    /// Suggestions and a game refuse their parameters as they always did.
    #[test]
    fn suggestions_and_a_game_refuse_each_parameter() {
        let suggest = |query: &str| refused(SuggestQuery::parse(&Request::get(&format!("/?{query}"))));
        for (query, parameter, message) in [
            ("prefix=m", "field", "field must be player, event or annotator"),
            ("field=colour&prefix=m", "field", "field must be player, event or annotator"),
            ("field=player", "prefix", "prefix must not be empty"),
            ("field=player&prefix=+", "prefix", "prefix must not be empty"),
            ("field=event&prefix=p&limit=0", "limit", "limit must be between 1 and 20"),
            ("field=event&prefix=p&limit=21", "limit", "limit must be between 1 and 20"),
        ] {
            assert_eq!(suggest(query), bad(parameter, message), "{query}");
        }
        let req = Request::get("/?field=annotator&prefix=Kas&limit=3");
        let Ok(query) = SuggestQuery::parse(&req) else { panic!("refused") };
        assert_eq!(
            (query.field, query.field_name, query.prefix, query.limit),
            (SuggestField::Annotator, "annotator", "Kas", 3)
        );

        let game = |number: &str, query: &str| refused(GameQuery::parse(number, &Request::get(&format!("/?{query}"))));
        let not_found = r#"{"error":{"code":"not_found","message":"No such resource"}}"#;
        for number in ["0", "-1", "x", "4294967296"] {
            assert_eq!(game(number, ""), (404, not_found.to_string()), "{number}");
        }
        assert_eq!(game("1", "annotations=all"), bad("annotations", "annotations must be reading or full"));
        for (query, full) in [("", false), ("annotations=reading", false), ("annotations=full", true)] {
            let Ok(parsed) = GameQuery::parse("7", &Request::get(&format!("/?{query}"))) else { panic!("{query}") };
            assert_eq!((parsed.number, parsed.options.full), (7, full), "{query}");
        }
    }

    /// An analysis and a warm-up refuse their parameters as they always did:
    /// each whole number before the bounds [`Search`] checks.
    #[test]
    fn the_engine_parameters_are_refused_as_before() {
        let analysis = |query: &str| refused(AnalyzeQuery::parse(&Request::get(&format!("/?{query}"))));
        let stream = "stream is at most 64 letters, digits, - and _";
        let long = format!("stream={}", "a".repeat(65));
        for (query, parameter, message) in [
            ("multipv=x", "multipv", "multipv is a whole number"),
            ("multipv=6", "multipv", "multipv is 1 to 5"),
            ("depth=x&movetime=y", "depth", "depth is a whole number"),
            ("movetime=-1", "movetime", "movetime is a whole number"),
            ("depth=5&movetime=100", "movetime", "Give depth or movetime, not both"),
            ("depth=0", "depth", "depth is 1 to 99"),
            ("stream=a%20b", "stream", stream),
            (long.as_str(), "stream", stream),
            ("stream=a%20b&multipv=x", "multipv", "multipv is a whole number"),
            ("threads=x&hash=y", "threads", "threads is a whole number"),
            ("hash=x", "hash", "hash is a whole number"),
            ("multipv=9&threads=x", "threads", "threads is a whole number"),
        ] {
            assert_eq!(analysis(query), bad(parameter, message), "{query}");
        }
        let too_many = format!("threads is 1 to {}", engine::limits().max_threads);
        assert_eq!(analysis("threads=0"), bad("threads", &too_many));
        let warm_up = |query: &str| refused(WarmQuery::parse(&Request::get(&format!("/?{query}"))));
        assert_eq!(warm_up("threads=x"), bad("threads", "threads is a whole number"));
        assert_eq!(warm_up("threads=0&hash=x"), bad("hash", "hash is a whole number"));
        assert_eq!(warm_up("threads=0"), bad("threads", &too_many));

        let Ok(query) = AnalyzeQuery::parse(&Request::get("/?moves=e2e4+e7e5&multipv=2&depth=6&threads=1&hash=16"))
        else {
            panic!("refused")
        };
        let search = &query.search;
        assert_eq!(
            (search.multipv, search.limit, search.threads, search.hash_mb, query.stream.as_str()),
            (2, Limit::Depth(6), Some(1), Some(16), "")
        );
        let longest = "a".repeat(64);
        for (query, name) in [("stream=".to_string(), ""), (format!("stream={longest}"), longest.as_str())] {
            let Ok(parsed) = AnalyzeQuery::parse(&Request::get(&format!("/?{query}"))) else { panic!("{query}") };
            assert_eq!((parsed.search.limit, parsed.stream.as_str()), (Limit::Infinite, name), "{query}");
        }
        let Ok(query) = WarmQuery::parse(&Request::get("/?hash=16")) else { panic!("refused") };
        assert_eq!((query.search.threads, query.search.hash_mb), (None, Some(16)));
    }

    /// A `500 internal` is logged by its database's id, and neither the log
    /// nor the answer carries the path its error does (#117, #173).
    #[test]
    fn an_internal_error_is_logged_by_the_database_id_alone() {
        let held = crate::log::testing::hold();
        let dir = std::env::temp_dir().join(format!("bridge-api-internal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::log::open(&dir);
        let db = dir.join("Jane Doe").join("Private Games.2cbg");
        assert!(db.is_absolute());
        let id = "0123456789abcdef";
        let r = internal(id, &Error::Io(db.clone(), std::io::Error::other("the disk is gone")));
        assert_eq!(r.status, 500);
        assert_eq!(r.body, r#"{"error":{"code":"internal","message":".2cbg: the disk is gone"}}"#);
        // Other tests may log beside this one.
        let log = std::fs::read_to_string(dir.join(crate::log::FILE_NAME)).unwrap();
        let line = log.lines().find(|l| l.contains(id)).unwrap();
        assert!(line.ends_with(" internal error on database 0123456789abcdef: .2cbg: the disk is gone"), "{line}");
        for private in [dir.to_str().unwrap(), "Jane Doe", "Private Games"] {
            assert!(!log.contains(private), "{private} in {log}");
            assert!(!r.body.contains(private), "{private} in {}", r.body);
        }
        drop(held);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
