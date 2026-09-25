//! A scripted UCI engine for the bridge's engine tests (#52). It answers the
//! handshake and searches by writing made-up lines every 20 ms: one per
//! MultiPV line, a depth deeper each time.
//!
//! The moves of a position steer it: with `h2h3` among them it exits when it
//! is told to search; with `a2a3` it ignores `stop`; with `b2b3` it writes one
//! line and then nothing until it is stopped. Started under a name containing
//! `chatty`, it answers `uci` with eight seconds of `id name` lines before
//! `uciok`.
//!
//! Its `nps` tells what it was set to (#58): `Threads` × 1 000 000 + `Hash` ×
//! 1 000 + how many `Threads` and `Hash` options it has been sent.

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
    let mut say = |text: &str| {
        let _ = writeln!(out, "{text}");
        let _ = out.flush();
    };
    let (mut multipv, mut position) = (1u32, String::new());
    let (mut threads, mut hash, mut resources_set) = (1u64, 16u64, 0u64);
    let mut search: Option<(u32, Option<u32>, Option<Instant>)> = None;
    loop {
        let wait = if search.is_some() { Duration::from_millis(20) } else { Duration::from_secs(3600) };
        match commands.recv_timeout(wait) {
            Ok(command) => {
                let words: Vec<&str> = command.split_whitespace().collect();
                match words.as_slice() {
                    ["uci"] => {
                        if std::env::args().next().is_some_and(|name| name.contains("chatty")) {
                            let until = Instant::now() + Duration::from_secs(8);
                            while Instant::now() < until {
                                say(&format!("id name {}Engine", " ".repeat(60_000)));
                            }
                        }
                        say("id name Fake UCI 1.0");
                        say("id author the bridge's tests");
                        say("option name MultiPV type spin default 1 min 1 max 500");
                        say("uciok");
                    }
                    ["isready"] => say("readyok"),
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
                    ["position", ..] => position = command.clone(),
                    ["go", rest @ ..] => {
                        if position.contains(" h2h3") {
                            std::process::exit(3);
                        }
                        say("info string searching");
                        search = Some(match rest {
                            ["depth", d] => (0, d.parse().ok(), None),
                            ["movetime", t] => {
                                (0, None, Some(Instant::now() + Duration::from_millis(t.parse().unwrap_or(0))))
                            }
                            _ => (0, None, None),
                        });
                    }
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
        if let Some((depth, limit, until)) = search.as_mut() {
            if position.contains(" b2b3") && *depth >= 1 {
                continue;
            }
            *depth += 1;
            for k in 1..=multipv {
                say(&format!(
                    "info depth {depth} seldepth {} multipv {k} score cp {} nodes {} nps {} time {} pv e2e4 e7e5",
                    *depth + 2,
                    10 * k,
                    *depth * 1000,
                    threads * 1_000_000 + hash * 1_000 + resources_set,
                    *depth * 20,
                ));
            }
            let done = limit.is_some_and(|l| *depth >= l) || until.is_some_and(|u| Instant::now() >= u);
            if done {
                search = None;
                say("bestmove e2e4 ponder e7e5");
            }
        }
    }
}
