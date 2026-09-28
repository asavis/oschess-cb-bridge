//! Where an index gets its games: the positions of each game's main line, as a
//! small trait that each database format implements.

use chesscore::{Board, Color, Move, Piece};

use cbformat::game::{GameResult, RecordKind};
use cbformat::pgnfile::lex::{self, Lexer};
use cbformat::pgnfile::line::{LineEnd, main_line};
use cbformat::replay::{self, TreeVisitor, start_board};
use cbformat::v2::{self, HEADER_RECORD_SIZE, MoveData, Record};
use cbformat::view::Base;
use cbformat::{Error, Result, cbh, pgnfile};

use super::format::{NO_MOVE, Outcome, PRUNE_PLY, pack_move, structure};
use crate::store::{Head, Store};

/// What a walk takes from each game's main line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The positions within the index's plies, for the tree.
    Tree,
    /// Those, and every [`structure`] the main line holds past
    /// [`PRUNE_PLY`], for the deep section (#133): the whole line is read.
    Build,
    /// The first ply the main line reaches this position at, and the move
    /// played from it.
    Find(Target),
}

/// A position looked for in games: its key, and each side's men and pawns.
/// A game only ever loses those, so a line left with fewer than the position
/// has can no longer reach it, and its walk stops there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    key: u64,
    counts: [u32; 4],
}

impl Target {
    pub fn of(board: &Board) -> Target {
        Target { key: board.hash(), counts: counts(board) }
    }
}

/// Each side's men, then each side's pawns.
fn counts(board: &Board) -> [u32; 4] {
    [
        board.colors(Color::White).count_ones(),
        board.colors(Color::Black).count_ones(),
        board.colored(Piece::Pawn, Color::White).count_ones(),
        board.colored(Piece::Pawn, Color::Black).count_ones(),
    ]
}

/// The most structures a main line holds: each change of one is a pawn
/// moving forward or a capture, so no line holds more.
const MAX_STRUCTURES: usize = 8 * 6 * 2 + 30 + 1;

/// One game's contribution to the index.
pub struct Line {
    pub number: u32,
    pub outcome: Outcome,
    /// The average rating of the two players, or the one known; 0 with none.
    pub elo: u16,
    /// Each position the main line reaches within the index's plies, once,
    /// with the move played from it (`NO_MOVE` at the end) and its ply.
    pub positions: Vec<(u64, u16, u8)>,
    /// [`Mode::Build`]: the structures the main line holds past
    /// [`PRUNE_PLY`], each once, in the order it reaches them. A structure
    /// never comes back once it changed, so the last one is all a new one is
    /// compared with.
    pub structures: Vec<u64>,
    /// [`Mode::Find`]: the move played from the position's first occurrence
    /// (`NO_MOVE` at the line's end), once the line reached it.
    pub found: Option<u16>,
    pub mode: Mode,
    /// The position the walk is at, noted before its move is played: its key
    /// and, in [`Mode::Build`] past [`PRUNE_PLY`], its structure.
    here: u64,
    here_structure: u64,
    /// [`Mode::Find`]: the position the walk is at has fewer men or pawns of
    /// a side than the one looked for.
    past: bool,
}

impl Line {
    /// Adds a position the first time the line reaches it.
    fn reach(&mut self, key: u64, mv: u16, ply: u8) {
        if !self.positions.iter().any(|p| p.0 == key) {
            self.positions.push((key, mv, ply));
        }
    }

    /// Whether the walk reads the move played from the position at `ply`.
    fn reads(&self, ply: u32, max_ply: u8) -> bool {
        self.mode != Mode::Tree || ply <= u32::from(max_ply)
    }

    /// Notes the position `board` at `ply`, before its move is played.
    fn at(&mut self, board: &Board, ply: u32) {
        self.here = board.hash();
        match self.mode {
            Mode::Build if ply > u32::from(PRUNE_PLY) => self.here_structure = structure(board),
            Mode::Find(target) => {
                self.past = counts(board).iter().zip(target.counts).any(|(&have, need)| have < need);
            }
            _ => {}
        }
    }

