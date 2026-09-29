//! The games of a position (#148), which `GET /v1/databases/{id}/games?fen=`
//! lists: every game whose main line reaches it, at any ply, once. They are
//! the games the explorer counts ([`super::stats`]), found the way it counts
//! them, as a set of record numbers that a list then sorts, searches and
//! pages ([`crate::search::select_position`]).
//!
//! - **Beyond the tree's plies**, the games of the position's structure that
//!   the explorer replays, each that reaches it kept: all of them when the
//!   tree does not hold the position, else those that first reach it beyond
//!   the tree's plies.
//! - **Within them**, the games the tree counts:
//!   - none when the tree does not hold the position;
//!   - its notable games when it has [`TOP_GAMES`] or fewer, which are then
//!     all of them;
//!   - else those a scan of the move stream's slots finds: a game from the
//!     standard start whose home pawns left in an order that allows the
//!     position (Scid's home-pawn test, [`Departures::allows`]), its first
//!     words replayed until it reaches the position, or loses a home pawn the
//!     position keeps, which never returns; a set-up game always replayed,
//!     from its start. The standard start is found at the first ply of every
//!     game from it, from its entry alone.
//!
//! The scan reads each game's slot, not the tail that its record's CRC
//! covers too, which would mean reading every game whole: it checks each
//! block of the stream's slots against the CRC of the block's number and its
//! slots instead, the first time it reads the block while the stream is
//! open, so that a slot damaged, or sound but in another record's place, is
//! never read. What it finds must also be the count the tree's record gives,
//! which its block's CRC in the index covers. A failure of either drops the
//! index to be built again, as for any damage found.
//!
//! The first scan while the stream is open also notes which games start
//! from the standard position and which from a set-up one, and the stream
//! keeps them, when the search budget has room (#142). A scan for the
//! standard start then takes the first as they are, every one of them
//! reaching it at its first ply, and replays the set-up ones alone, instead
//! of reading every slot.
//!
//! [`Departures::allows`]: super::stream::Departures::allows

use std::sync::atomic::{AtomicUsize, Ordering};

use chesscore::Board;

use crate::search::memory::Cancel;
use crate::search::workers::{self, threads};
use crate::search::{Members, Position, SearchError};

use super::Loaded;
use super::answer::{Keep, replay_with};
use super::file::Bad;
use super::format::{MAX_PLY, Stats, TOP_GAMES};
use super::stream::{Hit, Slot, Starts, Stream, Target, home_pawns};
use super::tree::{Keys, standard_keys};

/// The games of `board` in the index `loaded`, as a list narrows to them.
pub struct Games<'a> {
    pub loaded: &'a Loaded,
    pub board: &'a Board,
}

impl Position for Games<'_> {
    fn key(&self) -> u64 {
        self.board.hash()
    }

    fn games(&self, cancel: &Cancel) -> Result<Members, SearchError> {
        games(self.loaded, self.board, cancel)
    }
}

/// The games of `board` in the index `loaded`, found on the shared workers:
/// `Superseded` once `cancel` is, `Busy` when the search memory or the
/// workers have no room, and `IndexDamaged` when the index or its stream is
/// found damaged, or what it finds does not add up.
pub fn games(loaded: &Loaded, board: &Board, cancel: &Cancel) -> Result<Members, SearchError> {
    // A position the tree answers alone looks at nothing else later.
    if cancel.is_cancelled() {
        return Err(SearchError::Superseded);
    }
    let members = Members::new(loaded.records() as usize + 1)?;
    let tree = loaded.lookup(board.hash()).map_err(damaged)?;
    let within = match &tree {
        Some(stats) => within(loaded, board, stats, &members, cancel)?,
        None => 0,
    };
    let target = match tree {
        Some(_) => Target::of(board).beyond(loaded.base.header.max_ply),
        None => Target::of(board),
    };
    let beyond = replay_with(loaded, board, &target, tree.is_some(), cancel, &Listed(&members))
        .map_err(|e| match e {
            Bad::Busy if cancel.is_cancelled() => SearchError::Superseded,
            e => damaged(e),
        })?
        .into_iter()
        .sum::<u64>();
    // A game reaches the position first within the tree's plies or beyond
    // them, never both, and each is a game of the index.
    let count = members.count();
    if within.checked_add(beyond) != Some(count) || count > loaded.games() {
        return Err(SearchError::IndexDamaged);
    }
    Ok(members)
}

/// A read of the index that failed: `Busy` when the search memory had no room
/// for it, else damage.
fn damaged(e: Bad) -> SearchError {
    match e {
        Bad::Busy => SearchError::Busy,
        Bad::Io(_) | Bad::Corrupt(_) => SearchError::IndexDamaged,
    }
}

/// The games replays find beyond the tree's plies, added to the set; each
/// worker counts those it found.
struct Listed<'a>(&'a Members);

impl Keep for Listed<'_> {
    type Part = u64;
    const BYTES: usize = 0;

    fn part(&self) -> Option<u64> {
        Some(0)
    }

    fn add(&self, found: &mut u64, game: u32, _: Hit) {
        self.0.insert(game);
        *found += 1;
    }
}

/// Adds the games that reach `board` within the tree's plies to `members`,
/// whose record in the tree is `tree`: how many. Their count must be the
/// record's, else the index is damaged.
fn within(
    loaded: &Loaded,
    board: &Board,
    tree: &Stats,
    members: &Members,
    cancel: &Cancel,
) -> Result<u64, SearchError> {
    let found = if tree.counts.games <= TOP_GAMES as u64 {
        // A position of this few games keeps every one as a notable game.
        tree.top.iter().filter(|&&game| members.insert(game)).count() as u64
    } else {
        scan(&loaded.stream, board, members, cancel)?
    };
    if found != tree.counts.games {
        return Err(SearchError::IndexDamaged);
    }
    Ok(found)
}

