//! `GET /v1/databases/{id}/explorer?fen=`: the moves played from a position
//! and its notable games, in the shape the oschess panel's explorer tabs use.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chesscore::{Board, Move, Piece};

use cbformat::game::RecordKind;
use cbformat::pgn::san;
use cbformat::view::Base;

use crate::api::App;
use crate::catalog::{Entry, Opened};
use crate::http::{Request, Response};
use crate::json::{self, Obj};
use crate::reply::{bad_parameter, error, error_with, ok, unavailable};
use crate::rows::{Names, row_obj};
use crate::search::SearchError;
use crate::search::memory::{Cancel, Hold};
use crate::search::workers::{self, threads};
use crate::store::{Head, Store, with_store};

use super::file::Bad;
use super::format::{Counts, NO_MOVE, Stats, TOP_GAMES, structure, unpack_move};
use super::runs::Progress;
use super::source::average_elo;
use super::stream::{Hit, Target};
use super::{Loaded, Lookup};

pub fn route(app: &App, entry: &Entry, req: &Request) -> Response {
    // Asked for its explorer, the database is in use: the keeper rebuilds
    // its index when it changes (#149).
    app.catalog.explorer.mark_in_use(&entry.id);
    if req.param("variant").is_some_and(|v| v != "standard") {
        return unsupported();
    }
    let Some(fen) = req.param("fen") else { return bad_parameter("fen", "fen is required") };
    let board = match board(fen) {
        Ok(board) => board,
        Err(answer) => return answer,
    };
    let open = match entry.open_to_read() {
        Ok(open) => open,
        Err(state) => return unavailable(state),
    };
    let loaded = match ready(app, entry, &open) {
        Ok(loaded) => loaded,
        Err(answer) => return answer,
    };
    match stats(&loaded, &board, &Cancel::never()) {
        Ok(stats) => ok(render(&open.db, &board, stats, &loaded)),
        Err(Bad::Busy) => busy(),
        Err(_) => rebuilding(app, entry),
    }
}

/// The position `fen` names; else its answer: `400` naming `fen` for a FEN
/// that is not a valid position, `422 unsupported` for Chess960. Castling
/// rights named by rook file are Chess960 notation, and a position whose
/// castling needs Chess960 rules is Chess960: neither is indexed, since the
/// Polyglot key cannot tell which rook a right belongs to.
pub fn board(fen: &str) -> Result<Board, Response> {
    let Ok(board) = Board::from_fen(fen) else { return Err(bad_parameter("fen", "fen is not a valid position")) };
    if board.is_chess960() {
        return Err(unsupported());
    }
    Ok(board)
}

/// The position index of `entry` at the generation of `open`, ready to
/// answer; else the answer meanwhile: `409 database_unavailable` with
/// `state: "indexing"` and the progress while it is built, `503
/// index_unavailable` when it could not be, `503 busy` when the search memory
/// has no room to open it.
pub fn ready(app: &App, entry: &Entry, open: &Opened) -> Result<Arc<Loaded>, Response> {
    let Some(shared) = app.catalog.get(&entry.id) else { return Err(crate::reply::not_found()) };
    match app.catalog.explorer.index(shared, open) {
        Lookup::Ready(loaded) => Ok(loaded),
        Lookup::Pending(progress) => Err(indexing(&progress)),
        Lookup::Failed(why) => {
            Err(error(503, "index_unavailable", &format!("The position index could not be built: {why}")))
        }
        Lookup::Busy => Err(busy()),
    }
}

/// Drops the index of `entry`, which a read found damaged, so that the next
/// request builds it again; the answer meanwhile.
pub fn rebuilding(app: &App, entry: &Entry) -> Response {
    app.catalog.explorer.forget(&entry.id);
    error_with(409, "database_unavailable", "The position index is being rebuilt", |o| o.str("state", "indexing"))
}

fn busy() -> Response {
    error(503, "busy", "The search memory is taken by searches; retry")
}

/// The answer for a Chess960 position or variant.
pub fn unsupported() -> Response {
    error_with(422, "unsupported", "Chess960 positions are not indexed", |o| o.str("variant", "chess960"))
}

