//! `GET /v1/databases/{id}/explorer?fen=`: the moves played from a position
//! and its notable games, in the shape the oschess panel's explorer tabs use.

use chesscore::{Board, Move, Piece};

use cbformat::pgn::san;

use crate::api::{App, clip};
use crate::catalog::Entry;
use crate::http::{Request, Response};
use crate::json::{self, Obj};
use crate::reply::{bad_parameter, error, error_with, ok};
use crate::store::{Any, Head, Store, with_store};

use super::file::Bad;
use super::format::{Counts, MAX_PLY, Stats, TOP_GAMES, unpack_move};
use super::runs::Progress;
use super::source::average_elo;
use super::{Loaded, Lookup};

pub fn route(app: &App, entry: &Entry, req: &Request) -> Response {
    if req.param("variant").is_some_and(|v| v != "standard") {
        return unsupported();
    }
    let Some(fen) = req.param("fen") else { return bad_parameter("fen", "fen is required") };
    let Ok(board) = Board::from_fen(fen) else { return bad_parameter("fen", "fen is not a valid position") };
    // Castling rights named by rook file are Chess960 notation, and a position
    // whose castling needs Chess960 rules is Chess960: neither is indexed, since
    // the Polyglot key cannot tell which rook a right belongs to.
    if board.is_chess960() {
        return unsupported();
    }
    let open = match entry.open_to_read() {
        Ok(open) => open,
        Err(state) => {
            return error_with(409, "database_unavailable", "The database is not ready", |o| {
                o.str("state", state.name())
            });
        }
    };
    let Some(shared) = app.catalog.get(&entry.id) else { return crate::reply::not_found() };
    match app.catalog.explorer.index(shared, &open) {
        Lookup::Ready(loaded) => match loaded.lookup(board.hash()) {
            Ok(stats) => ok(render(&open.db, &board, stats, &loaded)),
            Err(Bad::Busy) => error(503, "busy", "The search memory is taken by searches; retry"),
            Err(_) => {
                app.catalog.explorer.forget(&entry.id);
                error_with(409, "database_unavailable", "The position index is being rebuilt", |o| {
                    o.str("state", "indexing")
                })
            }
        },
        Lookup::Pending(progress) => indexing(&progress),
        Lookup::Failed(why) => {
            error(503, "index_unavailable", &format!("The position index could not be built: {why}"))
        }
    }
}

fn unsupported() -> Response {
    error_with(422, "unsupported", "Chess960 positions are not indexed", |o| o.str("variant", "chess960"))
}

fn indexing(p: &Progress) -> Response {
    let progress = Obj::new()
        .str("phase", p.phase())
        .num("done", p.done.load(std::sync::atomic::Ordering::Relaxed) as i64)
        .num("total", p.total.load(std::sync::atomic::Ordering::Relaxed) as i64)
        .done();
    error_with(409, "database_unavailable", "The position index is being built", |o| {
        o.str("state", "indexing").raw("progress", &progress)
    })
}

fn counts(o: Obj, c: &Counts) -> Obj {
    o.num("games", c.games as i64)
        .num("white", c.white as i64)
        .num("draws", c.draws as i64)
        .num("black", c.black as i64)
}

/// The answer for `board`: its counts, its moves most played first, and its
/// notable games, best rated first.
pub fn render<'a>(db: impl Into<Any<'a>>, board: &Board, stats: Option<Stats>, loaded: &Loaded) -> String {
    let db = db.into();
    let stats = stats.unwrap_or_default();
    let moves = stats.moves.iter().filter_map(|(code, c)| {
        let mv = unpack_move(board, *code).filter(|&mv| board.is_legal(mv))?;
        Some(counts(Obj::new().str("uci", &uci(board, mv)).str("san", &san(board, mv)), c).done())
    });
    let mut top: Vec<(u16, u32, std::sync::Arc<str>)> = stats
        .top
        .iter()
        .filter_map(|&n| loaded.game(n, || with_store!(db, db => top_game(db, n))).map(|(elo, json)| (elo, n, json)))
        .collect();
    top.sort_unstable_by_key(|t| std::cmp::Reverse((t.0, t.1)));
    top.dedup_by_key(|t| t.1);
    top.truncate(TOP_GAMES);
    let games = top.iter().map(|t| t.2.to_string());
    let index = Obj::new()
        .num("records", i64::from(loaded.records()))
        .num("games", loaded.games() as i64)
        .num("maxPly", i64::from(MAX_PLY))
        .done();
    counts(Obj::new().str("generation", &format!("{:016x}", loaded.generation)), &stats.counts)
        .raw("moves", &json::array(moves))
        .raw("topGames", &json::array(games))
        .raw("index", &index)
        .done()
}

/// UCI with castling as the king's two-square step (`e1g1`, `e1c1`), for a
/// standard position; `chesscore` writes it as the king taking its rook.
fn uci(board: &Board, mv: Move) -> String {
    let castles =
        matches!(board.piece_at(mv.from), Some((Piece::King, c)) if board.piece_at(mv.to) == Some((Piece::Rook, c)));
    if castles {
        let file = if mv.to.file() > mv.from.file() { 'g' } else { 'c' };
        return format!("{}{file}{}", mv.from, mv.from.rank() + 1);
    }
    mv.to_string()
}

/// Game `number`'s rating, for ranking, and its entry in `topGames`.
fn top_game<S: Store>(db: &S, number: u32) -> Option<(u16, String)> {
    let r = db.record(number).ok()?;
    let name = |id: i64| db.player(id).ok().flatten().map(|p| clip(p.pgn())).unwrap_or_default();
    let event = db.tournament(r.tournament()).ok().flatten().map(|t| clip(t.title)).unwrap_or_default();
    let year = r.played_date().year();
    let (white_elo, black_elo) = r.elo();
    let o = Obj::new()
        .num("number", i64::from(number))
        .str("white", &name(r.white()))
        .str("black", &name(r.black()))
        .num("whiteElo", i64::from(white_elo.max(0)))
        .num("blackElo", i64::from(black_elo.max(0)))
        .str("result", r.result().pgn());
    let o = if year == 0 { o.raw("year", "null") } else { o.num("year", i64::from(year)) };
    Some((average_elo(&r), o.str("event", &event).done()))
}
