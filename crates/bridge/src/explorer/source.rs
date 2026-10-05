//! Where an index gets its games: the positions of each game's main line, as a
//! small trait that each database format implements.

use chesscore::{Board, Move, Piece};

use cbformat::game::{GameResult, RecordKind};
use cbformat::movetable::{self, FIRST_CASTLE_960, MoveWord};
use cbformat::pgnfile::lex::{self, Lexer};
use cbformat::pgnfile::line::{LineEnd, main_line};
use cbformat::replay::{self, TreeVisitor, start_board};
use cbformat::v2::{self, HEADER_RECORD_SIZE, MoveData, Record};
use cbformat::view::Base;
use cbformat::{Error, Result, cbh, pgnfile};

use super::format::{MAX_PLY, NO_MOVE, Outcome, pack_move, structure};
use super::stream::{self, Departures, MAX_PLIES, SETUP_BYTES};
use crate::store::{Head, Store};

/// The most structures a main line holds: each change of one is a pawn
/// moving forward or a capture, so no line holds more.
pub(super) const MAX_STRUCTURES: usize = 8 * 6 * 2 + 30 + 1;

/// One game's contribution to the index. Its main line is read to its end,
/// or to [`MAX_PLIES`], where the move stream ends it.
pub struct Line {
    pub number: u32,
    pub outcome: Outcome,
    /// The average rating of the two players, or the one known; 0 with none.
    pub elo: u16,
    pub rating_sum: u32,
    pub date: u32,
    /// Each position the main line reaches within the index's plies, once,
    /// with the move played from it (`NO_MOVE` at the end) and its ply: a
    /// build counts them, and replays them from the move stream (#147).
    pub positions: Vec<(u64, u16, u8)>,
    /// The structures the main line holds past [`MAX_PLY`], each once, in
    /// the order it reaches them, which a build counts as well. A structure
    /// never comes back once it changed, so the last one is all a new one is
    /// compared with.
    pub structures: Vec<u64>,
    /// The first of the structures that the line holds beyond the index's
    /// plies, which the ones after it follow; `None` when the line ends
    /// within them. A game can reach a position the tree holds first beyond
    /// its plies only in one of these (#146).
    pub beyond: Option<usize>,
    /// The main line's moves as 2CBH move words, each checked as it was
    /// played: normal moves and the four castlings of standard chess, for the
    /// move stream (#145).
    pub words: Vec<u16>,
    /// The start, when it is not the standard one ([`stream::setup_of`]).
    pub setup: Option<[u8; SETUP_BYTES]>,
    /// The move number of a set-up start; 0 for the standard one.
    pub start_move: u16,
    /// The home pawns in the order the line lost them.
    pub departures: Departures,
    /// The position the walk is at, noted before its move is played: its
    /// key, its structure past [`MAX_PLY`], and its home pawns.
    here: u64,
    here_structure: u64,
    home: u16,
    /// The pawns and the men the structure noted last was found for, which
    /// it stays while they do.
    structure_of: Option<(u64, u32)>,
}

impl Line {
    /// Game `number`'s line of `words` from `setup`, as a walk leaves it, for
    /// the tests of the move stream.
    #[cfg(test)]
    pub(super) fn of(number: u32, words: Vec<u16>, setup: Option<[u8; SETUP_BYTES]>, outcome: Outcome) -> Line {
        Line {
            number,
            outcome,
            elo: 2000,
            rating_sum: 4000,
            date: 0,
            positions: Vec::new(),
            structures: Vec::new(),
            beyond: None,
            words,
            start_move: setup.map_or(0, |_| 1),
            setup,
            departures: Departures::default(),
            here: 0,
            here_structure: 0,
            home: 0,
            structure_of: None,
        }
    }

    /// Adds a position the first time the line reaches it.
    fn reach(&mut self, key: u64, mv: u16, ply: u8) {
        if !self.positions.iter().any(|p| p.0 == key) {
            self.positions.push((key, mv, ply));
        }
    }