    /// Takes the position noted at `ply`, with `mv` played from it
    /// (`NO_MOVE` at the line's end); whether the walk goes on.
    fn visit(&mut self, mv: u16, ply: u32, max_ply: u8) -> bool {
        if let Mode::Find(target) = self.mode {
            if self.here == target.key {
                self.found = Some(mv);
                return false;
            }
            return !self.past;
        }
        if ply <= u32::from(max_ply) {
            self.reach(self.here, mv, ply as u8);
        }
        if self.mode == Mode::Build && ply > u32::from(PRUNE_PLY) {
            let structure = self.here_structure;
            if self.structures.last() != Some(&structure) && self.structures.len() < MAX_STRUCTURES {
                self.structures.push(structure);
            }
        }
        true
    }
}

/// Records read at a time.
pub const RECORDS: usize = 2048;
/// The largest move record's content indexed, as `GET .../games/{number}`
/// serves (`crate::store::MAX_GAME_BYTES`). A larger one leaves its game out.
pub const MAX_MOVE_RECORD: usize = crate::store::MAX_GAME_BYTES;
/// The frame around a record's content and spare area.
const FRAME_BYTES: usize = 64;
/// The move buffer: a run's move records back to back when they fit, else one
/// record at a time.
pub const MOVE_BYTES: usize = MAX_MOVE_RECORD + FRAME_BYTES;

/// A worker's buffers for reading games, allocated once, fallibly, after the
/// worker reserved [`Workspace::BYTES`] in the search budget.
pub struct Workspace {
    headers: Vec<u8>,
    records: Records,
    moves: Vec<u8>,
    /// The games of a run whose move records the buffer did not hold at once.
    later: Vec<u32>,
    line: Line,
    /// Reads PGN games' texts.
    lexer: Lexer,
    /// Games left out because their moves could not be read: damaged, or a
    /// move record over [`MAX_MOVE_RECORD`].
    pub skipped: u64,
}

/// A run's header records, in the format being read.
struct Records {
    two_cbh: Vec<Record>,
    classic: Vec<cbh::Record>,
}

impl Workspace {
    /// What a workspace takes: the buffers, a line of at most 41 positions
    /// and its structures.
    pub const BYTES: usize = (RECORDS + 1)
        * (HEADER_RECORD_SIZE + std::mem::size_of::<Record>() + std::mem::size_of::<cbh::Record>() + 4)
        + MOVE_BYTES
        + lex::MAX_TAG_VALUE
        + lex::MAX_TAG_NAME
        + lex::MAX_SYMBOL
        + 1024
        + MAX_STRUCTURES * 8;

    /// Sets what the walks take from each game from now on.
    pub fn set_mode(&mut self, mode: Mode) {
        self.line.mode = mode;
    }

    /// The last game's line, as the walk left it.
    pub fn line(&self) -> &Line {
        &self.line
    }

    pub fn new() -> Option<Workspace> {
        let buf = |n: usize| {
            let mut v = Vec::new();
            v.try_reserve_exact(n).ok()?;
            Some(v)
        };
        let mut positions = Vec::new();
        positions.try_reserve_exact(64).ok()?;
        let mut structures = Vec::new();
        structures.try_reserve_exact(MAX_STRUCTURES).ok()?;
        let mut two_cbh = Vec::new();
        two_cbh.try_reserve_exact(RECORDS + 1).ok()?;
        let mut classic = Vec::new();
        classic.try_reserve_exact(RECORDS + 1).ok()?;
        let mut later = Vec::new();
        later.try_reserve_exact(RECORDS).ok()?;
        Some(Workspace {
            headers: buf((RECORDS + 1) * HEADER_RECORD_SIZE)?,
            records: Records { two_cbh, classic },
            moves: buf(MOVE_BYTES)?,
            later,
            line: Line {
                number: 0,
                outcome: Outcome::Other,
                elo: 0,
                positions,
                structures,
                found: None,
                mode: Mode::Tree,
                here: 0,
                here_structure: 0,
                past: false,
            },
            lexer: Lexer::new(),
            skipped: 0,
        })
    }
}

/// A database the index can be built from.
pub trait Source: Sync {
    fn records(&self) -> u32;

