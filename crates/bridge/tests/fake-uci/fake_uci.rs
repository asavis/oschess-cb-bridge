//! A scripted UCI engine for the bridge's engine tests (#52). It answers the
//! handshake and searches by writing made-up lines every 20 ms: one per
//! MultiPV line, a depth deeper each time.
//!
//! The moves of a position steer it: with `h2h3` among them it exits when it
//! is told to search; with `a2a3` it ignores `stop`; with `b2b3` it writes one
//! line and then nothing until it is stopped; with `d2d3` it exits with 43
//! when it is told to stop; with `f2f3` it exits with 42 a moment after its
//! best move, while it waits for the next search. Started under a name containing
//! `chatty`, it answers `uci` with `id name` lines, never `uciok`, for as
//! long as they can be written.
//! Started under a name containing `silent`, it answers `uci` with nothing,
//! and waits until its input ends.
//!
//! Its `nps` tells what it was set to (#58): `Threads` × 10⁹ + `Hash` × 10³ +
//! how many `Threads` and `Hash` options it has been sent. Its `seldepth` is
//! the depth + 2, + 100 for each `ucinewgame` it has been sent (#336).
//!
//! A search ends at its `depth`, `movetime`, `nodes` (1000 a depth) or `mate`
//! (the first line then scores that mate, found at that depth), whichever
//! comes first; with `c2c3` among the moves there is no mate to find, and a
//! mate search goes on like an unlimited one. With `searchmoves`, line k
//! starts with the k-th of them.

use std::io::{self, BufRead, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn main() {
    let (tx, commands) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut out = io::stdout().lock();
    // Whether the line was written: it is not once the bridge is gone.
    let mut say = |text: &str| writeln!(out, "{text}").and_then(|()| out.flush()).is_ok();
    let (mut multipv, mut position) = (1u32, String::new());
    let (mut threads, mut hash, mut resources_set) = (1u64, 16u64, 0u64);
    let mut newgames = 0u32;
    let mut search: Option<Search> = None;
    loop {
        let wait = if search.is_some() { Duration::from_millis(20) } else { Duration::from_secs(3600) };
        match commands.recv_timeout(wait) {
            Ok(command) => {
                let words: Vec<&str> = command.split_whitespace().collect();
                match words.as_slice() {
                    ["uci"] => {
                        if std::env::args().next().is_some_and(|name| name.contains("silent")) {
                            continue;
                        }
                        if std::env::args().next().is_some_and(|name| name.contains("chatty")) {
                            while say(&format!("id name {}Engine", " ".repeat(60_000))) {}
                            return;
                        }
                        say("id name Fake UCI 1.0");
                        say("id author the bridge's tests");
                        say("option name MultiPV type spin default 1 min 1 max 500");
                        say("uciok");
                    }
                    ["isready"] => {
                        say("readyok");
                    }
                    ["setoption", "name", "MultiPV", "value", n] => multipv = n.parse().unwrap_or(1),
                    ["setoption", "name", "Threads", "value", n] => {
                        threads = n.parse().unwrap_or(0);
                        resources_set += 1;
                    }
                    ["setoption", "name", "Hash", "value", n] => {
                        hash = n.parse().unwrap_or(0);
                        resources_set += 1;
                    }
                    ["setoption", ..] => {}
                    ["ucinewgame"] => newgames += 1,
                    ["position", ..] => position = command.clone(),
                    ["go", rest @ ..] => {
                        if position.contains(" h2h3") {
                            std::process::exit(3);
                        }
                        say("info string searching");
                        search = Some(Search::of(rest));
                    }
                    ["stop"] if position.contains(" d2d3") => std::process::exit(43),
                    ["stop"] if !position.contains(" a2a3") => {
                        if search.take().is_some() {
                            say("bestmove e2e4 ponder e7e5");
                        }
                    }
                    ["quit"] => return,
                    _ => {}
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        if let Some(s) = search.as_mut() {
            if position.contains(" b2b3") && s.depth >= 1 {
                continue;
            }
            s.depth += 1;
            let depth = s.depth;
            let nodes = u64::from(depth) * 1000;
            let found = s.mate.filter(|&m| depth >= m && !position.contains(" c2c3"));
            for k in 1..=multipv {
                let first =
                    s.searchmoves.get((k as usize - 1) % s.searchmoves.len().max(1)).map_or("e2e4", String::as_str);
                let score = match found {
                    Some(m) if k == 1 => format!("mate {m}"),
                    _ => format!("cp {}", 10 * k),
                };
                say(&format!(
                    "info depth {depth} seldepth {} multipv {k} score {score} nodes {} nps {} time {} pv {first} e7e5",
                    depth + 2 + 100 * newgames,
                    s.nodes.map_or(nodes, |n| nodes.min(n)),
                    threads * 1_000_000_000 + hash * 1_000 + resources_set,
                    depth * 20,
                ));
            }
            let done = s.depth_limit.is_some_and(|l| depth >= l)
                || s.until.is_some_and(|u| Instant::now() >= u)
                || s.nodes.is_some_and(|n| nodes >= n)
                || found.is_some();
            if done {
                let best = s.searchmoves.first().cloned().unwrap_or_else(|| "e2e4".into());
                search = None;
                say(&format!("bestmove {best} ponder e7e5"));
                if position.contains(" f2f3") {
                    std::thread::sleep(Duration::from_millis(50));
                    std::process::exit(42);
                }
            }
        }
    }
}

/// A search the engine was told to run, and how deep it is.
struct Search {
    depth: u32,
    depth_limit: Option<u32>,
    until: Option<Instant>,
    nodes: Option<u64>,
    mate: Option<u32>,
    searchmoves: Vec<String>,
}

impl Search {
    /// The search of the words after `go`.
    fn of(words: &[&str]) -> Search {
        let mut s =
            Search { depth: 0, depth_limit: None, until: None, nodes: None, mate: None, searchmoves: Vec::new() };
        let mut words = words.iter();
        let value = |words: &mut std::slice::Iter<&str>| words.next().and_then(|v| v.parse::<u64>().ok());
        while let Some(&word) = words.next() {
            match word {
                "depth" => s.depth_limit = value(&mut words).map(|d| d as u32),
                "movetime" => s.until = value(&mut words).map(|t| Instant::now() + Duration::from_millis(t)),
                "nodes" => s.nodes = value(&mut words),
                "mate" => s.mate = value(&mut words).map(|m| m as u32),
                "searchmoves" => s.searchmoves = words.by_ref().map(|m| m.to_string()).collect(),
                _ => {}
            }
        }
        s
    }
}