    /// Whether the walk reads the move played from the position at `ply`:
    /// the move stream keeps at most [`MAX_PLIES`] of a line.
    fn reads(&self, ply: u32) -> bool {
        (ply as usize) < MAX_PLIES
    }

    /// Notes the line's start, `board`: a set-up start is kept.
    fn start(&mut self, board: &Board) {
        let setup = stream::setup_of(board);
        // A start at another move than the first is kept, whatever its
        // pieces: its moves are numbered from it (#272).
        let number = board.fullmove_number().max(1);
        self.setup = (setup != *stream::standard_setup() || number != 1).then_some(setup);
        self.start_move = self.setup.map_or(0, |_| number);
        self.home = stream::home_pawns(board);
    }

    /// Notes the position `board` at `ply`, before its move is played.
    fn at(&mut self, board: &Board, ply: u32) {
        self.here = board.hash();
        if ply > u32::from(MAX_PLY) {
            // Only a pawn's move or a capture changes a structure, and each
            // changes the pawns or the men.
            let now = (board.pieces(Piece::Pawn), board.occupied().count_ones());
            if self.structure_of != Some(now) {
                self.structure_of = Some(now);
                self.here_structure = structure(board);
            }
        }
        let home = stream::home_pawns(board);
        let mut left = self.home & !home;
        while left != 0 {
            self.departures.push(left.trailing_zeros());
            left &= left - 1;
        }
        self.home = home;
    }

    /// Takes the position noted at `ply`, with `mv` played from it
    /// (`NO_MOVE` at the line's end).
    fn visit(&mut self, mv: u16, ply: u32, max_ply: u8) {
        if ply <= u32::from(max_ply) {
            self.reach(self.here, mv, ply as u8);
        }
        if ply > u32::from(MAX_PLY) {
            let structure = self.here_structure;
            if self.structures.last() != Some(&structure) && self.structures.len() < MAX_STRUCTURES {
                self.structures.push(structure);
            }
            if ply > u32::from(max_ply) && self.beyond.is_none() {
                self.beyond = Some(self.structures.len().saturating_sub(1));
            }
        }
    }

    /// Takes `word`, the move just played from the position visited last.
    fn played(&mut self, word: u16) {
        self.words.push(word);
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
    /// What a workspace takes: the buffers, a line of at most 21 positions
    /// and its structures. Its words, once kept ([`Workspace::keep_words`]),
    /// are counted in [`stream::WORKER_BYTES`].
    pub const BYTES: usize = (RECORDS + 1)
        * (HEADER_RECORD_SIZE + std::mem::size_of::<Record>() + std::mem::size_of::<cbh::Record>() + 4)
        + MOVE_BYTES
        + lex::MAX_TAG_VALUE
        + lex::MAX_TAG_NAME
        + lex::MAX_SYMBOL
        + 1024
        + MAX_STRUCTURES * 8;

    /// Makes room for a line's words, [`MAX_PLIES`] at most, so that the
    /// walks allocate nothing for them; `None` when there is none.
    pub fn keep_words(&mut self) -> Option<()> {
        let words = &mut self.line.words;
        words.try_reserve_exact(MAX_PLIES.saturating_sub(words.len())).ok()
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
                rating_sum: 3000,
                date: 0,
                positions,
                structures,
                beyond: None,
                words: Vec::new(),
                setup: None,
                start_move: 0,
                departures: Departures::default(),
                here: 0,
                here_structure: 0,
                home: 0,
                structure_of: None,
            },
            lexer: Lexer::new(),
            skipped: 0,
        })
    }
}

