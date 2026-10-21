//! The UCI engine the analysis board uses through the bridge (#13, #52).
//!
//! One process runs the engine named in `bridge.toml`. It starts with the
//! first analysis, and ends after [`IDLE`] without one, when it crashes, or
//! when the bridge stops. It searches one position at a time: a newer analysis
//! takes it from the running one, which stops. From the browser only the
//! position, the number of lines, an optional depth or time, and what narrows
//! a search further (a node limit, a mate search, the moves searched and a new
//! game, #336) reach the engine, each checked here; `Threads` and `Hash` come
//! from the configuration unless the analysis names them.
//! The process and the UCI it speaks are in `engine/uci.rs`; what this
//! computer allows the engine comes from [`crate::machine`].

mod uci;

pub use crate::machine::{Limits, limits};
pub use uci::HANDSHAKE;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chesscore::{Board, Move, Piece, Square};

use crate::http::Sink;
use crate::json::{self, Obj};
use crate::machine;
use crate::sync::lock;

use uci::{NotStarted, Pacer, Process, bestmove, info_json};

/// How often an engine that follows `bridge.toml` looks at the file.
pub const CONFIG_POLL: Duration = Duration::from_secs(1);
/// How long the engine may sit unused before its process ends.
pub const IDLE: Duration = Duration::from_secs(600);
/// The most lines the browser may ask for.
pub const MAX_MULTIPV: u32 = 5;
/// The deepest search the browser may ask for.
pub const MAX_DEPTH: u32 = 99;
/// The longest search the browser may ask for, in milliseconds.
pub const MAX_MOVETIME_MS: u32 = 600_000;
/// The most moves after the position.
pub const MAX_MOVES: usize = 600;
/// The longest FEN.
pub const MAX_FEN: usize = 128;
/// The most nodes a search may be limited to.
pub const MAX_NODES: u64 = 1_000_000_000;
/// The longest mate a mate search may look for, in moves.
pub const MAX_MATE: u32 = 99;
/// The most moves a search may be restricted to; no position has more legal ones.
pub const MAX_SEARCHMOVES: usize = 256;
/// The parameters that narrow a search beyond its depth or time (#336), as
/// `/v1/status` lists them and an answer acknowledges them.
pub const NARROWING: [&str; 4] = ["nodes", "mate", "searchmoves", "newgame"];
/// The hash table when the configuration names none, before the memory cap.
pub const DEFAULT_HASH_MB: u32 = 512;
/// The smallest hash table an analysis may ask for.
pub const MIN_HASH_MB: u32 = 16;
/// The largest hash table an analysis may ask for, on any computer.
pub const MAX_HASH_MB: u32 = 32_768;

/// How often a search looks at its client and at newer analyses.
const POLL: Duration = Duration::from_millis(50);

/// The engine the configuration names, with its settings resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineConfig {
    pub program: PathBuf,
    pub threads: u32,
    pub hash_mb: u32,
}

impl EngineConfig {
    /// `program` with `threads` and `hash_mb` where given, else the defaults:
    /// all logical processors but two, and [`DEFAULT_HASH_MB`] capped at a
    /// quarter of the physical memory. Either is kept within this computer's
    /// [`limits`], so the engine starts, and an analysis naming neither runs,
    /// with the defaults `/v1/status` reports.
    pub fn new(program: PathBuf, threads: Option<u32>, hash_mb: Option<u32>) -> Self {
        let limits = limits();
        let threads = threads.unwrap_or_else(machine::default_threads).clamp(1, limits.max_threads);
        let hash_mb = hash_mb.unwrap_or_else(machine::default_hash_mb).clamp(1, limits.max_hash_mb);
        EngineConfig { program, threads, hash_mb }
    }
}

/// What an analysis searches: a checked position, how long, and the engine's
/// `Threads` and `Hash` when the client names them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Search {
    /// The position as the engine is told it: a FEN written by `chesscore`, or
    /// `startpos`, then the moves in UCI.
    position: String,
    pub multipv: u32,
    pub limit: Limit,
    pub threads: Option<u32>,
    pub hash_mb: Option<u32>,
    /// A node limit (`go nodes`), beside the depth or time.
    pub nodes: Option<u64>,
    /// A mate search in this many moves (`go mate`).
    pub mate: Option<u32>,
    /// The only moves searched (`go searchmoves`), in UCI as the engine reads it.
    pub searchmoves: Vec<String>,
    /// Whether the engine hears `ucinewgame` before the position, which clears its table.
    pub newgame: bool,
}