/// The games of `stream` that reach `board` within the tree's plies, added
/// to `members`: how many. The workers take a block of the stream, 4,096
/// games, at a time, and look whether the request was superseded before
/// each. The first scan notes the starts of every game as it reads their
/// slots, and the stream keeps them once it has read every block; a scan
/// for the standard start then reads them instead ([`from_start`]).
fn scan(stream: &Stream, board: &Board, members: &Members, cancel: &Cancel) -> Result<u64, SearchError> {
    let (key, home) = (board.hash(), home_pawns(board));
    if key == standard_keys().hash()
        && home == u16::MAX
        && let Some(starts) = stream.starts()
    {
        return from_start(stream, starts, (key, home), members, cancel);
    }
    let finding = stream.find_starts();
    let blocks = stream.header.blocks as usize;
    let next = AtomicUsize::new(0);
    let found = workers::run(threads().min(blocks).max(1), 0, cancel, |w| {
        let mut found = 0u64;
        loop {
            let block = next.fetch_add(1, Ordering::Relaxed);
            if block >= blocks {
                return Ok(found);
            }
            if w.stopped() || cancel.is_cancelled() {
                return Err(SearchError::Superseded);
            }
            let (first, slots) = stream.slots(block).map_err(damaged)?;
            let mut noting = finding.as_ref().map(Starts::noting);
            for (number, slot) in (first..).zip(slots) {
                if let Some(noting) = &mut noting {
                    noting.note(number, &slot.entry);
                }
                if reaches(stream, number, &slot, key, home).map_err(damaged)? && members.insert(number) {
                    found += 1;
                }
            }
        }
    })?;
    if let Some(starts) = finding {
        stream.keep(starts);
    }
    Ok(found.iter().sum())
}

/// Words of the set-up games' numbers a worker takes at a time: 4,096 games.
const SET_UP_WORDS: usize = 64;

/// The games that reach the standard start, of key `key` and home pawns
/// `home`, within the tree's plies, added to `members` from the starts the
/// first scan found: how many. Every game from the standard start reaches it at its
/// first ply, as [`reaches`] finds from its entry alone, and the set-up games
/// are replayed, as the scan replays them, the workers taking 4,096 of them
/// at a time.
fn from_start(
    stream: &Stream,
    starts: &Starts,
    (key, home): (u64, u16),
    members: &Members,
    cancel: &Cancel,
) -> Result<u64, SearchError> {
    let standard = members.union(&starts.standard);
    let parts = starts.set_up.words().div_ceil(SET_UP_WORDS);
    let next = AtomicUsize::new(0);
    let found = workers::run(threads().min(parts).max(1), 0, cancel, |w| {
        let mut found = 0u64;
        loop {
            let part = next.fetch_add(1, Ordering::Relaxed);
            if part >= parts {
                return Ok(found);
            }
            if w.stopped() || cancel.is_cancelled() {
                return Err(SearchError::Superseded);
            }
            for number in starts.set_up.iter_in(part * SET_UP_WORDS..(part + 1) * SET_UP_WORDS) {
                if set_up_reaches(stream, number, key, home).map_err(damaged)? && members.insert(number) {
                    found += 1;
                }
            }
        }
    })?;
    Ok(standard + found.iter().sum::<u64>())
}

/// Whether record `number`, whose slot is `slot`, is a game that reaches the
/// position of key `key` and home pawns `home` within the tree's plies, as
/// the tree counts it.
fn reaches(stream: &Stream, number: u32, slot: &Slot<'_>, key: u64, home: u16) -> Result<bool, Bad> {
    let entry = slot.entry;
    if !entry.indexed() {
        return Ok(false);
    }
    if !entry.setup() {
        return match entry.departures.allows(home) {
            true => played(standard_keys(), slot.words(), key, home),
            false => Ok(false),
        };
    }
    set_up_reaches(stream, number, key, home)
}

/// Whether record `number`, a set-up game the index holds, reaches the
/// position as [`reaches`] finds.
fn set_up_reaches(stream: &Stream, number: u32, key: u64, home: u16) -> Result<bool, Bad> {
    // A set-up game's start is in its tail: its record is read whole, and
    // checked against its CRC. The Mega Database has a few thousand.
    let record = stream.record(number)?;
    let line = match record.start()? {
        Some(start) => Keys::of(&start).ok_or(Bad::Corrupt("stream start"))?,
        None => standard_keys(),
    };
    played(line, record.words(), key, home)
}

/// Whether the line from `line` whose words are `words` reaches the position
/// of key `key` and home pawns `home` within the tree's plies. A pawn never
/// comes back to its home square, so the line stops once it has lost a home
/// pawn the position keeps.
fn played(mut line: Keys, mut words: impl Iterator<Item = u16>, key: u64, home: u16) -> Result<bool, Bad> {
    for ply in 0..=MAX_PLY {
        if line.hash() == key {
            return Ok(true);
        }
        let Some(word) = words.next().filter(|_| ply < MAX_PLY) else { return Ok(false) };
        line.packed(word).ok_or(Bad::Corrupt("stream word"))?;
        line.play(word);
        if line.home() & home != home {
            return Ok(false);
        }
    }
    Ok(false)
}