/// A database the index can be built from.
pub trait Source: Sync {
    fn records(&self) -> u32;
    /// Newest non-deleted game date in a bounded header batch; synthetic
    /// sources without headers have no dates. Never called by a query.
    fn newest_date(&self, _first: u32, _last: u32, _work: &mut Workspace) -> Result<u32> {
        Ok(0)
    }

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

/// One bounded sequential header read, without names, moves or annotations.
fn newest_date<S: Store>(db: &S, first: u32, last: u32, work: &mut Workspace) -> Result<u32> {
    let count = last.min(db.record_count()).saturating_add(1).saturating_sub(first).min(RECORDS as u32) as usize;
    work.headers.resize(count * S::HEAD_BYTES, 0);
    let read = db.read_records(first, &mut work.headers)? as usize;
    Ok((0..read)
        .map(|i| S::head(first + i as u32, &work.headers[i * S::HEAD_BYTES..(i + 1) * S::HEAD_BYTES]))
        .filter(|r| r.kind() == RecordKind::Game && !r.is_deleted())
        .map(|r| super::ranking::date(r.played_date()))
        .max()
        .unwrap_or(0))
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
    fn newest_date(&self, first: u32, last: u32, work: &mut Workspace) -> Result<u32> {
        newest_date(self, first, last, work)
    }

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
    fn newest_date(&self, first: u32, last: u32, work: &mut Workspace) -> Result<u32> {
        newest_date(self, first, last, work)
    }

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
    fn newest_date(&self, first: u32, last: u32, work: &mut Workspace) -> Result<u32> {
        match self {
            Base::TwoCbh(db) => newest_date(db, first, last, work),
            Base::Cbh(db) => newest_date(db, first, last, work),
            Base::Pgn(db) => newest_date(db, first, last, work),
        }
    }

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
    fn newest_date(&self, first: u32, last: u32, work: &mut Workspace) -> Result<u32> {
        newest_date(self, first, last, work)
    }

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
/// index holds the game. Its words are kept as stored, each checked as it is
/// played.
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
    line.start(&board);
    let mut words = moves.main_line();
    let mut ply = 0u32;
    loop {
        line.at(&board, ply);
        let word = if line.reads(ply) { words.next() } else { None };
        let played = word.and_then(|w| match replay::play(&mut board, w) {
            Ok(Some(mv)) => Some((w, mv)),
            _ => None,
        });
        // The end of the line, a null move, damage or the stream's longest
        // line: the position is reached, and no move from it is counted.
        let Some((word, mv)) = played else {
            line.visit(NO_MOVE, ply, max_ply);
            return true;
        };
        line.visit(pack_move(mv), ply, max_ply);
        line.played(standard_word(word));
        ply += 1;
    }
}

/// The word a standard game's move is kept as: a Chess960 castling word,
/// which such a game may use, is the standard castling of its side.
fn standard_word(word: u16) -> u16 {
    if word < FIRST_CASTLE_960 {
        return word;
    }
    match movetable::decode(word) {
        Some(MoveWord::Castle960 { color, side, .. }) => {
            movetable::encode(MoveWord::Castle { color, side }).unwrap_or(word)
        }
        _ => word,
    }
}

/// Fills `line` with the main line of `record`'s classic game, the same way
/// as [`walk`]. The compact encoding names a move by the position it is
/// played in, so the tree is walked; the main line comes first in it, and
/// a walk stopped by damage has reported the moves before the damage. Each
/// move is kept as the 2CBH word that names it.
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
    line.start(&board);
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
    /// A main-line move announced and not yet played, with its word: it
    /// counts once it is, as an illegal move is reported before it is found
    /// to be one.
    pending: Option<(u16, u16)>,
    /// The line ended: at the stream's longest line, a null move, or the
    /// first move off the main line.
    done: bool,
}

impl TreeVisitor for MainLine<'_> {
    fn play(&mut self, before: &Board, mv: Option<Move>, main_line: bool) {
        if self.done {
            return;
        }
        let read = mv.filter(|_| main_line && self.line.reads(self.ply));
        match read.and_then(|mv| Some((pack_move(mv), replay::word_of(before, mv)?))) {
            Some(pending) => self.pending = Some(pending),
            None => {
                self.line.visit(NO_MOVE, self.ply, self.max_ply);
                self.done = true;
            }
        }
    }

    fn played(&mut self, after: &Board) {
        if let Some((mv, word)) = self.pending.take() {
            self.line.visit(mv, self.ply, self.max_ply);
            self.line.played(word);
            self.ply += 1;
            self.line.at(after, self.ply);
        }
    }

    /// Once the line is done the rest of the game is not decoded.
    fn stopped(&self) -> bool {
        self.done
    }
}