/// What narrows a search beyond its depth or time (#336): a node limit, a
/// mate search, the moves searched, and a new game. None of them by default.
#[derive(Clone, Copy, Debug, Default)]
pub struct Narrowing<'a> {
    pub nodes: Option<u64>,
    pub mate: Option<u32>,
    /// UCI moves separated by spaces, as `moves` is given; `None` searches
    /// every move, and a list naming none is refused.
    pub searchmoves: Option<&'a str>,
    pub newgame: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    /// Until the client leaves or a newer analysis takes the engine.
    Infinite,
    Depth(u32),
    MovetimeMs(u32),
}

impl Search {
    /// The search of `moves` (UCI, separated by spaces) played from `fen`, or
    /// from the start position without one. Every move must be legal; a FEN
    /// must describe a legal standard position. The error names the
    /// parameter and why.
    pub fn new(fen: Option<&str>, moves: &str, multipv: u32, limit: Limit) -> Result<Search, (&'static str, String)> {
        Search::narrowed(fen, moves, multipv, limit, Narrowing::default())
    }

    /// [`Search::new`] narrowed as `narrowing` asks (#336). Each move of
    /// `searchmoves` must be legal in the position after `moves`.
    pub fn narrowed(
        fen: Option<&str>,
        moves: &str,
        multipv: u32,
        limit: Limit,
        narrowing: Narrowing,
    ) -> Result<Search, (&'static str, String)> {
        if narrowing.nodes.is_some_and(|n| !(1..=MAX_NODES).contains(&n)) {
            return Err(("nodes", format!("nodes is 1 to {MAX_NODES}")));
        }
        if narrowing.mate.is_some_and(|m| !(1..=MAX_MATE).contains(&m)) {
            return Err(("mate", format!("mate is 1 to {MAX_MATE}")));
        }
        if !(1..=MAX_MULTIPV).contains(&multipv) {
            return Err(("multipv", format!("multipv is 1 to {MAX_MULTIPV}")));
        }
        match limit {
            Limit::Depth(d) if !(1..=MAX_DEPTH).contains(&d) => {
                return Err(("depth", format!("depth is 1 to {MAX_DEPTH}")));
            }
            Limit::MovetimeMs(t) if !(1..=MAX_MOVETIME_MS).contains(&t) => {
                return Err(("movetime", format!("movetime is 1 to {MAX_MOVETIME_MS} ms")));
            }
            _ => {}
        }
        let (mut board, mut position) = match fen {
            None => (Board::startpos(), String::from("startpos")),
            Some(fen) => {
                if fen.len() > MAX_FEN {
                    return Err(("fen", format!("A FEN has at most {MAX_FEN} bytes")));
                }
                let board = Board::from_fen(fen).map_err(|e| ("fen", format!("Not a legal position: {e}")))?;
                if board.is_chess960() {
                    return Err(("fen", "Chess960 positions are not analysed".to_string()));
                }
                let position = format!("fen {}", board.fen());
                (board, position)
            }
        };
        let moves: Vec<&str> = moves.split_whitespace().collect();
        if moves.len() > MAX_MOVES {
            return Err(("moves", format!("At most {MAX_MOVES} moves")));
        }
        if !moves.is_empty() {
            position.push_str(" moves");
        }
        for (i, text) in moves.iter().enumerate() {
            let shown: String = text.chars().take(12).collect();
            let illegal = |why: String| ("moves", format!("Move {} ({shown:?}): {why}", i + 1));
            let mv: Move = text.parse().map_err(|_| illegal("not a UCI move".into()))?;
            let mv = castling_as_king_takes_rook(&board, mv);
            let uci = standard_uci(&board, mv);
            board.play_checked(mv).map_err(|e| illegal(e.to_string()))?;
            position.push(' ');
            position.push_str(&uci);
        }
        let searched: Vec<&str> = narrowing.searchmoves.unwrap_or_default().split_whitespace().collect();
        // An empty restriction must not become a search of every move.
        if narrowing.searchmoves.is_some() && searched.is_empty() {
            return Err(("searchmoves", "searchmoves names at least one move".into()));
        }
        if searched.len() > MAX_SEARCHMOVES {
            return Err(("searchmoves", format!("At most {MAX_SEARCHMOVES} moves")));
        }
        let mut searchmoves = Vec::with_capacity(searched.len());
        for (i, text) in searched.iter().enumerate() {
            let shown: String = text.chars().take(12).collect();
            let illegal = |why: &str| ("searchmoves", format!("Move {} ({shown:?}): {why}", i + 1));
            let mv: Move = text.parse().map_err(|_| illegal("not a UCI move"))?;
            let mv = castling_as_king_takes_rook(&board, mv);
            if !board.is_legal(mv) {
                return Err(illegal("not legal in the position"));
            }
            searchmoves.push(standard_uci(&board, mv));
        }
        Ok(Search {
            position,
            multipv,
            limit,
            threads: None,
            hash_mb: None,
            nodes: narrowing.nodes,
            mate: narrowing.mate,
            searchmoves,
            newgame: narrowing.newgame,
        })
    }

