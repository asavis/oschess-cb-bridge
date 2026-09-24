//! Where an index gets its games: the positions of each game's main line, as a
//! small trait that each database format implements.

use cbformat::replay::{self, start_board};
use cbformat::v2::{Database, GameResult, HEADER_RECORD_SIZE, Record, RecordKind};
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

/// A database the index can be built from.
pub trait Source: Sync {
    fn records(&self) -> u32;

    /// Calls `each` for every game of records `first..=last` that the index
    /// holds, in order: standard chess, not deleted, and not a guiding text
    /// or analysis. A game whose moves are damaged contributes the positions
    /// before the damage. Only a failed read is an error.
    fn lines(&self, first: u32, last: u32, max_ply: u8, each: &mut dyn FnMut(&Line)) -> Result<()>;

    /// A digest of header records `1..=last`: two databases whose first
    /// `last` records are the same give the same digest.
    fn digest(&self, last: u32) -> Result<u64>;
}

/// Records read at a time for [`Source::lines`]: a few megabytes of moves.
const LINES_BATCH: u32 = 4096;
/// Header records read at a time for [`Source::digest`].
const DIGEST_BATCH: u32 = 16 << 10;

impl Source for Database {
    fn records(&self) -> u32 {
        self.record_count()
    }

    fn lines(&self, first: u32, last: u32, max_ply: u8, each: &mut dyn FnMut(&Line)) -> Result<()> {
        let mut line = Line { number: 0, outcome: Outcome::Other, elo: 0, positions: Vec::new() };
        let mut next = first.max(1);
        while next <= last {
            let batch = self.batch(next, last.min(next.saturating_add(LINES_BATCH - 1)))?;
            let ids = batch.ids();
            if ids.is_empty() {
                break;
            }
            for id in ids.clone() {
                let r = batch.record(id)?;
                if r.kind() != RecordKind::Game || r.is_deleted() {
                    continue;
                }
                let data = match batch.moves_of(&r) {
                    Ok(d) => d,
                    Err(e @ Error::Io(..)) => return Err(e),
                    Err(_) => continue,
                };
                let Ok(moves) = data.moves() else { continue };
                if moves.is_chess960() {
                    continue;
                }
                let Ok(mut board) = moves.start().and_then(|s| start_board(&s)) else { continue };
                if board.is_chess960() {
                    continue;
                }
                line.number = id;
                line.outcome = outcome(&r);
                line.elo = average_elo(&r);
                line.positions.clear();
                let mut words = moves.main_line();
                let mut ply = 0u8;
                loop {
                    let key = board.hash();
                    let played = if ply < max_ply { words.next() } else { None };
                    let mv = match played.map(|w| replay::play(&mut board, w)) {
                        Some(Ok(Some(mv))) => Some(mv),
                        // The end of the line, a null move, damage or the
                        // index's depth: the position is reached, and no move
                        // from it is counted.
                        _ => None,
                    };
                    if !line.positions.iter().any(|p| p.0 == key) {
                        line.positions.push((key, mv.map_or(NO_MOVE, pack_move), ply));
                    }
                    if mv.is_none() {
                        break;
                    }
                    ply += 1;
                }
                each(&line);
            }
            next = ids.end().saturating_add(1);
            if *ids.end() == u32::MAX {
                break;
            }
        }
        Ok(())
    }

    fn digest(&self, last: u32) -> Result<u64> {
        let last = last.min(self.record_count());
        let mut buf = vec![0u8; DIGEST_BATCH as usize * HEADER_RECORD_SIZE];
        let mut hash = 0xcbf2_9ce4_8422_2325u64 ^ u64::from(last);
        let mut next = 1u32;
        while next <= last {
            let want = (last - next + 1).min(DIGEST_BATCH) as usize;
            let read = self.read_records(next, &mut buf[..want * HEADER_RECORD_SIZE])?;
            if read == 0 {
                return Err(Error::Format("header records ended early".into()));
            }
            for chunk in buf[..read as usize * HEADER_RECORD_SIZE].as_chunks::<8>().0 {
                hash = (hash ^ u64::from_le_bytes(*chunk)).wrapping_mul(0x0000_0100_0000_01b3).rotate_left(29);
            }
            next += read;
        }
        Ok(hash)
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