    /// Calls `each` for every game of records `first..=last` that the index
    /// holds, in order: standard chess, not deleted, and not a guiding text
    /// or analysis. A game whose moves are damaged contributes the positions
    /// before the damage; one whose move record cannot be read is skipped and
    /// counted in `work`. Only a failed read is an error. Everything is read
    /// into `work`'s buffers, and annotations never.
    fn lines(
        &self,
        first: u32,
        last: u32,
        max_ply: u8,
        work: &mut Workspace,
        each: &mut dyn FnMut(&Line),
    ) -> Result<()>;
}

/// What reading games' main lines needs of a format, besides [`Store`].
trait Games: Store {
    type Window: Copy;
    type Moves<'a>;
    /// The workspace's records of this format.
    fn run_of(records: &mut Records) -> &mut Vec<Self::Head>;
    /// Reads the move records of `run` into `buf` at once, when they fit.
    fn window(&self, run: &[Self::Head], next: Option<&Self::Head>, buf: &mut Vec<u8>) -> Result<Option<Self::Window>>;
    /// The move record of `r` from the window `buf` holds, when it is there,
    /// refused as [`Games::read_moves`] refuses it when over
    /// [`MAX_MOVE_RECORD`].
    fn moves_in<'a>(&self, window: Self::Window, buf: &'a [u8], r: &Self::Head) -> Option<Result<Self::Moves<'a>>>;
    /// Reads the move record of `r` into `buf`, within [`MAX_MOVE_RECORD`].
    fn read_moves<'a>(&self, r: &Self::Head, buf: &'a mut Vec<u8>) -> Result<Self::Moves<'a>>;
    /// Fills `line` with the main line of `r`'s game; whether the index
    /// holds the game.
    fn walk(moves: &Self::Moves<'_>, r: &Self::Head, max_ply: u8, line: &mut Line) -> bool;
}

impl Games for v2::Database {
    type Window = v2::MoveWindow;
    type Moves<'a> = MoveData<'a>;
    fn run_of(records: &mut Records) -> &mut Vec<Record> {
        &mut records.two_cbh
    }
    fn window(&self, run: &[Record], next: Option<&Record>, buf: &mut Vec<u8>) -> Result<Option<v2::MoveWindow>> {
        self.read_move_window(run, next, buf)
    }
    fn moves_in<'a>(&self, window: v2::MoveWindow, buf: &'a [u8], r: &Record) -> Option<Result<MoveData<'a>>> {
        v2::Database::moves_in(self, window, buf, r, MAX_MOVE_RECORD)
    }
    fn read_moves<'a>(&self, r: &Record, buf: &'a mut Vec<u8>) -> Result<MoveData<'a>> {
        self.read_moves_into(r, MAX_MOVE_RECORD, buf)
    }
    fn walk(moves: &MoveData<'_>, r: &Record, max_ply: u8, line: &mut Line) -> bool {
        walk(moves, r, max_ply, line)
    }
}

impl Games for cbh::Database {
    type Window = cbh::MoveWindow;
    type Moves<'a> = cbh::MoveData<'a>;
    fn run_of(records: &mut Records) -> &mut Vec<cbh::Record> {
        &mut records.classic
    }
    fn window(
        &self,
        run: &[cbh::Record],
        next: Option<&cbh::Record>,
        buf: &mut Vec<u8>,
    ) -> Result<Option<cbh::MoveWindow>> {
        self.read_move_window(run, next, buf)
    }
    fn moves_in<'a>(
        &self,
        window: cbh::MoveWindow,
        buf: &'a [u8],
        r: &cbh::Record,
    ) -> Option<Result<cbh::MoveData<'a>>> {
        cbh::Database::moves_in(self, window, buf, r, MAX_MOVE_RECORD)
    }
    fn read_moves<'a>(&self, r: &cbh::Record, buf: &'a mut Vec<u8>) -> Result<cbh::MoveData<'a>> {
        self.read_moves_into(r, MAX_MOVE_RECORD, buf)
    }
    fn walk(moves: &cbh::MoveData<'_>, r: &cbh::Record, max_ply: u8, line: &mut Line) -> bool {
        walk_classic(moves, r, max_ply, line)
    }
}