    /// The line that acknowledges what narrows this search (#336), as
    /// `docs/api.md` «Compatibility» asks of a parameter an older bridge
    /// ignores: `{"search":{…}}` with each one applied. `None` when nothing does.
    pub fn acknowledgement(&self) -> Option<String> {
        if self.nodes.is_none() && self.mate.is_none() && self.searchmoves.is_empty() && !self.newgame {
            return None;
        }
        let mut applied = Obj::new();
        if let Some(n) = self.nodes {
            // Within MAX_NODES, so it fits.
            applied = applied.num("nodes", i64::try_from(n).unwrap_or(i64::MAX));
        }
        if let Some(m) = self.mate {
            applied = applied.num("mate", i64::from(m));
        }
        if !self.searchmoves.is_empty() {
            applied = applied.raw("searchmoves", &json::array(self.searchmoves.iter().map(|m| json::string(m))));
        }
        if self.newgame {
            applied = applied.bool("newgame", true);
        }
        Some(Obj::new().raw("search", &applied.done()).done())
    }

    /// This search with the engine's `Threads` and `Hash` as the client asks,
    /// within `limits`; one left out keeps the configured default.
    pub fn with_resources(
        mut self,
        threads: Option<u32>,
        hash_mb: Option<u32>,
        limits: Limits,
    ) -> Result<Search, (&'static str, String)> {
        if threads.is_some_and(|t| !(1..=limits.max_threads).contains(&t)) {
            return Err(("threads", format!("threads is 1 to {}", limits.max_threads)));
        }
        if hash_mb.is_some_and(|h| !(MIN_HASH_MB..=limits.max_hash_mb).contains(&h)) {
            return Err(("hash", format!("hash is {MIN_HASH_MB} to {} MB", limits.max_hash_mb)));
        }
        self.threads = threads;
        self.hash_mb = hash_mb;
        Ok(self)
    }

    fn go(&self) -> String {
        let mut go = match self.limit {
            // A node limit ends the search by itself, and a mate search when it
            // finds the mate; `infinite` would hold back even that best move.
            Limit::Infinite if self.nodes.is_some() || self.mate.is_some() => "go".to_string(),
            Limit::Infinite => "go infinite".into(),
            Limit::Depth(d) => format!("go depth {d}"),
            Limit::MovetimeMs(t) => format!("go movetime {t}"),
        };
        if let Some(n) = self.nodes {
            go.push_str(&format!(" nodes {n}"));
        }
        if let Some(m) = self.mate {
            go.push_str(&format!(" mate {m}"));
        }
        // `searchmoves` takes every word after it, so it comes last.
        if !self.searchmoves.is_empty() {
            go.push_str(" searchmoves ");
            go.push_str(&self.searchmoves.join(" "));
        }
        go
    }
}

/// `chesscore` castles by the king taking its rook; UCI for a standard
/// position moves the king two squares. Either form is accepted.
fn castling_as_king_takes_rook(board: &Board, mv: Move) -> Move {
    let king = matches!(board.piece_at(mv.from), Some((Piece::King, c)) if c == board.side_to_move());
    // A promotion suffix stays on the move, for `play_checked` to refuse.
    if king && mv.promotion.is_none() && mv.from.rank() == mv.to.rank() && mv.from.file().abs_diff(mv.to.file()) == 2 {
        let file = if mv.to.file() > mv.from.file() { 7 } else { 0 };
        return Move::new(mv.from, Square::new(file, mv.from.rank()), None);
    }
    mv
}

/// UCI with castling as the king's two-square step, as a standard engine reads it.
fn standard_uci(board: &Board, mv: Move) -> String {
    let castles =
        matches!(board.piece_at(mv.from), Some((Piece::King, c)) if board.piece_at(mv.to) == Some((Piece::Rook, c)));
    if castles {
        let file = if mv.to.file() > mv.from.file() { 6 } else { 2 };
        return Move::new(mv.from, Square::new(file, mv.from.rank()), None).to_string();
    }
    mv.to_string()
}

/// The engine, or none when the configuration names none. One built from
/// `bridge.toml` follows the file: a changed engine takes effect at the next
/// analysis, as a changed list of databases does.
#[derive(Clone)]
pub struct Engine {
    shared: Arc<Shared>,
}

struct Shared {
    idle: Duration,
    /// How long a new process may take to answer `uci` with `uciok`
    /// ([`Engine::set_handshake`]).
    handshake: Mutex<Duration>,
    /// The configuration file the engine follows, as the database list does
    /// (#175); `None` for a fixed one.
    file: Option<Arc<crate::config::Watched>>,
    current: Mutex<Current>,
    /// The analyses running now, and when the newest began (#61).
    analyses: AtomicUsize,
    began: Mutex<Option<Instant>>,
}