/// Fills `line` with the main line of a PGN game's text, as [`walk`] reads a
/// 2CBH game; whether the index holds the game. The moves are read as
/// written ([`pgnfile::line`]): the line ends at the first that names no
/// legal move, a null move among them. Each is kept as the 2CBH word that
/// names it.
fn walk_pgn(text: &[u8], r: &pgnfile::Record, max_ply: u8, line: &mut Line, lexer: &mut Lexer) -> bool {
    if r.is_chess960() || r.is_other_variant() {
        return false;
    }
    begin(line, r);
    let (mut ply, mut chess960) = (0u32, false);
    let end = main_line(text, lexer, &mut |board, mv| {
        if ply == 0 {
            if board.is_chess960() {
                chess960 = true;
                return false;
            }
            line.start(board);
        }
        line.at(board, ply);
        match mv.filter(|_| line.reads(ply)).and_then(|mv| Some((mv, replay::word_of(board, mv)?))) {
            Some((mv, word)) => {
                line.visit(pack_move(mv), ply, max_ply);
                line.played(word);
                ply += 1;
                true
            }
            // The end of the line, the stream's longest line, a null move or
            // a move it cannot play: the position is reached, and no move
            // from it is counted.
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
    line.rating_sum = super::ranking::rating_sum(r);
    line.date = super::ranking::date(r.played_date());
    line.positions.clear();
    line.structures.clear();
    line.beyond = None;
    line.words.clear();
    line.setup = None;
    line.start_move = 0;
    line.departures = Departures::default();
    line.structure_of = None;
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

    /// A classic game's walk keeps the 2CBH word of each main-line move, its
    /// castling as the standard castling word, and stops at the first move
    /// off the main line: the rest of the game is not decoded.
    #[test]
    fn a_classic_walk_keeps_the_words_of_its_main_line() {
        let mut work = Workspace::new().unwrap();
        work.keep_words().unwrap();
        let mut board = Board::from_fen("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1").unwrap();
        let line = &mut work.line;
        line.start(&board);
        line.at(&board, 0);
        let mut main = MainLine { line, max_ply: MAX_PLY, ply: 0, pending: None, done: false };
        let mut words = Vec::new();
        for uci in ["e1h1", "a8a1"] {
            let mv: Move = uci.parse().unwrap();
            words.push(replay::word_of(&board, mv).unwrap());
            main.play(&board, Some(mv), true);
            board.play_checked(mv).unwrap();
            main.played(&board);
        }
        assert!(!main.stopped());
        main.play(&board, Some("f1f2".parse().unwrap()), false);
        assert!(main.stopped(), "off the main line");
        assert_eq!(work.line.words, words);
        assert_eq!(replay::standard_move(words[0]), Some("e1h1".parse().unwrap()));
        assert!(work.line.setup.is_some(), "a set-up start is kept");
        assert_eq!(work.line.positions.len(), 3);
    }

    /// A Chess960 castling word in a standard game is kept as the standard
    /// castling of its side.
    #[test]
    fn chess960_castling_words_are_kept_as_standard_castling() {
        let short = movetable::encode(MoveWord::Castle960 {
            position: 518,
            color: movetable::Color::White,
            side: movetable::CastleSide::Short,
        })
        .unwrap();
        let standard =
            movetable::encode(MoveWord::Castle { color: movetable::Color::White, side: movetable::CastleSide::Short });
        assert_eq!(standard_word(short), standard.unwrap());
        assert_eq!(standard_word(1), 1);
    }
}
