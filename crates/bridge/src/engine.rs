//! The UCI engine the analysis board uses through the bridge (#13, #52).
//!
//! One process runs the engine named in `bridge.toml`. It starts with the
//! first analysis, and ends after [`IDLE`] without one, when it crashes, or
//! when the bridge stops. It searches one position at a time: a newer analysis
//! takes it from the running one, which stops. From the browser only the
//! position, the number of lines and an optional depth or time reach the
//! engine, each checked here; `Threads` and `Hash` come from the configuration.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use chesscore::{Board, Move, Piece, Square};

use crate::http::Sink;
use crate::json::{self, Obj};

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
/// The hash table when the configuration names none, before the memory cap.
pub const DEFAULT_HASH_MB: u32 = 512;
/// The smallest hash table an analysis may ask for.
pub const MIN_HASH_MB: u32 = 16;
/// The largest hash table an analysis may ask for, on any computer.
pub const MAX_HASH_MB: u32 = 32_768;

/// How long the engine may take to answer `uci` with `uciok`.
const HANDSHAKE: Duration = Duration::from_secs(5);
/// How long it may take to answer `isready`; the first allocates the hash.
const READY: Duration = Duration::from_secs(30);
/// How long a stopped search may take to name its best move.
const STOP_GRACE: Duration = Duration::from_secs(2);
/// How long an ending process may take to exit after `quit`.
const QUIT_GRACE: Duration = Duration::from_millis(500);
/// Lines of a search are written at most this often.
const PACE: Duration = Duration::from_millis(250);
/// With nothing new for this long, the last lines are written again, which
/// keeps the connection alive through a long step of the search.
const KEEP_ALIVE: Duration = Duration::from_secs(2);
/// How often a search looks at its client and at newer analyses.
const POLL: Duration = Duration::from_millis(50);
/// The longest line the engine may write; longer ones are dropped whole.
const MAX_LINE: usize = 64 << 10;
/// Lines of the engine waiting to be read; past them the engine waits on its
/// own output, so a flood of output holds the engine rather than memory.
const QUEUED_LINES: usize = 256;

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
        let threads = threads.unwrap_or_else(default_threads).clamp(1, limits.max_threads);
        let hash_mb = hash_mb.unwrap_or_else(default_hash_mb).clamp(1, limits.max_hash_mb);
        EngineConfig { program, threads, hash_mb }
    }
}

fn default_threads() -> u32 {
    let n = std::thread::available_parallelism().map_or(1, |n| n.get());
    u32::try_from(n.saturating_sub(2)).unwrap_or(u32::MAX).max(1)
}

fn default_hash_mb() -> u32 {
    match physical_memory_mb() {
        Some(total) => DEFAULT_HASH_MB.min(u32::try_from(total / 4).unwrap_or(u32::MAX)).max(16),
        None => DEFAULT_HASH_MB,
    }
}

#[cfg(windows)]
fn physical_memory_mb() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..unsafe { std::mem::zeroed() } };
    // SAFETY: `status` is a valid MEMORYSTATUSEX with its length set, as the call requires.
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status.ullTotalPhys >> 20)
}

#[cfg(not(windows))]
fn physical_memory_mb() -> Option<u64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 =
        info.lines().find_map(|l| l.strip_prefix("MemTotal:"))?.trim().strip_suffix("kB")?.trim().parse().ok()?;
    Some(kb >> 10)
}

/// What an analysis on this computer may ask the engine for (#58).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_threads: u32,
    pub max_hash_mb: u32,
}

/// This computer's [`Limits`]: its logical processors, and the largest power
/// of two at or below half its physical memory, from [`MIN_HASH_MB`] to
/// [`MAX_HASH_MB`].
pub fn limits() -> Limits {
    static LIMITS: OnceLock<Limits> = OnceLock::new();
    *LIMITS.get_or_init(|| {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        Limits {
            max_threads: u32::try_from(threads).unwrap_or(u32::MAX).max(1),
            max_hash_mb: max_hash_mb(physical_memory_mb()),
        }
    })
}