impl Source for v2::Database {
    fn records(&self) -> u32 {
        self.record_count()
    }

    fn lines(
        &self,
        first: u32,
        last: u32,
        max_ply: u8,
        work: &mut Workspace,
        each: &mut dyn FnMut(&Line),
    ) -> Result<()> {
        lines(self, first, last, max_ply, work, each)
    }
}

impl Source for cbh::Database {
    fn records(&self) -> u32 {
        self.record_count()
    }

    fn lines(
        &self,
        first: u32,
        last: u32,
        max_ply: u8,
        work: &mut Workspace,
        each: &mut dyn FnMut(&Line),
    ) -> Result<()> {
        lines(self, first, last, max_ply, work, each)
    }
}

impl Source for Base {
    fn records(&self) -> u32 {
        self.record_count()
    }

    fn lines(
        &self,
        first: u32,
        last: u32,
        max_ply: u8,
        work: &mut Workspace,
        each: &mut dyn FnMut(&Line),
    ) -> Result<()> {
        match self {
            Base::TwoCbh(db) => lines(db, first, last, max_ply, work, each),
            Base::Cbh(db) => lines(db, first, last, max_ply, work, each),
            Base::Pgn(db) => db.lines(first, last, max_ply, work, each),
        }
    }
}

/// A PGN file's games are read from its text: the texts of consecutive games
/// at once when the move buffer holds them, as a run's move records are.
impl Source for pgnfile::Database {
    fn records(&self) -> u32 {
        self.record_count()
    }

    fn lines(
        &self,
        first: u32,
        last: u32,
        max_ply: u8,
        work: &mut Workspace,
        each: &mut dyn FnMut(&Line),
    ) -> Result<()> {
        const SIZE: usize = pgnfile::RECORD_SIZE;
        let last = last.min(self.record_count());
        let mut next = first.max(1);
        let Workspace { headers, moves, line, lexer, skipped, .. } = work;
        while next <= last {
            let want = (last - next + 1).min(RECORDS as u32) as usize;
            // Within the capacity reserved for the headers of a 2CBH run.
            headers.clear();
            headers.resize(want * SIZE, 0);
            let read = self.read_records(next, headers)? as usize;
            if read == 0 {
                break;
            }
            let record = |i: usize| {
                let bytes = headers[i * SIZE..(i + 1) * SIZE].try_into().expect("a whole record");
                pgnfile::Record::from_bytes(next + i as u32, bytes)
            };
            let mut i = 0;
            while i < read {
                // The games from `i` whose texts the buffer holds together.
                let start = record(i).offset();
                let (mut end, mut j) = (start, i);
                while j < read {
                    let r = record(j);
                    let Some(stop) = r.offset().checked_add(u64::from(r.len())) else { break };
                    if r.offset() < start || stop - start > MAX_MOVE_RECORD as u64 {
                        break;
                    }
                    end = end.max(stop);
                    j += 1;
                }
                if j == i {
                    // Longer than a move record may be, or placed by damage.
                    *skipped += 1;
                    i += 1;
                    continue;
                }
                moves.clear();
                moves.resize((end - start) as usize, 0);
                self.read_span(start, moves)?;
                for k in i..j {
                    let r = record(k);
                    let at = (r.offset() - start) as usize;
                    if walk_pgn(&moves[at..at + r.len() as usize], &r, max_ply, line, lexer) {
                        each(line);
                    }
                }
                i = j;
            }
            next += read as u32;
        }
        Ok(())
    }
}