fn indexing(p: &Progress) -> Response {
    let progress = Obj::new()
        .str("phase", p.phase())
        .num("done", p.done.load(Ordering::Relaxed) as i64)
        .num("total", p.total.load(Ordering::Relaxed) as i64)
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
pub fn render(db: &Base, board: &Board, stats: Option<Stats>, loaded: &Loaded) -> String {
    let stats = stats.unwrap_or_default();
    let moves = stats.moves.iter().filter_map(|(code, c)| {
        let mv = unpack_move(board, *code).filter(|&mv| board.is_legal(mv))?;
        Some(counts(Obj::new().str("uci", &uci(board, mv)).str("san", &san(board, mv)), c).done())
    });
    let mut top: Vec<(u16, u32, std::sync::Arc<str>)> = with_store!(db, db => {
        // The names of one answer's games, read once for all of them.
        let mut names = Names::new(db);
        stats
            .top
            .iter()
            .filter_map(|&n| loaded.game(n, || top_game(db, &mut names, n)).map(|(elo, json)| (elo, n, json)))
            .collect()
    });
    top.sort_unstable_by_key(|t| std::cmp::Reverse((t.0, t.1)));
    top.dedup_by_key(|t| t.1);
    top.truncate(TOP_GAMES);
    let games = top.iter().map(|t| t.2.to_string());
    let index = Obj::new().num("records", i64::from(loaded.records())).num("games", loaded.games() as i64).done();
    counts(Obj::new().str("generation", &format!("{:016x}", loaded.generation)), &stats.counts)
        .raw("moves", &json::array(moves))
        .raw("topGames", &json::array(games))
        .raw("index", &index)
        .done()
}

/// Candidates a worker replays at least, so that a small bucket takes one.
/// A bucket that one worker would take whole is replayed on the calling
/// thread instead, which takes less time than starting a worker would, as
/// one of the shared workers all the same.
const DEEP_GAMES_PER_WORKER: usize = 256;
/// Candidates a worker takes at a time: the workers share a bucket as they
/// go, so that one the machine runs less often takes fewer.
const DEEP_GAMES_AT_ONCE: usize = 64;
/// Room for the moves played from a position: it has 218 legal moves at most.
const MAX_MOVES: usize = 256;

/// A move played from the position: its counts, and the first game, by
/// number, that played it, which orders moves played as often.
#[derive(Clone, Copy)]
struct Played {
    mv: u16,
    counts: Counts,
    first: u32,
}

/// What a worker's replays found, the same few bytes however many games
/// reach the position: their counts, the moves played with theirs, and the
/// best games so far by average rating, then number.
struct Found {
    counts: Counts,
    moves: Vec<Played>,
    top: Vec<(u16, u32)>,
}

impl Found {
    /// What one takes, all a worker reserves.
    const BYTES: usize =
        MAX_MOVES * std::mem::size_of::<Played>() + (TOP_GAMES + 1) * std::mem::size_of::<(u16, u32)>();

    fn new() -> Option<Found> {
        let mut moves = Vec::new();
        moves.try_reserve_exact(MAX_MOVES).ok()?;
        let mut top = Vec::new();
        top.try_reserve_exact(TOP_GAMES + 1).ok()?;
        Some(Found { counts: Counts::default(), moves, top })
    }

    /// Adds `counts` of games that played `mv` from the position (`NO_MOVE`
    /// when they ended there), the best of them ranked `best`, the first of
    /// them by number `first`.
    fn add(&mut self, mv: u16, counts: &Counts, best: (u16, u32), first: u32) {
        self.counts.merge(counts);
        self.add_move(Played { mv, counts: *counts, first });
        self.rank(best);
    }

    fn add_move(&mut self, played: Played) {
        if played.mv == NO_MOVE {
            return;
        }
        if let Some(m) = self.moves.iter_mut().find(|m| m.mv == played.mv) {
            m.counts.merge(&played.counts);
            m.first = m.first.min(played.first);
        } else if self.moves.len() < MAX_MOVES {
            // Every move played from one position is legal there, so the room
            // is never short, and nothing grows past what was reserved.
            self.moves.push(played);
        }
    }

    fn rank(&mut self, best: (u16, u32)) {
        let at = self.top.partition_point(|&b| b > best);
        if at < TOP_GAMES {
            self.top.insert(at, best);
            self.top.truncate(TOP_GAMES);
        }
    }

    fn merge(&mut self, other: &Found) {
        self.counts.merge(&other.counts);
        for &played in &other.moves {
            self.add_move(played);
        }
        for &best in &other.top {
            self.rank(best);
        }
    }

    /// The answer of these games alone: the moves played as often in the
    /// order their first games have.
    fn into_stats(mut self) -> Stats {
        self.moves.sort_unstable_by_key(|m| (std::cmp::Reverse(m.counts.games), m.first));
        let moves = self.moves.iter().map(|m| (m.mv, m.counts)).collect();
        Stats { counts: self.counts, moves, top: self.top.iter().map(|b| b.1).collect() }
    }
}

/// What the index answers for `board` (#146): the tree's record when it holds
/// the position, and the games of the position's structure that the tree did
/// not count, replayed from the move stream: all of them when the tree does
/// not hold it, else those that reach it first beyond the tree's plies, which
/// the deep section marks as holding its structure there. The tree counted
/// every other game that reaches it, once, at its first visit, so each game
/// counts once, with the move it played from its first visit.
/// Counts add, moves add by code, and the notable games are the best of
/// both, by rating, then number. `None` when no game reaches the position.
/// Errors as [`deep`]'s, and `Corrupt` when the sums count more games than
/// the index holds.
pub fn stats(loaded: &Loaded, board: &Board, cancel: &Cancel) -> Result<Option<Stats>, Bad> {
    let Some(mut tree) = loaded.lookup(board.hash())? else { return deep(loaded, board, cancel) };
    let target = Target::of(board).beyond(loaded.base.header.max_ply);
    let Some(found) = replay(loaded, board, &target, true, cancel)? else { return Ok(Some(tree)) };
    tree.counts = sum(&tree.counts, &found.counts, loaded.games())?;
    for played in &found.moves {
        match tree.moves.iter_mut().find(|m| m.0 == played.mv) {
            Some(m) => m.1 = sum(&m.1, &played.counts, tree.counts.games)?,
            None => tree.moves.push((played.mv, played.counts)),
        }
    }
    // Most played first, then by code, as the tree orders them.
    tree.moves.sort_unstable_by(|a, b| b.1.games.cmp(&a.1.games).then(a.0.cmp(&b.0)));
    // The tree ranks its games by the rating their stream entries keep.
    let mut top = found.top;
    for &game in &tree.top {
        top.push((loaded.stream.entry(game)?.elo(), game));
    }
    top.sort_unstable_by(|a, b| b.cmp(a));
    top.truncate(TOP_GAMES);
    tree.top = top.iter().map(|b| b.1).collect();
    Ok(Some(tree))
}

/// `a` and `b` added, as counts of at most `games` games. Each game counts
/// once, so anything more, a sum past 64 bits included, is damage that the
/// checks of the tree's record alone could not see.
fn sum(a: &Counts, b: &Counts, games: u64) -> Result<Counts, Bad> {
    a.checked_merge(b).filter(|c| c.within(games)).ok_or(Bad::Corrupt("counts"))
}

/// The games of `board`'s structure that reach it, whether the tree holds it
/// or not, as [`stats`] answers a position the tree does not hold: each
/// counted once at the first ply its main line reaches the position, with the
/// move played from there; the moves played as often in the order their first
/// games have. `None` when none does. A replay stops at its next game once
/// `cancel` is, and the answer is then `Busy`; a stream found damaged is
/// `Corrupt`.
pub fn deep(loaded: &Loaded, board: &Board, cancel: &Cancel) -> Result<Option<Stats>, Bad> {
    Ok(replay(loaded, board, &Target::of(board), false, cancel)?.map(Found::into_stats))
}

/// The games that hold `board`'s structure, only those that hold it beyond
/// the tree's plies when `beyond`, and that reach `target`, counted as
/// [`stats`] counts them; `None` when none does.
fn replay(
    loaded: &Loaded,
    board: &Board,
    target: &Target,
    beyond: bool,
    cancel: &Cancel,
) -> Result<Option<Found>, Bad> {
    let mut all: Option<Found> = None;
    for part in replay_with(loaded, board, target, beyond, cancel, &Counted)? {
        match &mut all {
            Some(all) => all.merge(&part),
            None => all = Some(part),
        }
    }
    Ok(all.filter(|all| all.counts.games > 0))
}

/// What the replays of a position's candidates keep of the games that reach
/// it: a part for each worker, which [`Keep::BYTES`] bounds.
pub(super) trait Keep: Sync {
    type Part: Send;
    /// What a part takes at most, which a worker reserves before it starts.
    const BYTES: usize;
    /// A part, allocated fallibly.
    fn part(&self) -> Option<Self::Part>;
    /// Adds game `game`, which reached the position as `hit` says, to `part`.
    fn add(&self, part: &mut Self::Part, game: u32, hit: Hit);
}

/// The explorer's counts, moves and notable games.
struct Counted;

impl Keep for Counted {
    type Part = Found;
    const BYTES: usize = Found::BYTES;

    fn part(&self) -> Option<Found> {
        Found::new()
    }

    fn add(&self, found: &mut Found, game: u32, hit: Hit) {
        let mut counts = Counts::default();
        counts.add(hit.outcome);
        found.add(hit.mv, &counts, (hit.elo, game), game);
    }
}

/// The games that hold `board`'s structure, only those that hold it beyond
/// the tree's plies when `beyond`, replayed to `target` from the move stream
/// ([`IndexFile::deep_games`] names the candidates), each that reaches it
/// kept by `keep` in the part of the worker that replayed it: on the calling
/// thread, taken as one of the shared workers, when one worker would take
/// them all, else on at most half of them, so that searches keep the rest.
/// The parts; none when no game holds the structure. Either waits for its
/// first worker as a search does, and is `Busy` when none comes free. A
/// replay stops at its next game once `cancel` is, and the answer is then
/// `Busy`; a stream found damaged is `Corrupt`.
///
/// [`IndexFile::deep_games`]: super::file::IndexFile::deep_games
pub(super) fn replay_with<K: Keep>(
    loaded: &Loaded,
    board: &Board,
    target: &Target,
    beyond: bool,
    cancel: &Cancel,
    keep: &K,
) -> Result<Vec<K::Part>, Bad> {
    let (games, _memory) = loaded.base.deep_games(structure(board), beyond)?;
    if games.is_empty() {
        return Ok(Vec::new());
    }
    let find = |game: u32, part: &mut K::Part| -> Result<(), Bad> {
        if let Some(hit) = loaded.stream.find(game, target)? {
            keep.add(part, game, hit);
        }
        Ok(())
    };
    if games.len() <= DEEP_GAMES_PER_WORKER {
        // Requests at once never replay on more threads than the workers.
        let _worker = workers::one(cancel).map_err(|_| Bad::Busy)?;
        let _memory = Hold::reserve(K::BYTES).map_err(|_| Bad::Busy)?;
        let mut part = keep.part().ok_or(Bad::Busy)?;
        for &game in &games {
            if cancel.is_cancelled() {
                return Err(Bad::Busy);
            }
            find(game, &mut part)?;
        }
        return Ok(vec![part]);
    }
    let want = games.len().div_ceil(DEEP_GAMES_PER_WORKER).min((threads() / 2).max(1));
    let next = AtomicUsize::new(0);
    let parts = workers::run(want, K::BYTES, cancel, |w| {
        let mut part = keep.part().ok_or(SearchError::Busy)?;
        loop {
            let from = next.fetch_add(DEEP_GAMES_AT_ONCE, Ordering::Relaxed);
            let Some(taken) = games.get(from..(from + DEEP_GAMES_AT_ONCE).min(games.len())) else { break };
            for &game in taken {
                if w.stopped() || cancel.is_cancelled() {
                    return Err(SearchError::Superseded);
                }
                if let Err(damaged) = find(game, &mut part) {
                    return Ok(Err(damaged));
                }
            }
        }
        Ok(Ok(part))
    })
    .map_err(|_| Bad::Busy)?;
    parts.into_iter().collect()
}

/// UCI with castling as the king's two-square step (`e1g1`, `e1c1`), for a
/// standard position; `chesscore` writes it as the king taking its rook. The
/// explorer's moves are written this way.
pub fn uci(board: &Board, mv: Move) -> String {
    let castles =
        matches!(board.piece_at(mv.from), Some((Piece::King, c)) if board.piece_at(mv.to) == Some((Piece::Rook, c)));
    if castles {
        let file = if mv.to.file() > mv.from.file() { 'g' } else { 'c' };
        return format!("{}{file}{}", mv.from, mv.from.rank() + 1);
    }
    mv.to_string()
}

/// Game `number`'s rating, for ranking, and its entry in `topGames`: its
/// `/games` row whole, and the year of its date for clients written before
/// rows (#144). `None` when the row cannot be read.
fn top_game<S: Store>(db: &S, names: &mut Names<'_, S>, number: u32) -> Option<(u16, String)> {
    let r = db.record(number).ok()?;
    let row = row_obj(names, &r).ok()?;
    // Only a game's header holds a date; any other record's row has `????.??.??`.
    let year = if matches!(r.kind(), RecordKind::Game) { r.played_date().year() } else { 0 };
    let row = if year == 0 { row.raw("year", "null") } else { row.num("year", i64::from(year)) };
    Some((average_elo(&r), row.done()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::format::Outcome;

    /// However many games a worker finds, what it keeps stays within what it
    /// reserved, and two workers' finds merge into the same answer as one's,
    /// each move with the first game that played it.
    #[test]
    fn found_games_are_added_up_in_the_room_reserved() {
        let (a, b) = (Found::new().unwrap(), Found::new().unwrap());
        let (mut one, mut halves) = (Found::new().unwrap(), [a, b]);
        let (moves, top) = (one.moves.capacity(), one.top.capacity());
        for n in 1..=50_000u32 {
            let mut counts = Counts::default();
            counts.add([Outcome::White, Outcome::Draw, Outcome::Black][n as usize % 3]);
            let mv = if n % 7 == 0 { NO_MOVE } else { (n % 5) as u16 + 1 };
            let best = ((n * 7919 % 3000) as u16, n);
            one.add(mv, &counts, best, n);
            halves[n as usize % 2].add(mv, &counts, best, n);
        }
        assert_eq!((one.moves.capacity(), one.top.capacity()), (moves, top), "nothing grew");
        assert_eq!(one.counts.games, 50_000);
        assert_eq!(one.moves.len(), 5);
        assert_eq!(one.moves.iter().map(|m| m.counts.games).sum::<u64>(), 50_000 - 50_000 / 7);
        // Move m is first played by game m - 1, and move 1 by game 5.
        let firsts = |f: &Found| {
            let mut v: Vec<(u16, u32, u64)> = f.moves.iter().map(|m| (m.mv, m.first, m.counts.games)).collect();
            v.sort_unstable();
            v
        };
        assert_eq!(
            firsts(&one).iter().map(|m| (m.0, m.1)).collect::<Vec<_>>(),
            [(1, 5), (2, 1), (3, 2), (4, 3), (5, 4)]
        );
        let [mut merged, other] = halves;
        merged.merge(&other);
        assert_eq!(merged.counts, one.counts);
        assert_eq!(merged.top, one.top);
        assert_eq!(firsts(&merged), firsts(&one));
        let mut expected: Vec<(u16, u32)> = (1..=50_000u32).map(|n| ((n * 7919 % 3000) as u16, n)).collect();
        expected.sort_unstable_by(|x, y| y.cmp(x));
        assert_eq!(one.top, expected[..TOP_GAMES]);
    }
}