fn max_hash_mb(total_mb: Option<u64>) -> u32 {
    let half = total_mb.map_or(u64::from(DEFAULT_HASH_MB), |t| t / 2);
    let half = half.clamp(u64::from(MIN_HASH_MB), u64::from(MAX_HASH_MB));
    1 << (u64::BITS - 1 - half.leading_zeros())
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
        Ok(Search { position, multipv, limit, threads: None, hash_mb: None })
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
        match self.limit {
            Limit::Infinite => "go infinite".into(),
            Limit::Depth(d) => format!("go depth {d}"),
            Limit::MovetimeMs(t) => format!("go movetime {t}"),
        }
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
    /// The configuration file the engine follows; `None` for a fixed one.
    file: Option<PathBuf>,
    current: Mutex<Current>,
}

#[derive(Default)]
struct Current {
    /// The configuration file's signature when it was last read.
    signature: Option<u64>,
    config: Option<EngineConfig>,
    inner: Option<Arc<Inner>>,
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

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
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
        let current = Current { signature: None, config, inner };
        Engine { shared: Arc::new(Shared { idle, file: None, current: Mutex::new(current) }) }
    }

    /// The engine the `bridge.toml` at `path` names, read again whenever the
    /// file changes. A file that cannot be read or parsed keeps the engine it
    /// named before. The file is looked at every [`CONFIG_POLL`] as well, so a
    /// changed engine stops the running search at once, not at the next call.
    pub fn from_config_file(path: PathBuf) -> Self {
        Self::following(path, CONFIG_POLL)
    }

    /// [`Engine::from_config_file`] looking at the file every `poll` (tests).
    pub fn following(path: PathBuf, poll: Duration) -> Self {
        let engine = Engine { shared: Arc::new(Shared { idle: IDLE, file: Some(path), current: Mutex::default() }) };
        engine.current();
        let watched = Arc::downgrade(&engine.shared);
        let _ = std::thread::Builder::new().name("bridge-engine-config".into()).stack_size(crate::THREAD_STACK).spawn(
            move || {
                while let Some(shared) = watched.upgrade() {
                    Engine { shared }.current();
                    std::thread::sleep(poll);
                }
            },
        );
        engine
    }

    /// The engine now, after reading the configuration again if it changed.
    /// A replaced engine's running search stops, and its process ends when
    /// that search lets it go.
    fn current(&self) -> Option<Arc<Inner>> {
        let mut current = lock(&self.shared.current);
        if let Some(path) = &self.shared.file {
            let signature = crate::sources::signature(Some(path));
            if current.signature != Some(signature) {
                // Only a regular file is read: a pipe would block every caller
                // on this lock. A failed read is tried again next time, even
                // when the file itself did not change, as access may return.
                let read = if path.is_file() {
                    std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|t| crate::config::parse(&t))
                } else {
                    Err("not a regular file".to_string())
                };
                if let Ok(config) = read {
                    current.signature = Some(signature);
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
        }
        current.inner.clone()
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

    /// Whether the engine's process is running (tests).
    pub fn is_running(&self) -> bool {
        self.current().is_some_and(|i| i.slot.try_lock().map(|s| s.is_some()).unwrap_or(true))
    }

    /// Runs `search` and writes its lines to `sink` until it ends, the client
    /// leaves, or a newer analysis takes the engine. `stream` names the
    /// client's view: a newer analysis from another one ends this one with
    /// `{"superseded":true}`, one from the same view ends it without a line.
    pub fn analyze(&self, search: &Search, stream: &str, sink: &mut dyn Sink) {
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
        if slot.as_mut().is_some_and(|p| !p.alive()) {
            *slot = None;
        }
        if slot.is_none() {
            match Process::start(&inner.config) {
                Ok(p) => {
                    *lock(&inner.name) = Some(p.name.clone());
                    *slot = Some(p);
                }
                Err(e) => {
                    let _ = sink.line(&error_line("engine_failed", &e));
                    return;
                }
            }
        }
        let process = slot.as_mut().expect("started above");
        let outcome = inner.run(process, search, ticket, stream, sink);
        process.used = Instant::now();
        if outcome == Outcome::Lost {
            *slot = None;
        }
    }
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
        let _ = std::thread::Builder::new().name("bridge-engine-idle".into()).stack_size(crate::THREAD_STACK).spawn(
            move || {
                while let Some(inner) = reaper.upgrade() {
                    inner.end_if_idle();
                    drop(inner);
                    std::thread::sleep(every);
                }
            },
        );
        inner
    }

    fn superseded(&self, stream: &str, sink: &mut dyn Sink) {
        if *lock(&self.newest_stream) != stream {
            let _ = sink.line(r#"{"superseded":true}"#);
        }
    }

    fn end_if_idle(&self) {
        let Ok(mut slot) = self.slot.try_lock() else { return };
        if slot.as_ref().is_some_and(|p| p.used.elapsed() >= self.idle) {
            *slot = None;
        }
    }

    fn run(&self, p: &mut Process, search: &Search, ticket: u64, stream: &str, sink: &mut dyn Sink) -> Outcome {
        let exited = |sink: &mut dyn Sink| {
            let _ = sink.line(&error_line("engine_exited", "The engine stopped"));
            Outcome::Lost
        };
        let threads = search.threads.unwrap_or(self.config.threads);
        let hash_mb = search.hash_mb.unwrap_or(self.config.hash_mb);
        let mut setup = Vec::new();
        // Stockfish takes both between searches; a new Hash clears its table.
        if p.threads != threads {
            setup.push(format!("setoption name Threads value {threads}"));
        }
        if p.hash_mb != hash_mb {
            setup.push(format!("setoption name Hash value {hash_mb}"));
        }
        if p.multipv != search.multipv {
            setup.push(format!("setoption name MultiPV value {}", search.multipv));
        }
        setup.push(format!("position {}", search.position));
        setup.push("isready".into());
        if setup.iter().any(|c| p.send(c).is_err()) || p.wait_for("readyok", READY).is_none() {
            return exited(sink);
        }
        (p.threads, p.hash_mb, p.multipv) = (threads, hash_mb, search.multipv);
        if p.send(&search.go()).is_err() {
            return exited(sink);
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
                    return exited(sink);
                }
            }
            if self.turn.load(Ordering::SeqCst) != ticket {
                let outcome = p.stop();
                self.superseded(stream, sink);
                return outcome;
            }
            if sink.gone() || pacer.tick(sink).is_err() {
                return p.stop();
            }
        }
    }
}