/// [`Source::lines`] for a format.
fn lines<S: Games>(
    db: &S,
    first: u32,
    last: u32,
    max_ply: u8,
    work: &mut Workspace,
    each: &mut dyn FnMut(&Line),
) -> Result<()> {
    let last = last.min(db.record_count());
    let mut next = first.max(1);
    let Workspace { headers, records, moves, later, line, skipped, .. } = work;
    let records = S::run_of(records);
    while next <= last {
        let want = (last - next + 1).min(RECORDS as u32) as usize;
        // One header past the run, when there is one: its move record
        // starts where the run's last one ends.
        headers.clear();
        headers.resize((want + 1) * S::HEAD_BYTES, 0);
        let read = db.read_records(next, headers)? as usize;
        if read == 0 {
            break;
        }
        let count = read.min(want);
        // Within the capacity reserved for it: nothing is allocated.
        records.clear();
        for i in 0..read {
            let at = i * S::HEAD_BYTES;
            records.push(S::head(next + i as u32, &headers[at..at + S::HEAD_BYTES]));
        }
        let (run, after) = records.split_at(count);
        // The run's move records at once when the buffer holds them; the
        // others, one by one, after the run.
        let window = db.window(run, after.first(), moves)?;
        later.clear();
        for (i, r) in run.iter().enumerate() {
            if r.kind() != RecordKind::Game || r.is_deleted() {
                continue;
            }
            match window.and_then(|w| db.moves_in(w, moves, r)) {
                Some(Ok(data)) => {
                    if S::walk(&data, r, max_ply, line) {
                        each(line);
                    }
                }
                Some(Err(_)) => *skipped += 1,
                None => later.push(i as u32),
            }
        }
        for &i in later.iter() {
            let r = &run[i as usize];
            match db.read_moves(r, moves) {
                Ok(data) => {
                    if S::walk(&data, r, max_ply, line) {
                        each(line);
                    }
                }
                Err(e @ Error::Io(..)) => return Err(e),
                Err(_) => *skipped += 1,
            }
        }
        next += count as u32;
    }
    Ok(())
}

/// Fills `line` with the main line of `record`'s 2CBH game; whether the
/// index holds the game.
fn walk(data: &MoveData<'_>, record: &Record, max_ply: u8, line: &mut Line) -> bool {
    let Ok(moves) = data.moves() else { return false };
    if moves.is_chess960() {
        return false;
    }
    let Ok(mut board) = moves.start().and_then(|s| start_board(&s)) else { return false };
    if board.is_chess960() {
        return false;
    }
    begin(line, record);
    let mut words = moves.main_line();
    let mut ply = 0u32;
    loop {
        line.at(&board, ply);
        let played = if line.reads(ply, max_ply) { words.next() } else { None };
        let mv = match played.map(|w| replay::play(&mut board, w)) {
            Some(Ok(Some(mv))) => Some(mv),
            // The end of the line, a null move, damage or the index's depth:
            // the position is reached, and no move from it is counted.
            _ => None,
        };
        if !line.visit(mv.map_or(NO_MOVE, pack_move), ply, max_ply) || mv.is_none() {
            return true;
        }
        ply += 1;
    }
}

/// Fills `line` with the main line of `record`'s classic game, the same way
/// as [`walk`]. The compact encoding names a move by the position it is
/// played in, so the tree is walked; the main line comes first in it, and
/// a walk stopped by damage has reported the moves before the damage.
fn walk_classic(data: &cbh::MoveData<'_>, record: &cbh::Record, max_ply: u8, line: &mut Line) -> bool {
    let Ok(moves) = data.moves() else { return false };
    if moves.is_chess960() {
        return false;
    }
    // Resolved once: a set-up game's start can take a replay of all its moves.
    let Ok(start) = cbh::start_as_played(&moves) else { return false };
    let Ok(board) = start_board(&start) else { return false };
    if board.is_chess960() {
        return false;
    }
    begin(line, record);
    line.at(&board, 0);
    let mut main = MainLine { line, max_ply, ply: 0, pending: None, done: false };
    // Damage ends the line where it is: the positions before it are kept.
    let _ = cbh::walk_from(&moves, &start, &mut main);
    if !main.done {
        main.line.visit(NO_MOVE, main.ply, main.max_ply);
    }
    true
}

/// The main line of a tree walk, as [`walk`] reads it from a 2CBH game.
struct MainLine<'a> {
    line: &'a mut Line,
    max_ply: u8,
    ply: u32,
    /// A main-line move announced and not yet played: it counts once it is,
    /// as an illegal move is reported before it is found to be one.
    pending: Option<u16>,
    /// The line ended: at the index's depth, a null move, or the first move
    /// off the main line.
    done: bool,
}