/// Counts an analysis while it runs.
struct Running<'a>(&'a Shared);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.analyses.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Current {
    config: Option<EngineConfig>,
    inner: Option<Arc<Inner>>,
    /// What the engine has seen of the changes of the file it follows.
    seen: crate::config::Seen,
}

struct Inner {
    config: EngineConfig,
    idle: Duration,
    /// The ticket of the newest analysis; a running one that sees a newer
    /// ticket stops.
    turn: AtomicU64,
    /// The `stream` of the newest analysis.
    newest_stream: Mutex<String>,
    /// The engine's own name, once it has said it.
    name: Mutex<Option<String>>,
    slot: Mutex<Option<Process>>,
}

impl Engine {
    pub fn none() -> Self {
        Self::fixed(None, IDLE)
    }

    pub fn new(config: EngineConfig) -> Self {
        Self::fixed(Some(config), IDLE)
    }

    /// [`Engine::new`] whose process ends after `idle` without an analysis (tests).
    pub fn with_idle(config: EngineConfig, idle: Duration) -> Self {
        Self::fixed(Some(config), idle)
    }

    fn fixed(config: Option<EngineConfig>, idle: Duration) -> Self {
        let inner = config.clone().map(|c| Inner::start(c, idle));
        let current = Current { config, inner, ..Current::default() };
        let shared = Shared {
            idle,
            handshake: Mutex::new(HANDSHAKE),
            file: None,
            current: Mutex::new(current),
            analyses: AtomicUsize::new(0),
            began: Mutex::new(None),
        };
        Engine { shared: Arc::new(shared) }
    }

    /// The engine the `bridge.toml` `file` names, read again whenever the
    /// file changes, as `config::Watched` reads it: a file that cannot be read
    /// or parsed keeps the engine it named before, and no file is the defaults,
    /// no engine. The file is looked at every [`CONFIG_POLL`] as well, so a
    /// changed engine stops the running search at once, not at the next call.
    pub fn from_config_file(file: Arc<crate::config::Watched>) -> Self {
        Self::following(file, CONFIG_POLL)
    }

    /// [`Engine::from_config_file`] looking at the file every `poll` (tests).
    pub fn following(file: Arc<crate::config::Watched>, poll: Duration) -> Self {
        let shared = Shared {
            idle: IDLE,
            handshake: Mutex::new(HANDSHAKE),
            file: Some(file),
            current: Mutex::default(),
            analyses: AtomicUsize::new(0),
            began: Mutex::new(None),
        };
        let engine = Engine { shared: Arc::new(shared) };
        engine.current();
        let watched = Arc::downgrade(&engine.shared);
        let watching = std::thread::Builder::new()
            .name("bridge-engine-config".into())
            .stack_size(crate::THREAD_STACK)
            .spawn(move || {
                while let Some(shared) = watched.upgrade() {
                    Engine { shared }.current();
                    std::thread::sleep(poll);
                }
            });
        if let Err(e) = watching {
            crate::fault!("the engine's settings are not watched: no thread: {e}");
        }
        engine
    }

    /// The engine now, after reading the configuration again if it changed.
    /// A replaced engine's running search stops, and its process ends when
    /// that search lets it go.
    fn current(&self) -> Option<Arc<Inner>> {
        let mut current = lock(&self.shared.current);
        if let Some(file) = &self.shared.file {
            let look = file.look(&mut current.seen);
            if look.changed {
                let config = look.config;
                let next = config.engine.map(|p| EngineConfig::new(p, config.engine_threads, config.engine_hash));
                if next != current.config {
                    if let Some(old) = current.inner.take() {
                        old.turn.fetch_add(1, Ordering::SeqCst);
                    }
                    current.inner = next.clone().map(|c| Inner::start(c, self.shared.idle));
                    current.config = next;
                }
            }
        }
        current.inner.clone()
    }

    /// Sets how long a process started from now on may take to answer `uci`
    /// with `uciok`: [`HANDSHAKE`] unless set. A loaded machine can take
    /// longer than that to start the tests' engine, and the tests that are
    /// not about the handshake set a longer one (#238).
    pub fn set_handshake(&self, limit: Duration) {
        *lock(&self.shared.handshake) = limit;
    }

    /// The engine's name for `/v1/status`: what it said in the handshake,
    /// else its file's name; `None` without an engine.
    pub fn name(&self) -> Option<String> {
        let inner = self.current()?;
        let said = lock(&inner.name).clone();
        Some(said.unwrap_or_else(|| file_stem(&inner.config.program)))
    }

    /// The configured `Threads` and `Hash` for `/v1/status`, which an analysis
    /// naming none uses; `None` without an engine.
    pub fn defaults(&self) -> Option<(u32, u32)> {
        self.current().map(|i| (i.config.threads, i.config.hash_mb))
    }