/// Whether `program` is a UCI engine: runs its handshake within the usual
/// limits and ends it. The engine's name, or why it was refused.
pub fn probe(program: &Path) -> Result<String, String> {
    let config = EngineConfig { program: program.to_path_buf(), threads: 1, hash_mb: 16 };
    Process::start(&config).map(|p| p.name.clone())
}

fn file_stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "engine".into())
}

fn error_line(code: &str, message: &str) -> String {
    let error = Obj::new().str("code", code).str("message", message).done();
    Obj::new().raw("error", &error).done()
}

/// Starts `command`, trying again for a moment while its program is busy. On
/// Linux a program written a moment ago, such as a freshly installed engine,
/// is busy (`ETXTBSY`) while a child another thread is starting still holds
/// the file open between its fork and its exec.
fn spawn(command: &mut Command) -> io::Result<Child> {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match command.spawn() {
            Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            started => return started,
        }
    }
}

/// The engine's process, its input, and its output line by line.
struct Process {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    name: String,
    /// The `Threads`, `Hash` and `MultiPV` the engine was last set to.
    threads: u32,
    hash_mb: u32,
    multipv: u32,
    used: Instant,
}

impl Process {
    fn start(config: &EngineConfig) -> Result<Process, String> {
        let mut command = Command::new(&config.program);
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        if let Some(dir) = config.program.parent().filter(|d| !d.as_os_str().is_empty()) {
            command.current_dir(dir);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            use windows_sys::Win32::System::Threading::{BELOW_NORMAL_PRIORITY_CLASS, CREATE_NO_WINDOW};
            // Below the browser, as ChessBase starts its engines; no console window.
            command.creation_flags(BELOW_NORMAL_PRIORITY_CLASS | CREATE_NO_WINDOW);
        }
        let mut child =
            spawn(&mut command).map_err(|e| format!("The engine {} did not start: {e}", config.program.display()))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("The engine's input and output are not available".into());
        };
        let (tx, lines) = mpsc::sync_channel(QUEUED_LINES);
        let _ = std::thread::Builder::new()
            .name("bridge-engine-out".into())
            .stack_size(crate::THREAD_STACK)
            .spawn(move || forward_lines(stdout, &tx));
        let mut p = Process {
            child,
            stdin,
            lines,
            name: file_stem(&config.program),
            threads: config.threads,
            hash_mb: config.hash_mb,
            multipv: 1,
            used: Instant::now(),
        };
        if p.send("uci").is_err() {
            return Err("The engine closed its input".into());
        }
        let deadline = Instant::now() + HANDSHAKE;
        loop {
            // The deadline holds however much the engine writes: a line already
            // queued is not taken once it has passed.
            let Some(left) = until(deadline) else {
                return Err("The file is not a UCI engine: it did not answer uciok in time".into());
            };
            match p.lines.recv_timeout(left) {
                Ok(line) if line == "uciok" => break,
                Ok(line) => {
                    if let Some(name) = line.strip_prefix("id name ") {
                        p.name = name.trim().chars().take(100).collect();
                    }
                }
                Err(_) => return Err("The file is not a UCI engine: it did not answer uciok".into()),
            }
        }
        let options = [
            format!("setoption name Threads value {}", config.threads),
            format!("setoption name Hash value {}", config.hash_mb),
            "isready".into(),
        ];
        if options.iter().any(|c| p.send(c).is_err()) || p.wait_for("readyok", READY).is_none() {
            return Err("The engine did not get ready".into());
        }
        Ok(p)
    }

    fn send(&mut self, command: &str) -> io::Result<()> {
        self.stdin.write_all(command.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()
    }

    /// Reads lines until one equal to `word` or starting with it and a space.
    fn wait_for(&mut self, word: &str, within: Duration) -> Option<String> {
        let deadline = Instant::now() + within;
        loop {
            let line = self.lines.recv_timeout(until(deadline)?).ok()?;
            if line == word || line.strip_prefix(word).is_some_and(|rest| rest.starts_with(' ')) {
                return Some(line);
            }
        }
    }

    /// Stops the search and waits for its best move, which nobody reads.
    fn stop(&mut self) -> Outcome {
        if self.send("stop").is_ok() && self.wait_for("bestmove", STOP_GRACE).is_some() {
            Outcome::Kept
        } else {
            Outcome::Lost
        }
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.send("quit");
        let deadline = Instant::now() + QUIT_GRACE;
        while Instant::now() < deadline {
            if !matches!(self.child.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Sends `output` line by line until it ends or nobody receives. A line longer
/// than [`MAX_LINE`] is read past and dropped whole, never kept; at most
/// [`QUEUED_LINES`] wait, after which this waits, and with it the engine.
fn forward_lines(output: impl Read, tx: &SyncSender<String>) {
    let mut reader = BufReader::with_capacity(8 << 10, output);
    let mut line = Vec::new();
    let mut overlong = false;
    loop {
        let available = match reader.fill_buf() {
            Ok([]) => {
                // The last line may have no line end.
                if !overlong && !line.is_empty() {
                    let _ = tx.send(String::from_utf8_lossy(&line).trim_end().to_string());
                }
                return;
            }
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        let end = available.iter().position(|&b| b == b'\n');
        let part = &available[..end.unwrap_or(available.len())];
        if !overlong && line.len() + part.len() <= MAX_LINE {
            line.extend_from_slice(part);
        } else {
            overlong = true;
            line.clear();
        }
        let used = part.len() + usize::from(end.is_some());
        reader.consume(used);
        if end.is_some() {
            if !overlong && tx.send(String::from_utf8_lossy(&line).trim_end().to_string()).is_err() {
                return;
            }
            line.clear();
            overlong = false;
        }
    }
}

/// The time left before `deadline`; `None` once it has passed, so that a
/// receive with a zero timeout never takes a queued line late.
fn until(deadline: Instant) -> Option<Duration> {
    deadline.checked_duration_since(Instant::now()).filter(|left| !left.is_zero())
}

/// The best move of a `bestmove` line.
fn bestmove(line: &str) -> Option<String> {
    let mut t = line.split_whitespace();
    (t.next()? == "bestmove").then(|| t.next().filter(|m| is_uci_move(m)).unwrap_or("(none)").to_string())
}

fn is_uci_move(s: &str) -> bool {
    let b = s.as_bytes();
    matches!(b.len(), 4 | 5)
        && (b'a'..=b'h').contains(&b[0])
        && (b'1'..=b'8').contains(&b[1])
        && (b'a'..=b'h').contains(&b[2])
        && (b'1'..=b'8').contains(&b[3])
        && b.get(4).is_none_or(|p| b"qrbn".contains(p))
}

/// An `info` line with a depth and a score as `{"info": ...}`, with its line
/// number; `None` for any other line. The line of moves may be empty: in a
/// position already decided, the engine gives the score alone.
fn info_json(line: &str) -> Option<(u32, String)> {
    let mut t = line.split_whitespace().peekable();
    if t.next()? != "info" {
        return None;
    }
    let (mut depth, mut seldepth, mut multipv, mut nodes, mut nps, mut time) = (None, None, 1, None, None, None);
    let (mut score, mut bound, mut pv) = (None, None, Vec::new());
    while let Some(key) = t.next() {
        let mut num = || t.next().and_then(|v| v.parse::<i64>().ok());
        match key {
            "string" => return None,
            "depth" => depth = num(),
            "seldepth" => seldepth = num(),
            "multipv" => multipv = num().and_then(|k| u32::try_from(k).ok())?,
            "nodes" => nodes = num(),
            "nps" => nps = num(),
            "time" => time = num(),
            "score" => {
                let kind = t.next()?;
                let value: i64 = t.next()?.parse().ok()?;
                score = match kind {
                    "cp" => Some(("cp", value)),
                    "mate" => Some(("mate", value)),
                    _ => return None,
                };
                bound = match t.peek() {
                    Some(&"lowerbound") => Some("lower"),
                    Some(&"upperbound") => Some("upper"),
                    _ => None,
                };
                if bound.is_some() {
                    t.next();
                }
            }
            "pv" => {
                pv = t.by_ref().take_while(|m| is_uci_move(m)).map(json::string).collect();
                break;
            }
            _ => {}
        }
    }
    let (depth, (kind, value)) = (depth?, score?);
    if !(1..=MAX_MULTIPV).contains(&multipv) {
        return None;
    }
    let mut info = Obj::new().num("depth", depth);
    if let Some(s) = seldepth {
        info = info.num("seldepth", s);
    }
    info = info.num("multipv", multipv).raw("score", &Obj::new().num(kind, value).done());
    if let Some(b) = bound {
        info = info.str("bound", b);
    }
    for (key, value) in [("nodes", nodes), ("nps", nps), ("time", time)] {
        if let Some(v) = value {
            info = info.num(key, v);
        }
    }
    let info = info.raw("pv", &json::array(pv)).done();
    Some((multipv, Obj::new().raw("info", &info).done()))
}

/// Writes the newest line of each number at most every [`PACE`], and the last
/// ones again after [`KEEP_ALIVE`] without news.
struct Pacer {
    pending: BTreeMap<u32, String>,
    last: BTreeMap<u32, String>,
    flushed: Instant,
    written: Instant,
}

impl Pacer {
    fn new() -> Self {
        let long_ago = Instant::now().checked_sub(PACE).unwrap_or_else(Instant::now);
        Pacer { pending: BTreeMap::new(), last: BTreeMap::new(), flushed: long_ago, written: Instant::now() }
    }

    fn push(&mut self, multipv: u32, line: String) {
        self.pending.insert(multipv, line);
    }

    fn tick(&mut self, sink: &mut dyn Sink) -> io::Result<()> {
        if !self.pending.is_empty() && self.flushed.elapsed() >= PACE {
            return self.flush(sink);
        }
        if self.written.elapsed() >= KEEP_ALIVE && !self.last.is_empty() {
            for line in self.last.values() {
                sink.line(line)?;
            }
            self.written = Instant::now();
        }
        Ok(())
    }

    fn flush(&mut self, sink: &mut dyn Sink) -> io::Result<()> {
        for (k, line) in std::mem::take(&mut self.pending) {
            sink.line(&line)?;
            self.last.insert(k, line);
        }
        self.flushed = Instant::now();
        self.written = self.flushed;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_largest_hash_is_a_power_of_two_within_half_the_memory() {
        assert_eq!(max_hash_mb(Some(16 * 1024)), 8192);
        assert_eq!(max_hash_mb(Some(12 * 1024)), 4096);
        assert_eq!(max_hash_mb(Some(1024 * 1024)), MAX_HASH_MB);
        assert_eq!(max_hash_mb(Some(8)), MIN_HASH_MB);
        assert_eq!(max_hash_mb(None), DEFAULT_HASH_MB);
        let l = limits();
        assert!(l.max_threads >= 1 && l.max_hash_mb.is_power_of_two());
    }

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
    fn reads_info_lines() {
        let line = "info depth 20 seldepth 31 multipv 2 score cp -35 lowerbound nodes 123456 nps 999 hashfull 12 tbhits 0 time 812 pv e7e5 g1f3 b8c6";
        let (k, json) = info_json(line).unwrap();
        assert_eq!(k, 2);
        assert_eq!(
            json,
            r#"{"info":{"depth":20,"seldepth":31,"multipv":2,"score":{"cp":-35},"bound":"lower","nodes":123456,"nps":999,"time":812,"pv":["e7e5","g1f3","b8c6"]}}"#
        );
        let (_, json) = info_json("info depth 30 score mate -3 pv h7h8q a1a2").unwrap();
        assert_eq!(json, r#"{"info":{"depth":30,"multipv":1,"score":{"mate":-3},"pv":["h7h8q","a1a2"]}}"#);
        for line in [
            "info string NNUE evaluation using nn.nnue",
            "info depth 5 currmove e2e4 currmovenumber 1",
            "info depth 5 multipv 9 score cp 10 pv e2e4",
            "bestmove e2e4",
            "info depth 5 score wdl 1 2 3 pv e2e4",
        ] {
            assert_eq!(info_json(line), None, "{line}");
        }
        // A decided position: mate or stalemate on the board, with no moves.
        let (_, json) = info_json("info depth 0 score mate 0").unwrap();
        assert_eq!(json, r#"{"info":{"depth":0,"multipv":1,"score":{"mate":0},"pv":[]}}"#);
        let (_, json) = info_json("info depth 0 score cp 0").unwrap();
        assert_eq!(json, r#"{"info":{"depth":0,"multipv":1,"score":{"cp":0},"pv":[]}}"#);
        assert_eq!(bestmove("bestmove e2e4 ponder e7e5").as_deref(), Some("e2e4"));
        assert_eq!(bestmove("bestmove (none)").as_deref(), Some("(none)"));
        assert_eq!(bestmove("info depth 1"), None);
    }

    #[test]
    fn keeps_no_overlong_line_and_waits_when_the_queue_is_full() {
        let mut output = b"id name x\n".to_vec();
        output.extend(std::iter::repeat_n(b'z', 3 * MAX_LINE));
        output.extend(b"\nuciok\n");
        output.extend(std::iter::repeat_n(b'y', MAX_LINE));
        output.extend(b"\nlast");
        let (tx, rx) = mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || forward_lines(&output[..], &tx));
        // The queue holds one line; the reader waits for the rest to be taken.
        std::thread::sleep(Duration::from_millis(50));
        assert!(!reader.is_finished());
        let lines: Vec<String> = rx.iter().collect();
        reader.join().unwrap();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[..2], ["id name x", "uciok"]);
        assert_eq!(lines[2].len(), MAX_LINE);
        assert_eq!(lines[3], "last");
    }

    #[test]
    fn the_defaults_leave_room_for_the_machine() {
        let c = EngineConfig::new("sf".into(), None, None);
        assert!(c.threads >= 1 && c.hash_mb >= 1 && c.hash_mb <= DEFAULT_HASH_MB);
        let c = EngineConfig::new("sf".into(), Some(0), Some(0));
        assert_eq!((c.threads, c.hash_mb), (1, 1));
    }
}
