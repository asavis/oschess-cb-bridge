//! The engine's process and the UCI it speaks (#13, #52): the handshake, the
//! options an analysis sets, and the lines of a search, read within bounds
//! however much or little the engine writes.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};

use crate::http::Sink;
use crate::json::{self, Obj};

use super::{EngineConfig, MAX_MULTIPV, Outcome, Search, file_stem};

/// How long the engine may take to answer `uci` with `uciok`, unless
/// [`super::Engine::set_handshake`] sets another.
pub const HANDSHAKE: Duration = Duration::from_secs(5);
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
/// The longest line the engine may write; longer ones are dropped whole.
const MAX_LINE: usize = 64 << 10;
/// Lines of the engine waiting to be read; past them the engine waits on its
/// own output, so a flood of output holds the engine rather than memory.
const QUEUED_LINES: usize = 256;

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
pub(super) struct Process {
    child: Child,
    stdin: ChildStdin,
    pub(super) lines: Receiver<String>,
    pub(super) name: String,
    /// The `Threads`, `Hash` and `MultiPV` the engine was last set to.
    threads: u32,
    hash_mb: u32,
    multipv: u32,
    pub(super) used: Instant,
}

impl Process {
    /// Starts the engine of `config`, which may take `handshake` to answer
    /// `uci` with `uciok`, and sets it up.
    pub(super) fn start(config: &EngineConfig, handshake: Duration) -> Result<Process, String> {
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
        let deadline = Instant::now() + handshake;
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
                // The deadline passed while the engine wrote nothing more: the
                // same failure as the check above, told the same way.
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err("The file is not a UCI engine: it did not answer uciok in time".into());
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("The file is not a UCI engine: it did not answer uciok".into());
                }
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

    /// Sets the engine for `search`, or for a warm-up without one (#110):
    /// `Threads` and `Hash` where they differ from what it was last set to,
    /// and for a search its `MultiPV` where it differs and then its position;
    /// then asks whether the engine is ready. Whether it said so within
    /// [`READY`]; the values count as set only once it did. A warm-up and an
    /// analysis both set the engine here, so that the analysis finds it as
    /// the warm-up left it.
    pub(super) fn sync(&mut self, threads: u32, hash_mb: u32, search: Option<&Search>) -> io::Result<bool> {
        let multipv = search.map_or(self.multipv, |s| s.multipv);
        let mut setup = Vec::new();
        // Stockfish takes both between searches; a new Hash clears its table.
        if self.threads != threads {
            setup.push(format!("setoption name Threads value {threads}"));
        }
        if self.hash_mb != hash_mb {
            setup.push(format!("setoption name Hash value {hash_mb}"));
        }
        if self.multipv != multipv {
            setup.push(format!("setoption name MultiPV value {multipv}"));
        }
        if let Some(search) = search {
            setup.push(format!("position {}", search.position));
        }
        setup.push("isready".into());
        for command in &setup {
            self.send(command)?;
        }
        let ready = self.wait_for("readyok", READY).is_some();
        if ready {
            (self.threads, self.hash_mb, self.multipv) = (threads, hash_mb, multipv);
        }
        Ok(ready)
    }

    pub(super) fn send(&mut self, command: &str) -> io::Result<()> {
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
    pub(super) fn stop(&mut self) -> Outcome {
        if self.send("stop").is_ok() && self.wait_for("bestmove", STOP_GRACE).is_some() {
            Outcome::Kept
        } else {
            Outcome::Lost
        }
    }

    pub(super) fn alive(&mut self) -> bool {
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
pub(super) fn bestmove(line: &str) -> Option<String> {
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
pub(super) fn info_json(line: &str) -> Option<(u32, String)> {
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
pub(super) struct Pacer {
    pending: BTreeMap<u32, String>,
    last: BTreeMap<u32, String>,
    flushed: Instant,
    written: Instant,
}

impl Pacer {
    pub(super) fn new() -> Self {
        let long_ago = Instant::now().checked_sub(PACE).unwrap_or_else(Instant::now);
        Pacer { pending: BTreeMap::new(), last: BTreeMap::new(), flushed: long_ago, written: Instant::now() }
    }

    pub(super) fn push(&mut self, multipv: u32, line: String) {
        self.pending.insert(multipv, line);
    }

    pub(super) fn tick(&mut self, sink: &mut dyn Sink) -> io::Result<()> {
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

    pub(super) fn flush(&mut self, sink: &mut dyn Sink) -> io::Result<()> {
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
}
