//! Where an index gets its games: the positions of each game's main line, as a
//! small trait that each database format implements.

use cbformat::replay::{self, start_board};
use cbformat::v2::{Database, GameResult, HEADER_RECORD_SIZE, MoveData, Record, RecordKind};
use cbformat::{Error, Result};

use super::format::{NO_MOVE, Outcome, pack_move};

/// One game's contribution to the index.
pub struct Line {
    pub number: u32,
    pub outcome: Outcome,
    /// The average rating of the two players, or the one known; 0 with none.
    pub elo: u16,
    /// Each position the main line reaches within the index's plies, once,
    /// with the move played from it (`NO_MOVE` at the end) and its ply.
    pub positions: Vec<(u64, u16, u8)>,
}

/// Records read at a time.
pub const RECORDS: usize = 2048;
/// The largest move record's content indexed, as `GET .../games/{number}`
/// serves (`crate::api::MAX_GAME_BYTES`). A larger one leaves its game out.
pub const MAX_MOVE_RECORD: usize = crate::api::MAX_GAME_BYTES;
/// The frame around a record's content and spare area.
const FRAME_BYTES: usize = 64;
/// The move buffer: a run's move records back to back when they fit, else one
/// record at a time.
pub const MOVE_BYTES: usize = MAX_MOVE_RECORD + FRAME_BYTES;

/// A worker's buffers for reading games, allocated once, fallibly, after the
/// worker reserved [`Workspace::BYTES`] in the search budget.
pub struct Workspace {
    headers: Vec<u8>,
    records: Vec<Record>,
    moves: Vec<u8>,
    /// The games of a run whose move records the buffer did not hold at once.
    later: Vec<u32>,
    line: Line,
    /// Games left out because their moves could not be read: damaged, or a
    /// move record over [`MAX_MOVE_RECORD`].
    pub skipped: u64,
}

impl Workspace {
    /// What a workspace takes: the buffers, and a line of at most 41 positions.
    pub const BYTES: usize =
        (RECORDS + 1) * (HEADER_RECORD_SIZE + std::mem::size_of::<Record>() + 4) + MOVE_BYTES + 1024;

    pub fn new() -> Option<Workspace> {
        let buf = |n: usize| {
            let mut v = Vec::new();
            v.try_reserve_exact(n).ok()?;
            Some(v)
        };
        let mut positions = Vec::new();
        positions.try_reserve_exact(64).ok()?;
        let mut records = Vec::new();
        records.try_reserve_exact(RECORDS + 1).ok()?;
        let mut later = Vec::new();
        later.try_reserve_exact(RECORDS).ok()?;
        Some(Workspace {
            headers: buf((RECORDS + 1) * HEADER_RECORD_SIZE)?,
            records,
            moves: buf(MOVE_BYTES)?,
            later,
            line: Line { number: 0, outcome: Outcome::Other, elo: 0, positions },
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

impl Source for Database {
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
        let last = last.min(self.record_count());
        let mut next = first.max(1);
        while next <= last {
            let want = (last - next + 1).min(RECORDS as u32) as usize;
            // One header past the run, when there is one: its move record
            // starts where the run's last one ends.
            work.headers.clear();
            work.headers.resize((want + 1) * HEADER_RECORD_SIZE, 0);
            let read = self.read_records(next, &mut work.headers)? as usize;
            if read == 0 {
                break;
            }
            let count = read.min(want);
            // Within the capacity reserved for it: nothing is allocated.
            work.records.clear();
            for (i, b) in
                work.headers[..read * HEADER_RECORD_SIZE].as_chunks::<HEADER_RECORD_SIZE>().0.iter().enumerate()
            {
                work.records.push(Record::from_bytes(next + i as u32, b));
            }
            let (run, after) = work.records.split_at(count);
            // The run's move records at once when the buffer holds them; the
            // others, one by one, after the run.
            let window = self.read_move_window(run, after.first(), &mut work.moves)?;
            work.later.clear();
            for (i, r) in run.iter().enumerate() {
                if r.kind() != RecordKind::Game || r.is_deleted() {
                    continue;
                }
                match window.and_then(|w| self.moves_in(w, &work.moves, r)) {
                    Some(Ok(data)) => {
                        if walk(&data, r, max_ply, &mut work.line) {
                            each(&work.line);
                        }
                    }
                    Some(Err(_)) => work.skipped += 1,
                    None => work.later.push(i as u32),
                }
            }
            for &i in &work.later {
                let r = &run[i as usize];
                match self.read_moves_into(r, MAX_MOVE_RECORD, &mut work.moves) {
                    Ok(data) => {
                        if walk(&data, r, max_ply, &mut work.line) {
                            each(&work.line);
                        }
                    }
                    Err(e @ Error::Io(..)) => return Err(e),
                    Err(_) => work.skipped += 1,
                }
            }
            next += count as u32;
        }
        Ok(())
    }
}

/// Fills `line` with the main line of `record`'s game; whether the index
/// holds the game.
fn walk(data: &MoveData<'_>, record: &Record, max_ply: u8, line: &mut Line) -> bool {
    let Ok(moves) = data.moves() else { return false };
    if moves.is_chess960() {
        return false;
    }
    let Ok(mut board) = moves.start().and_then(|s| start_board(&s)) else { return false };
    if board.is_chess960() {
        return false;
    }
    line.number = record.id();
    line.outcome = outcome(record);
    line.elo = average_elo(record);
    line.positions.clear();
    let mut words = moves.main_line();
    let mut ply = 0u8;
    loop {
        let key = board.hash();
        let played = if ply < max_ply { words.next() } else { None };
        let mv = match played.map(|w| replay::play(&mut board, w)) {
            Some(Ok(Some(mv))) => Some(mv),
            // The end of the line, a null move, damage or the index's depth:
            // the position is reached, and no move from it is counted.
            _ => None,
        };
        if !line.positions.iter().any(|p| p.0 == key) {
            line.positions.push((key, mv.map_or(NO_MOVE, pack_move), ply));
        }
        if mv.is_none() {
            return true;
        }
        ply += 1;
    }
}

pub fn outcome(r: &Record) -> Outcome {
    match r.result() {
        GameResult::WhiteWins | GameResult::WhiteWinsForfeit => Outcome::White,
        GameResult::Draw | GameResult::DrawForfeit => Outcome::Draw,
        GameResult::BlackWins | GameResult::BlackWinsForfeit => Outcome::Black,
        _ => Outcome::Other,
    }
}

/// The players' average rating, the one known when the other is missing, or
/// 0; at most 4095, which the index stores in 12 bits.
pub fn average_elo(r: &Record) -> u16 {
    let (w, b) = (r.white_elo().max(0) as u32, r.black_elo().max(0) as u32);
    let avg = match (w, b) {
        (0, x) | (x, 0) => x,
        _ => (w + b) / 2,
    };
    avg.min(4095) as u16
}