    pub fn is_configured(&self) -> bool {
        self.current().is_some()
    }

    /// Whether an analysis runs that began less than `within` ago (#61): a
    /// restart would end its stream. One running longer, as in a tab left
    /// open on one position, counts as none, so that it cannot hold an
    /// update back for good.
    pub fn analyzing(&self, within: Duration) -> bool {
        self.shared.analyses.load(Ordering::SeqCst) > 0
            && lock(&self.shared.began).is_some_and(|began| began.elapsed() < within)
    }

    /// Whether the engine's process is running (tests).
    pub fn is_running(&self) -> bool {
        self.current().is_some_and(|i| i.slot.try_lock().map(|s| s.is_some()).unwrap_or(true))
    }

    /// Runs `search` and writes its lines to `sink` until it ends, the client
    /// leaves, or a newer analysis takes the engine. `stream` names the
    /// client's view: a newer analysis from another one ends this one with
    /// `{"superseded":true}`, one from the same view ends it without a line.
    pub fn analyze(&self, search: &Search, stream: &str, sink: &mut dyn Sink) {
        self.shared.analyses.fetch_add(1, Ordering::SeqCst);
        *lock(&self.shared.began) = Some(Instant::now());
        let _running = Running(&self.shared);
        let Some(inner) = self.current() else {
            let _ = sink.line(&error_line("no_engine", "No engine is configured"));
            return;
        };
        // The view first: the running search reads it once it sees the newer ticket.
        *lock(&inner.newest_stream) = stream.to_string();
        let ticket = inner.turn.fetch_add(1, Ordering::SeqCst) + 1;
        // Waits while the running search stops.
        let mut slot = lock(&inner.slot);
        if inner.turn.load(Ordering::SeqCst) != ticket {
            inner.superseded(stream, sink);
            return;
        }
        let process = match inner.process(&mut slot, *lock(&self.shared.handshake)) {
            Ok(p) => p,
            Err(e) => {
                let _ = sink.line(&error_line("engine_failed", &e));
                return;
            }
        };
        let outcome = inner.run(process, search, ticket, stream, sink);
        process.used = Instant::now();
        if outcome == Outcome::Lost {
            *slot = None;
        }
    }

    /// Starts the engine's process when none runs, with the threads and hash
    /// of `search` set and the engine ready, so that an analysis asking for
    /// the same only searches (#110). An analysis running now has the engine:
    /// nothing is sent to it. A warm-up counts as use for the idle timer.
    pub fn warm(&self, search: &Search) -> Warmed {
        let Some(inner) = self.current() else { return Warmed::NoEngine };
        let Ok(mut slot) = inner.slot.try_lock() else { return Warmed::Busy };
        let p = match inner.process(&mut slot, *lock(&self.shared.handshake)) {
            Ok(p) => p,
            Err(e) => return Warmed::Failed(e),
        };
        let threads = search.threads.unwrap_or(inner.config.threads);
        let hash_mb = search.hash_mb.unwrap_or(inner.config.hash_mb);
        if !p.sync(threads, hash_mb, None).unwrap_or(false) {
            let ended = p.ended();
            crate::fault!("the engine {} did not get ready for a warm-up: {ended}", p.program);
            *slot = None;
            return Warmed::Failed("the engine did not say it is ready".into());
        }
        p.used = Instant::now();
        Warmed::Ready
    }
}

/// What a warm-up found (#110).
#[derive(Debug, PartialEq, Eq)]
pub enum Warmed {
    /// The engine's process runs, set to the threads and hash asked for, and
    /// said it is ready.
    Ready,
    /// An analysis has the engine: nothing was sent.
    Busy,
    NoEngine,
    /// The process could not start or did not say it is ready.
    Failed(String),
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The process is still good for the next search.
    Kept,
    /// The process crashed or did not stop; it is dropped.
    Lost,
}

impl Inner {
    fn start(config: EngineConfig, idle: Duration) -> Arc<Inner> {
        let inner = Arc::new(Inner {
            config,
            idle,
            turn: AtomicU64::new(0),
            newest_stream: Mutex::new(String::new()),
            name: Mutex::new(None),
            slot: Mutex::new(None),
        });
        let reaper = Arc::downgrade(&inner);
        let every = (idle / 4).clamp(Duration::from_millis(20), Duration::from_secs(30));
        let reaping = std::thread::Builder::new()
            .name("bridge-engine-idle".into())
            .stack_size(crate::THREAD_STACK)
            .spawn(move || {
                while let Some(inner) = reaper.upgrade() {
                    inner.end_if_idle();
                    drop(inner);
                    std::thread::sleep(every);
                }
            });
        if let Err(e) = reaping {
            crate::fault!("an idle engine is not ended: no thread: {e}");
        }
        inner
    }