impl TreeVisitor for MainLine<'_> {
    fn play(&mut self, _before: &Board, mv: Option<Move>, main_line: bool) {
        if self.done {
            return;
        }
        match mv.filter(|_| main_line && self.line.reads(self.ply, self.max_ply)) {
            Some(mv) => self.pending = Some(pack_move(mv)),
            None => {
                self.line.visit(NO_MOVE, self.ply, self.max_ply);
                self.done = true;
            }
        }
    }

    fn played(&mut self, after: &Board) {
        if let Some(mv) = self.pending.take() {
            if !self.line.visit(mv, self.ply, self.max_ply) {
                self.done = true;
                return;
            }
            self.ply += 1;
            self.line.at(after, self.ply);
        }
    }

    /// Once the line is done the rest of the game is not decoded: a find
    /// replays each candidate only as far as it has to.
    fn stopped(&self) -> bool {
        self.done
    }
}

/// Fills `line` with the main line of a PGN game's text, as [`walk`] reads a
/// 2CBH game; whether the index holds the game. The moves are read as
/// written ([`pgnfile::line`]): the line ends at the first that names no
/// legal move, a null move among them.
fn walk_pgn(text: &[u8], r: &pgnfile::Record, max_ply: u8, line: &mut Line, lexer: &mut Lexer) -> bool {
    if r.is_chess960() || r.is_other_variant() {
        return false;
    }
    begin(line, r);
    let (mut ply, mut chess960) = (0u32, false);
    let end = main_line(text, lexer, &mut |board, mv| {
        if ply == 0 && board.is_chess960() {
            chess960 = true;
            return false;
        }
        line.at(board, ply);
        match mv.filter(|_| line.reads(ply, max_ply)) {
            Some(mv) => {
                let more = line.visit(pack_move(mv), ply, max_ply);
                ply += 1;
                more
            }
            // The end of the line, the index's depth, a null move or a move
            // it cannot play: the position is reached, and no move from it is
            // counted.
            None => {
                line.visit(NO_MOVE, ply, max_ply);
                false
            }
        }
    });
    !chess960 && end != LineEnd::BadStart
}

/// Starts `line` for `r`'s game.
fn begin(line: &mut Line, r: &impl Head) {
    line.number = r.id();
    line.outcome = outcome(r);
    line.elo = average_elo(r);
    line.positions.clear();
    line.structures.clear();
    line.found = None;
}

pub fn outcome(r: &impl Head) -> Outcome {
    match r.result() {
        GameResult::WhiteWins | GameResult::WhiteWinsForfeit => Outcome::White,
        GameResult::Draw | GameResult::DrawForfeit => Outcome::Draw,
        GameResult::BlackWins | GameResult::BlackWinsForfeit => Outcome::Black,
        _ => Outcome::Other,
    }
}

/// The players' average rating, the one known when the other is missing, or
/// 0; at most 4095, which the index stores in 12 bits.
pub fn average_elo(r: &impl Head) -> u16 {
    let (w, b) = r.elo();
    let (w, b) = (w.max(0) as u32, b.max(0) as u32);
    let avg = match (w, b) {
        (0, x) | (x, 0) => x,
        _ => (w + b) / 2,
    };
    avg.min(4095) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::format::MAX_PLY;

    /// A classic game's walk is told to stop once the position is found.
    #[test]
    fn a_classic_walk_stops_at_the_position_found() {
        let mut work = Workspace::new().unwrap();
        let start = Board::startpos();
        let mut e4 = start.clone();
        e4.play_checked("e2e4".parse().unwrap()).unwrap();
        let mut e5 = e4.clone();
        e5.play_checked("e7e5".parse().unwrap()).unwrap();
        work.set_mode(Mode::Find(Target::of(&e4)));
        work.line.at(&start, 0);
        let mut main = MainLine { line: &mut work.line, max_ply: MAX_PLY, ply: 0, pending: None, done: false };
        main.play(&start, Some("e2e4".parse().unwrap()), true);
        main.played(&e4);
        assert!(!main.stopped(), "not found yet");
        let reply: Move = "e7e5".parse().unwrap();
        main.play(&e4, Some(reply), true);
        main.played(&e5);
        assert!(main.stopped(), "found: the rest of the game is not decoded");
        assert_eq!(work.line.found, Some(pack_move(reply)));
    }
}