    /// The engine's process in `slot`: the one running, else a new one, which
    /// has `handshake` to answer `uci` and whose name is kept for
    /// `/v1/status`; one that has exited is replaced. Why a new one did not
    /// start.
    fn process<'a>(&self, slot: &'a mut Option<Process>, handshake: Duration) -> Result<&'a mut Process, String> {
        if let Some(p) = slot.as_mut()
            && ended_while_idle(p)
        {
            *slot = None;
        }
        match slot {
            Some(p) => Ok(p),
            None => {
                // The chosen engine answered as one when it was chosen: not now is a failure (#342).
                let p = Process::start(&self.config, handshake).map_err(|NotStarted { why, reported }| {
                    if !reported {
                        crate::fault!(
                            "the engine {} did not start as an engine: {why}",
                            file_stem(&self.config.program)
                        );
                    }
                    why
                })?;
                *lock(&self.name) = Some(p.name.clone());
                Ok(slot.insert(p))
            }
        }
    }

    fn superseded(&self, stream: &str, sink: &mut dyn Sink) {
        if *lock(&self.newest_stream) != stream {
            let _ = sink.line(r#"{"superseded":true}"#);
        }
    }

    fn end_if_idle(&self) {
        let Ok(mut slot) = self.slot.try_lock() else { return };
        if let Some(p) = slot.as_mut()
            && p.used.elapsed() >= self.idle
        {
            // An engine that ended on its own is reported before it is let go.
            ended_while_idle(p);
            *slot = None;
        }
    }

    fn run(&self, p: &mut Process, search: &Search, ticket: u64, stream: &str, sink: &mut dyn Sink) -> Outcome {
        let exited = |p: &mut Process, sink: &mut dyn Sink, when: &str| {
            let ended = p.ended();
            crate::fault!("the engine {} stopped {when}: {ended}", p.program);
            let _ = sink.line(&error_line("engine_exited", "The engine stopped"));
            Outcome::Lost
        };
        let threads = search.threads.unwrap_or(self.config.threads);
        let hash_mb = search.hash_mb.unwrap_or(self.config.hash_mb);
        if !p.sync(threads, hash_mb, Some(search)).unwrap_or(false) {
            return exited(p, sink, "before an analysis, not ready");
        }
        if let Some(acknowledgement) = search.acknowledgement() {
            let _ = sink.line(&acknowledgement);
        }
        if p.send(&search.go()).is_err() {
            return exited(p, sink, "as an analysis began");
        }
        let mut pacer = Pacer::new();
        loop {
            match p.lines.recv_timeout(POLL) {
                Ok(line) => {
                    if let Some((k, info)) = info_json(&line) {
                        pacer.push(k, info);
                    } else if let Some(best) = bestmove(&line) {
                        let _ = pacer.flush(sink);
                        let _ = sink.line(&Obj::new().str("bestmove", &best).done());
                        return Outcome::Kept;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = pacer.flush(sink);
                    return exited(p, sink, "during an analysis");
                }
            }
            if self.turn.load(Ordering::SeqCst) != ticket {
                let outcome = stop(p, "for a newer analysis");
                self.superseded(stream, sink);
                return outcome;
            }
            if sink.gone() || pacer.tick(sink).is_err() {
                return stop(p, "when its client left");
            }
        }
    }
}

/// Whether `p`'s process ended on its own while it waited for an analysis,
/// which is reported with how it ended (#342).
fn ended_while_idle(p: &mut Process) -> bool {
    if p.alive() {
        return false;
    }
    let ended = p.ended();
    crate::fault!("the engine {} had stopped between analyses: {ended}", p.program);
    true
}

/// Stops `p`'s search `when` it is no longer wanted. An engine that does not
/// stop as asked is let go, and reported with how it ended (#342).
fn stop(p: &mut Process, when: &str) -> Outcome {
    let outcome = p.stop();
    if outcome == Outcome::Lost {
        let ended = p.ended();
        crate::fault!("the engine {} did not stop {when}: {ended}", p.program);
    }
    outcome
}

/// Whether `program` is a UCI engine: runs its handshake within the usual
/// limits and ends it. The engine's name, or why it was refused.
pub fn probe(program: &Path) -> Result<String, String> {
    probe_within(program, HANDSHAKE)
}

/// [`probe`] with `handshake` in place of [`HANDSHAKE`] (tests).
pub fn probe_within(program: &Path, handshake: Duration) -> Result<String, String> {
    let config = EngineConfig { program: program.to_path_buf(), threads: 1, hash_mb: 16 };
    Process::start(&config, handshake).map(|p| p.name.clone()).map_err(|e| e.why)
}

fn file_stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "engine".into())
}

fn error_line(code: &str, message: &str) -> String {
    let error = Obj::new().str("code", code).str("message", message).done();
    Obj::new().raw("error", &error).done()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_threads_and_hash_are_kept_within_the_limits() {
        let l = limits();
        let c = EngineConfig::new("sf".into(), Some(u32::MAX), Some(u32::MAX));
        assert_eq!((c.threads, c.hash_mb), (l.max_threads, l.max_hash_mb));
        let c = EngineConfig::new("sf".into(), None, None);
        assert!(c.threads <= l.max_threads && c.hash_mb <= l.max_hash_mb);
    }

    #[test]
    fn a_search_takes_threads_and_hash_within_the_limits() {
        let limits = Limits { max_threads: 8, max_hash_mb: 1024 };
        let s = || Search::new(None, "", 1, Limit::Depth(1)).unwrap();
        let ok = s().with_resources(Some(8), Some(1024), limits).unwrap();
        assert_eq!((ok.threads, ok.hash_mb), (Some(8), Some(1024)));
        assert_eq!(s().with_resources(None, None, limits).unwrap(), s());
        assert_eq!(s().with_resources(Some(9), None, limits).unwrap_err().0, "threads");
        assert_eq!(s().with_resources(Some(0), None, limits).unwrap_err().0, "threads");
        assert_eq!(s().with_resources(None, Some(2048), limits).unwrap_err().0, "hash");
        assert_eq!(s().with_resources(None, Some(8), limits).unwrap_err().0, "hash");
    }

    #[test]
    fn checks_the_position_and_writes_it_for_the_engine() {
        let s = Search::new(None, "e2e4 e7e5 g1f3 b8c6 f1c4 g8f6 e1g1", 2, Limit::Infinite).unwrap();
        assert_eq!(s.position, "startpos moves e2e4 e7e5 g1f3 b8c6 f1c4 g8f6 e1g1");
        // The king taking its rook is the same castling.
        let s = Search::new(None, "e2e4 e7e5 g1f3 b8c6 f1c4 g8f6 e1h1", 1, Limit::Infinite).unwrap();
        assert!(s.position.ends_with("e1g1"));
        let fen = "r3k2r/8/8/8/8/8/8/R3K2R b KQkq - 0 1";
        let s = Search::new(Some(fen), "e8c8", 1, Limit::Depth(10)).unwrap();
        assert_eq!(s.position, format!("fen {fen} moves e8c8"));
        assert_eq!(s.go(), "go depth 10");
        assert_eq!(Search::new(None, "", 1, Limit::MovetimeMs(500)).unwrap().go(), "go movetime 500");
    }

    #[test]
    fn refuses_what_the_engine_must_not_see() {
        let refused =
            |fen: Option<&str>, moves: &str, multipv, limit| Search::new(fen, moves, multipv, limit).unwrap_err().0;
        assert_eq!(refused(None, "e2e5", 1, Limit::Infinite), "moves");
        // A king never promotes, castling or not.
        let castles = Some("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1");
        for mv in ["e1g1q", "e1c1n", "e1h1q"] {
            assert_eq!(refused(castles, mv, 1, Limit::Infinite), "moves", "{mv}");
        }
        assert_eq!(refused(None, "e2e4\nquit", 1, Limit::Infinite), "moves");
        assert_eq!(refused(None, "e2e4 quit", 1, Limit::Infinite), "moves");
        assert_eq!(refused(Some("8/8/8/8/8/8/8/8 w - - 0 1"), "", 1, Limit::Infinite), "fen");
        assert_eq!(
            refused(Some("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1\nquit"), "", 1, Limit::Infinite),
            "fen"
        );
        assert_eq!(refused(Some(&"x".repeat(MAX_FEN + 1)), "", 1, Limit::Infinite), "fen");
        assert_eq!(refused(None, &"g1f3 g8f6 f3g1 f6g8 ".repeat(151), 1, Limit::Infinite), "moves");
        assert_eq!(refused(None, "", 0, Limit::Infinite), "multipv");
        assert_eq!(refused(None, "", MAX_MULTIPV + 1, Limit::Infinite), "multipv");
        assert_eq!(refused(None, "", 1, Limit::Depth(0)), "depth");
        assert_eq!(refused(None, "", 1, Limit::Depth(MAX_DEPTH + 1)), "depth");
        assert_eq!(refused(None, "", 1, Limit::MovetimeMs(MAX_MOVETIME_MS + 1)), "movetime");
        // The side not to move is in check.
        assert_eq!(refused(Some("4k3/8/8/8/8/8/8/4R1K1 w - - 0 1"), "", 1, Limit::Infinite), "fen");
    }

    #[test]
    fn a_narrowed_search_writes_its_limits_and_acknowledges_them() {
        let narrowed = |fen: Option<&str>, moves: &str, limit, narrowing| {
            Search::narrowed(fen, moves, 2, limit, narrowing).unwrap()
        };
        let plain = narrowed(None, "", Limit::Depth(20), Narrowing::default());
        assert_eq!((plain.go(), plain.acknowledgement()), ("go depth 20".into(), None));
        let s = narrowed(
            None,
            "e2e4",
            Limit::Depth(20),
            Narrowing { nodes: Some(25_000_000), searchmoves: Some("e7e5 c7c5"), ..Narrowing::default() },
        );
        assert_eq!(s.go(), "go depth 20 nodes 25000000 searchmoves e7e5 c7c5");
        assert_eq!(s.acknowledgement().unwrap(), r#"{"search":{"nodes":25000000,"searchmoves":["e7e5","c7c5"]}}"#);
        // A node limit or a mate search alone ends the search: no `infinite`.
        let s = narrowed(None, "", Limit::Infinite, Narrowing { nodes: Some(1), ..Narrowing::default() });
        assert_eq!(s.go(), "go nodes 1");
        let fen = "6k1/5ppp/8/8/8/8/5PPP/R5K1 w - - 0 1";
        let s = narrowed(
            Some(fen),
            "",
            Limit::Infinite,
            Narrowing { mate: Some(1), nodes: Some(MAX_NODES), searchmoves: Some("a1a8"), newgame: true },
        );
        assert_eq!(s.go(), format!("go nodes {MAX_NODES} mate 1 searchmoves a1a8"));
        assert_eq!(
            s.acknowledgement().unwrap(),
            format!(r#"{{"search":{{"nodes":{MAX_NODES},"mate":1,"searchmoves":["a1a8"],"newgame":true}}}}"#)
        );
        let s = narrowed(None, "", Limit::MovetimeMs(500), Narrowing { mate: Some(MAX_MATE), ..Narrowing::default() });
        assert_eq!(s.go(), format!("go movetime 500 mate {MAX_MATE}"));
        let s = narrowed(None, "", Limit::Infinite, Narrowing { newgame: true, ..Narrowing::default() });
        assert_eq!(
            (s.go(), s.acknowledgement().unwrap()),
            ("go infinite".into(), r#"{"search":{"newgame":true}}"#.into())
        );
        // Castling is searched as the king's two-square step, either way it is named.
        let castles = Some("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1");
        for named in ["e1g1", "e1h1"] {
            let s =
                narrowed(castles, "", Limit::Depth(1), Narrowing { searchmoves: Some(named), ..Narrowing::default() });
            assert_eq!(s.searchmoves, ["e1g1"], "{named}");
        }
    }

    #[test]
    fn refuses_a_narrowing_out_of_bounds_or_not_legal() {
        let refused =
            |moves: &str, narrowing| Search::narrowed(None, moves, 1, Limit::Infinite, narrowing).unwrap_err().0;
        let n = Narrowing::default;
        assert_eq!(refused("", Narrowing { nodes: Some(0), ..n() }), "nodes");
        assert_eq!(refused("", Narrowing { nodes: Some(MAX_NODES + 1), ..n() }), "nodes");
        assert_eq!(refused("", Narrowing { mate: Some(0), ..n() }), "mate");
        assert_eq!(refused("", Narrowing { mate: Some(MAX_MATE + 1), ..n() }), "mate");
        // Legal in the position after the moves, not before them.
        assert_eq!(refused("e2e4", Narrowing { searchmoves: Some("e2e4"), ..n() }), "searchmoves");
        assert_eq!(refused("", Narrowing { searchmoves: Some("e2e5"), ..n() }), "searchmoves");
        assert_eq!(refused("", Narrowing { searchmoves: Some("e2e4 quit"), ..n() }), "searchmoves");
        assert_eq!(refused("", Narrowing { searchmoves: Some("e2e4\nquit"), ..n() }), "searchmoves");
        for empty in ["", "   ", "\t"] {
            assert_eq!(refused("", Narrowing { searchmoves: Some(empty), ..n() }), "searchmoves", "{empty:?}");
        }
        let too_many = "g1f3 ".repeat(MAX_SEARCHMOVES + 1);
        assert_eq!(refused("", Narrowing { searchmoves: Some(&too_many), ..n() }), "searchmoves");
        let most = "g1f3 ".repeat(MAX_SEARCHMOVES);
        assert!(Search::narrowed(None, "", 1, Limit::Infinite, Narrowing { searchmoves: Some(&most), ..n() }).is_ok());
    }

    #[test]
    fn the_defaults_leave_room_for_the_machine() {
        let c = EngineConfig::new("sf".into(), None, None);
        assert!(c.threads >= 1 && c.hash_mb >= 1 && c.hash_mb <= DEFAULT_HASH_MB);
        let c = EngineConfig::new("sf".into(), Some(0), Some(0));
        assert_eq!((c.threads, c.hash_mb), (1, 1));
    }
}
