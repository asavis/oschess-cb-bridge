//! The tree's passes of a build (#147): every position within the first
//! [`MAX_PLY`] plies of each game, replayed from the move stream the build
//! has just written, a range of the parts of the keys ([`part_of`]) at a
//! time, as many as the build's share of the budget holds.
//!
//! In a pass, each worker replays the games it takes, following their keys
//! through the move words alone ([`Keys`]), and keeps the entries of the
//! pass's parts in a buffer of its own. A full buffer is sorted by key and
//! each crowded position folded ([`fold`]): its best games kept whole, the
//! others counted by move and outcome. When that leaves too little room, the
//! pass ends at an earlier part for every worker, and the next pass starts
//! there. The positions that games from the standard start reach first
//! within [`SHALLOW_PLY`] plies, the most crowded, the stream pass has folded
//! already ([`Shallow`]): the passes leave them out. Then the workers take
//! the pass's parts in key order, each merging one part's entries from every
//! buffer and from those folded, adding each position up and making its
//! blocks, and write them once the parts before have been placed: the tree is
//! written once, in key order, and its blocks end where parts end, so any
//! number of passes gives the same file.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use chesscore::{Board, CastleSide, Color, Piece, Square, zobrist};

use cbformat::movetable::{self, Captured, FIRST_CASTLE_960, MoveWord};
use cbformat::replay;

use crate::indexdir::crc32_update;
use crate::search::SearchError;
use crate::search::memory::{Cancel, Hold, Refused};
use crate::search::workers::{self, threads};

use super::build::{Chunks, Counted, Out, PLANNED, Turns, corrupt, from_bad};
use super::format::{
    BLOCK_DATA, BLOCK_ENTRY, BLOCK_KEYS, Block, Counts, KEY_ENTRY, MAX_BLOCK_DATA, MAX_PLY, NO_MOVE, TOP_GAMES,
    encode_record, pack_move, part_of,
};
use super::runs::{ENTRY_BYTES, Entry, Limits, MAX_GAME, PassTime, Progress, Room, grow};
use super::stream::{self, Stream};

/// A position's run of entries this long or shorter is left as it is when a
/// buffer is folded.
const FOLD_MIN: usize = 2 * TOP_GAMES;
/// The most entries a position folds into: its notable games, and a
/// weighted entry for each move, or none, and outcome.
pub(super) const FOLD_ENTRIES: usize = TOP_GAMES + 219 * 4;
/// The positions a game from the standard start reaches first within this
/// many plies: at most 9,323 whatever the database, and the most crowded of
/// all. The stream pass folds their entries as it reads the games
/// ([`Shallow`]), and the tree's passes leave them out.
pub(super) const SHALLOW_PLY: u8 = 3;
/// The most and the fewest entries of those positions a worker of the stream
/// pass holds, folded whenever they fill its room: a worker sees nearly all
/// of them, which fold into a few dozen entries each.
pub(super) const SHALLOW_ENTRIES: usize = 1 << 20;
pub(super) const MIN_SHALLOW_ENTRIES: usize = 1 << 14;
/// The least room a worker's entries take.
const MIN_WORKER_ENTRIES: usize = 64;
/// The blocks a worker hands over at once, a block's worth, and its share of
/// those kept until their turn to be written comes.
const OUT_BYTES: usize = BLOCK_KEYS * KEY_ENTRY + MAX_BLOCK_DATA;
/// What a worker holds besides its entries: a block being made, the blocks
/// made and not yet handed over, its share of those kept, and the room a
/// crowded position folds in.
pub const WORKER_BYTES: usize = BLOCK_KEYS * KEY_ENTRY + MAX_BLOCK_DATA + 2 * OUT_BYTES + FOLD_ENTRIES * ENTRY_BYTES;

/// What a move word does to a position's key, from the move table, in 16
/// bytes, one lookup a ply: the key's change (the piece off its square and
/// onto the other, the piece taken, a pawn promoted, the rook of a castling,
/// and the side to move); the pawns it moves and takes, as the deep section's
/// [`deep::Tracker`] has them: the square a pawn leaves (bits 0-5), the
/// square it reaches (6-11), the square of a pawn taken (12-17), whether each
/// is so (18-20), and whether black moves (21); the move as the index packs
/// it, [`NO_MOVE`] for a word that names no move of standard chess; the
/// castling rights the move ends ([`Keys::rights`]); and the square a pawn
/// steps two squares to, plus one, else 0.
///
/// [`deep::Tracker`]: super::deep::Tracker
#[derive(Clone, Copy, Default)]
struct Step {
    key: u64,
    pawns: u32,
    mv: u16,
    ends: u8,
    double: u8,
}

/// The step of each word below the Chess960 castlings.
fn steps() -> &'static [Step] {
    static STEPS: OnceLock<Vec<Step>> = OnceLock::new();
    STEPS.get_or_init(|| (0..FIRST_CASTLE_960).map(step_of).collect())
}

/// The castling right of `color` on `side`, as a bit of [`Keys::rights`].
fn right(color: Color, side: CastleSide) -> u8 {
    1 << (2 * color.index() + side as usize)
}

fn step_of(word: u16) -> Step {
    let Some(mv) = replay::standard_move(word) else { return Step::default() };
    let side_of = |c: movetable::Color| if c == movetable::Color::White { Color::White } else { Color::Black };
    let back = |c: Color| if c == Color::White { 0 } else { 7 };
    let (key, pawns, ends, double) = match movetable::decode(word) {
        Some(MoveWord::Normal { color, piece, captured, promotion, .. }) => {
            let (us, them) = (side_of(color), !side_of(color));
            let piece = board_piece(piece);
            let mut key =
                zobrist::piece(piece, us, mv.from) ^ zobrist::piece(promotion.map_or(piece, board_piece), us, mv.to);
            let taken = match captured {
                Captured::Nothing => None,
                Captured::EnPassant => Some((Piece::Pawn, Square::new(mv.to.file(), mv.from.rank()))),
                Captured::Pawn => Some((Piece::Pawn, mv.to)),
                Captured::Knight => Some((Piece::Knight, mv.to)),
                Captured::Bishop => Some((Piece::Bishop, mv.to)),
                Captured::Rook => Some((Piece::Rook, mv.to)),
                Captured::Queen => Some((Piece::Queen, mv.to)),
            };
            if let Some((p, at)) = taken {
                key ^= zobrist::piece(p, them, at);
            }
            let pawn = piece == Piece::Pawn;
            let pawn_taken = taken.filter(|t| t.0 == Piece::Pawn).map(|t| t.1);
            let pawns = mv.from.index() as u32
                | (mv.to.index() as u32) << 6
                | pawn_taken.map_or(0, |at| at.index() as u32) << 12
                | u32::from(pawn) << 18
                | u32::from(pawn && promotion.is_none()) << 19
                | u32::from(pawn_taken.is_some()) << 20
                | u32::from(us == Color::Black) << 21;
            // As a board ends them: a king's move ends both of its side's,
            // a rook leaving a corner or taken on one ends that corner's.
            let corner = |c: Color, at: Square| match at.file() {
                0 if at.rank() == back(c) => right(c, CastleSide::Long),
                7 if at.rank() == back(c) => right(c, CastleSide::Short),
                _ => 0,
            };
            let mut ends = 0;
            if piece == Piece::King {
                ends |= right(us, CastleSide::Short) | right(us, CastleSide::Long);
            }
            if piece == Piece::Rook {
                ends |= corner(us, mv.from);
            }
            if captured == Captured::Rook {
                ends |= corner(them, mv.to);
            }
            let double = pawn && mv.from.rank().abs_diff(mv.to.rank()) == 2;
            (key, pawns, ends, if double { mv.to.index() as u8 + 1 } else { 0 })
        }
        Some(MoveWord::Castle { color, side }) => {
            let us = side_of(color);
            // The king takes its own rook, and both end on their files.
            let (king, rook) = match side {
                movetable::CastleSide::Short => (6, 5),
                movetable::CastleSide::Long => (2, 3),
            };
            let at = |file: u8| Square::new(file, back(us));
            let key = zobrist::piece(Piece::King, us, mv.from)
                ^ zobrist::piece(Piece::Rook, us, mv.to)
                ^ zobrist::piece(Piece::King, us, at(king))
                ^ zobrist::piece(Piece::Rook, us, at(rook));
            let pawns = u32::from(us == Color::Black) << 21;
            (key, pawns, right(us, CastleSide::Short) | right(us, CastleSide::Long), 0)
        }
        _ => return Step::default(),
    };
    Step { key: key ^ zobrist::white_to_move(), pawns, mv: pack_move(mv), ends, double }
}

fn board_piece(p: movetable::Piece) -> Piece {
    match p {
        movetable::Piece::King => Piece::King,
        movetable::Piece::Queen => Piece::Queen,
        movetable::Piece::Rook => Piece::Rook,
        movetable::Piece::Bishop => Piece::Bishop,
        movetable::Piece::Knight => Piece::Knight,
        movetable::Piece::Pawn => Piece::Pawn,
    }
}

/// A line's key followed through its move words alone, as the deep
/// section's tracker follows its structure: a word names the piece it moves,
/// what it takes and what a pawn becomes, and the stream's words were checked
/// when it was written, so no board is needed. The key is the one
/// [`Board::hash`] gives, the Polyglot key, whose en passant file counts only
/// when a pawn of the side to move stands beside the pawn that has just
/// stepped two squares: the pawns followed tell.
#[derive(Clone, Copy)]
struct Keys {
    /// The key without its en passant part, and that part, 0 for none.
    key: u64,
    en_passant: u64,
    /// The castling rights left: bit `2 * colour + side`, white first, O-O
    /// before O-O-O, as the keys have them.
    rights: u8,
    /// White's pawns, then black's.
    pawns: [u64; 2],
    steps: &'static [Step],
}

impl Keys {
    /// The key of `board` to follow; `None` for a castling rook off its
    /// corner, which a start the stream keeps never has
    /// ([`stream::board_of`]).
    fn of(board: &Board) -> Option<Keys> {
        let mut rights = 0;
        for color in [Color::White, Color::Black] {
            for (side, file) in [(CastleSide::Short, 7), (CastleSide::Long, 0)] {
                match board.castling_rook(color, side) {
                    None => {}
                    Some(f) if f == file => rights |= right(color, side),
                    Some(_) => return None,
                }
            }
        }
        let en_passant = board.en_passant().map_or(0, |sq| zobrist::en_passant(sq.file()));
        let pawns = [board.colored(Piece::Pawn, Color::White), board.colored(Piece::Pawn, Color::Black)];
        Some(Keys { key: board.hash() ^ en_passant, en_passant, rights, pawns, steps: steps() })
    }

    fn hash(&self) -> u64 {
        self.key ^ self.en_passant
    }

    /// The move `word` names, as the index packs it; `None` for a word that
    /// names no move of standard chess.
    fn packed(&self, word: u16) -> Option<u16> {
        self.steps.get(usize::from(word)).map(|s| s.mv).filter(|&mv| mv != NO_MOVE)
    }

    /// Plays `word`, which [`Keys::packed`] took.
    fn play(&mut self, word: u16) {
        let Some(&s) = self.steps.get(usize::from(word)) else { return };
        // Both sides' pawns at once, without a branch, as the deep section's
        // tracker plays them: all ones in `black` when black moves.
        let p = u64::from(s.pawns);
        let black = (p >> 21 & 1).wrapping_neg();
        let left = (p >> 18 & 1) << (p & 63);
        let reached = (p >> 19 & 1) << (p >> 6 & 63);
        let taken = (p >> 20 & 1) << (p >> 12 & 63);
        let [white_pawns, black_pawns] = self.pawns;
        self.pawns = [
            white_pawns & !(left & !black | taken & black) | reached & !black,
            black_pawns & !(left & black | taken & !black) | reached & black,
        ];
        self.key ^= s.key;
        let ended = self.rights & s.ends;
        if ended != 0 {
            for color in [Color::White, Color::Black] {
                for side in [CastleSide::Short, CastleSide::Long] {
                    if ended & right(color, side) != 0 {
                        self.key ^= zobrist::castle(color, side);
                    }
                }
            }
            self.rights &= !ended;
        }
        self.en_passant = 0;
        if s.double != 0 {
            // White steps to the fourth rank, black to the fifth; the pawns
            // of the other side beside it may take en passant.
            let to = s.double - 1;
            let them = if to < 32 { Color::Black } else { Color::White };
            let bit = 1u64 << to;
            let beside = (bit << 1 & !0x0101_0101_0101_0101) | (bit >> 1 & !0x8080_8080_8080_8080);
            if self.pawns[them.index()] & beside != 0 {
                self.en_passant = zobrist::en_passant(to & 7);
            }
        }
    }
}

/// A worker's entries of the positions [`SHALLOW_PLY`] names, folded as they
/// fill its room.
pub(super) struct Shallow {
    entries: Vec<Entry>,
    room: usize,
    scratch: Vec<Entry>,
}

impl Shallow {
    /// What a worker holds for `room` entries: them, and the room a crowded
    /// position folds in.
    pub fn bytes(room: usize) -> usize {
        (room + FOLD_ENTRIES) * ENTRY_BYTES
    }

    /// Room for `room` entries, allocated fallibly, which the worker holds in
    /// the budget first ([`Shallow::bytes`]); none for 0.
    pub fn new(room: usize) -> Option<Shallow> {
        let (mut entries, mut scratch) = (Vec::new(), Vec::new());
        if room > 0 {
            entries.try_reserve_exact(room).ok()?;
            scratch.try_reserve_exact(FOLD_ENTRIES).ok()?;
        }
        Some(Shallow { entries, room, scratch })
    }

    /// Adds `entry`; `false` once the entries no longer fold into half the
    /// room, which a database of games from the standard start comes near
    /// only with a small share of the budget, and then the build does
    /// without them.
    pub fn add(&mut self, entry: Entry) -> bool {
        if self.entries.len() >= self.room {
            self.fold();
            if self.entries.len() > self.room / 2 {
                return false;
            }
        }
        self.entries.push(entry);
        true
    }

    /// Sorts the entries and folds them.
    fn fold(&mut self) {
        self.entries.sort_unstable_by_key(|e| e.key);
        fold(&mut self.entries, &mut self.scratch);
    }

    /// Every worker's entries, each with the hold of its room, sorted and
    /// folded together: each worker's into the first one's room, which holds
    /// two, whose entries then fold into half of it again, as nearly always,
    /// the one merged giving its room back at once. The entries and their
    /// hold; `None` when they do not fold into the room, and the build does
    /// without them.
    pub fn merge(all: Vec<(Shallow, Hold)>, progress: &Progress) -> Result<Option<(Vec<Entry>, Hold)>, SearchError> {
        let mut all = all.into_iter();
        let Some((mut into, mut hold)) = all.next() else { return Ok(Some((Vec::new(), Hold::default()))) };
        into.fold();
        for (mut other, _room) in all {
            other.fold();
            if into.entries.len() + other.entries.len() > into.room {
                return Ok(None);
            }
            into.entries.extend_from_slice(&other.entries);
            into.fold();
        }
        // What the entries take, held before the room is given back.
        let len = into.entries.len();
        grow(&mut hold, len * ENTRY_BYTES, progress)?;
        let mut merged = Vec::new();
        merged.try_reserve_exact(len).map_err(|_| Refused::Busy)?;
        merged.extend_from_slice(&into.entries);
        drop(into);
        hold.shrink(len * ENTRY_BYTES);
        Ok(Some((merged, hold)))
    }
}

/// The tree as written: its positions and blocks, and where its table is.
pub(super) struct Tree {
    pub keys: u64,
    pub blocks: u32,
    pub table_offset: u64,
    pub table_crc: u32,
}

/// Writes the tree of the games in `stream` to `out`, then its table, within
/// `share` bytes of the budget: `counted` holds the entries of each part of
/// the keys, of `part_bits` bits, as the stream's pass counted them, which the
/// tree must hold as many of, those it folded, and those entries folded.
pub(super) fn write(
    stream: &Stream,
    counted: &Counted,
    part_bits: u8,
    out: &mut Out,
    progress: &Progress,
    share: usize,
    limits: &Limits,
) -> Result<Tree, SearchError> {
    let counts = &counted.entries;
    let total: u64 = counts.iter().sum();
    progress.start("positions", total);
    // The entries the passes collect: all but those the stream pass folded.
    let collected = |part: usize| counts[part] - counted.shallow[part];
    let folded = &counted.folded[..];
    // The table: a block a part, and one for every 1,024 entries more, which
    // grows as it must.
    let table_bytes = BLOCK_ENTRY * (counts.len() + (total / 1024) as usize + 16);
    let least = MIN_WORKER_ENTRIES * ENTRY_BYTES;
    // Half the workers at most, as many as the share holds beside a quarter
    // of it for the entries, one at least.
    let games = stream.header.records();
    let fit = (share.saturating_sub(table_bytes) / 4 * 3 / WORKER_BYTES).max(1);
    let want = threads().div_ceil(2).min(games.div_ceil(64) as usize).min(fit).max(1);
    let room = share.checked_sub(table_bytes + want * WORKER_BYTES).ok_or(SearchError::TooLarge)?;
    let room = room.min(limits.pass_bytes.unwrap_or(usize::MAX));
    if room < least {
        return Err(SearchError::TooLarge);
    }
    // The table's first room is reserved with the passes', so that the
    // entries never take the room the table then waits for.
    let (_memory, want, room) =
        Room { fixed: table_bytes, each: WORKER_BYTES, workers: want, least, room }.reserve(progress)?;
    let capacity = room / ENTRY_BYTES;
    let want = want.min(capacity / MIN_WORKER_ENTRIES);
    let mut table = Vec::new();
    table.try_reserve_exact(table_bytes).map_err(|_| Refused::Busy)?;
    let table_memory = Hold::default();
    let mut sink = Sink { at: out.offset, table, table_memory, keys: 0, blocks: 0, games: 0, progress };
    let mut first = 0;
    while first < counts.len() {
        // As many parts as the room holds, but for a little, one at least.
        let mut end = first;
        let mut planned = 0;
        while end < counts.len() && (end == first || planned + collected(end) <= (capacity * PLANNED / 100) as u64) {
            planned += collected(end);
            end += 1;
        }
        let hi = AtomicUsize::new(end);
        progress.tree_passes.fetch_add(1, Ordering::Relaxed);
        let pass = Pass { stream, part_bits, first, hi: &hi, capacity, planned, folded, progress };
        let started = Instant::now();
        let buffers = pass.collect(want)?;
        let replayed = Instant::now();
        let end = hi.load(Ordering::Relaxed);
        write_parts(&buffers, &pass, end, counts, &mut sink, out, want)?;
        out.sync_behind();
        let time = PassTime { replay: replayed - started, write: replayed.elapsed() };
        progress.time(|t| t.tree.push(time));
        first = end;
    }
    if sink.games != total {
        return Err(corrupt(&stream.path, "the move stream does not replay to the positions it was read with"));
    }
    out.offset = sink.at;
    let table_offset = out.offset;
    out.put(&sink.table)?;
    let blocks = u32::try_from(sink.blocks).map_err(|_| SearchError::TooLarge)?;
    Ok(Tree { keys: sink.keys, blocks, table_offset, table_crc: !crc32_update(!0, &sink.table) })
}

/// One pass: the parts of the keys from `first` to `hi`, which a worker lowers
/// when its entries do not fit.
struct Pass<'a> {
    stream: &'a Stream,
    part_bits: u8,
    first: usize,
    hi: &'a AtomicUsize,
    /// The entries all workers' buffers hold together, and those planned.
    capacity: usize,
    planned: u64,
    /// The entries of the positions that [`SHALLOW_PLY`] names, sorted and
    /// folded by the stream pass, which the pass then leaves out; none when
    /// it did without.
    folded: &'a [Entry],
    progress: &'a Progress,
}

impl Pass<'_> {
    fn part(&self, key: u64) -> usize {
        part_of(key, self.part_bits)
    }

    /// Each worker's entries of the pass's parts, sorted by key, of up to
    /// `want` workers.
    fn collect(&self, want: usize) -> Result<Vec<Vec<Entry>>, SearchError> {
        let (first, last) = (self.stream.header.first_record, self.stream.header.last_record);
        let chunks = Chunks::new(first, last, want);
        // Room for twice what a chunk adds, about.
        let spare = (2 * self.planned * chunks.size()).div_ceil(self.stream.header.records().max(1)) as usize;
        workers::run(want, 0, &Cancel::never(), |w| {
            let cap = self.capacity / w.count;
            let mut buf: Vec<Entry> = Vec::new();
            buf.try_reserve_exact(cap).map_err(|_| Refused::Busy)?;
            let mut scratch: Vec<Entry> = Vec::new();
            scratch.try_reserve_exact(FOLD_ENTRIES).map_err(|_| Refused::Busy)?;
            let mut seen = [0u64; MAX_PLY as usize + 1];
            let mut taker = chunks.taker();
            while let Some((lo, hi)) = taker.take(buf.len() + spare > cap) {
                if w.stopped() || self.progress.stop.load(Ordering::Relaxed) {
                    return Err(SearchError::Superseded);
                }
                for game in lo..=hi {
                    self.replay(game, &mut buf, cap, &mut scratch, &mut seen)?;
                }
            }
            buf.sort_unstable_by_key(|e| e.key);
            fold(&mut buf, &mut scratch);
            Ok(buf)
        })
    }

    /// Adds the entries of game `game`'s first positions that lie in the
    /// pass to `buf`: each position once, at its first visit, with the move
    /// played from there, as the walk that wrote the stream met them; but
    /// for those the stream pass folded.
    fn replay(
        &self,
        game: u32,
        buf: &mut Vec<Entry>,
        cap: usize,
        scratch: &mut Vec<Entry>,
        seen: &mut [u64; MAX_PLY as usize + 1],
    ) -> Result<(), SearchError> {
        let path = &self.stream.path;
        let record = self.stream.written(game).map_err(|e| from_bad(path, e))?;
        let entry = record.entry;
        if !entry.indexed() {
            return Ok(());
        }
        let start = record.start().map_err(|e| from_bad(path, e))?;
        let mut line = match &start {
            Some(board) => Keys::of(board).ok_or_else(|| corrupt(path, "stream start"))?,
            None => standard_keys(),
        };
        let mut words = record.words();
        // The positions seen, and a bit of each key's low six: a key whose
        // bit is clear is new, as nearly every one is.
        let mut visited = 0;
        let folded = if !self.folded.is_empty() && start.is_none() { usize::from(SHALLOW_PLY) + 1 } else { 0 };
        for ply in 0..=usize::from(MAX_PLY) {
            let key = line.hash();
            let word = words.next();
            let mv = match word {
                Some(w) => line.packed(w).ok_or_else(|| corrupt(path, "stream word"))?,
                None => NO_MOVE,
            };
            if !seen[..visited].contains(&key) {
                seen[visited] = key;
                visited += 1;
                let part = self.part(key);
                if ply >= folded && part >= self.first && part < self.hi.load(Ordering::Relaxed) {
                    if buf.len() >= cap {
                        make_room(buf, cap, scratch, self.first, self.hi, self.part_bits)?;
                    }
                    if part < self.hi.load(Ordering::Relaxed) {
                        buf.push(Entry::new(key, game, entry.outcome(), mv, entry.elo()));
                    }
                }
            }
            match word {
                Some(w) if ply < usize::from(MAX_PLY) => line.play(w),
                _ => break,
            }
        }
        Ok(())
    }
}

/// The keys of the standard start to follow.
fn standard_keys() -> Keys {
    static START: OnceLock<Keys> = OnceLock::new();
    *START.get_or_init(|| Keys::of(stream::standard()).expect("the standard start castles from the corners"))
}

/// Makes room in `buf`, a worker's full buffer of `cap` entries in a pass of
/// the parts from `first` to `hi`, of `part_bits` bits: its entries sorted
/// and folded, and when they still take three quarters of it, the pass ended
/// for every worker at the part that keeps about half. A first part that
/// alone leaves no room is too large for the share.
fn make_room(
    buf: &mut Vec<Entry>,
    cap: usize,
    scratch: &mut Vec<Entry>,
    first: usize,
    hi: &AtomicUsize,
    part_bits: u8,
) -> Result<(), SearchError> {
    let part = |e: &Entry| part_of(e.key, part_bits);
    buf.sort_unstable_by_key(|e| e.key);
    let end = hi.load(Ordering::Relaxed);
    buf.truncate(buf.partition_point(|e| part(e) < end));
    fold(buf, scratch);
    if buf.len() > cap / 4 * 3 {
        let cut = part(&buf[cap / 2]).max(first + 1);
        hi.fetch_min(cut, Ordering::Relaxed);
        buf.truncate(buf.partition_point(|e| part(e) < cut));
        if buf.len() > cap - cap / 8 {
            return Err(SearchError::TooLarge);
        }
    }
    Ok(())
}

/// Folds each position whose run of entries in the sorted `buf` is longer
/// than [`FOLD_MIN`]: its [`TOP_GAMES`] best games are kept whole, and every
/// other entry is counted into a weighted entry of its move and outcome, so
/// that a position however crowded takes a few hundred entries at most. The
/// position adds up to what it did. `scratch` holds [`FOLD_ENTRIES`].
fn fold(buf: &mut Vec<Entry>, scratch: &mut Vec<Entry>) {
    let n = buf.len();
    let (mut to, mut i) = (0, 0);
    while i < n {
        let key = buf[i].key;
        let mut j = i + 1;
        while j < n && buf[j].key == key {
            j += 1;
        }
        if j - i <= FOLD_MIN {
            buf.copy_within(i..j, to);
            to += j - i;
            i = j;
            continue;
        }
        // The best games whole, best first, then the weighted entries.
        scratch.clear();
        let mut best = 0;
        let rank = |e: &Entry| (e.elo(), e.game());
        for &e in &buf[i..j] {
            let folded = if e.is_weighted() {
                Some(e)
            } else if best < TOP_GAMES {
                let at = scratch[..best].partition_point(|b| rank(b) > rank(&e));
                scratch.insert(at, e);
                best += 1;
                None
            } else if rank(&e) > rank(&scratch[best - 1]) {
                let worst = scratch.remove(best - 1);
                let at = scratch[..best - 1].partition_point(|b| rank(b) > rank(&e));
                scratch.insert(at, e);
                Some(worst)
            } else {
                Some(e)
            };
            if let Some(f) = folded {
                match scratch[best..].iter_mut().find(|w| w.mv() == f.mv() && w.outcome() == f.outcome()) {
                    Some(w) => {
                        let games = (w.games() + f.games()).min(u64::from(MAX_GAME)) as u32;
                        *w = Entry::weighted(key, games, f.outcome(), f.mv());
                    }
                    None => scratch.push(Entry::weighted(key, f.games() as u32, f.outcome(), f.mv())),
                }
            }
        }
        // Never more than the run: each weighted entry folds one at least.
        buf[to..to + scratch.len()].copy_from_slice(scratch);
        to += scratch.len();
        i = j;
    }
    buf.truncate(to);
}

/// Where the tree's parts go, one part after another.
struct Sink<'a> {
    /// Where the next part's bytes go.
    at: u64,
    table: Vec<u8>,
    /// What the table holds beyond its first room, which the passes' hold
    /// holds.
    table_memory: Hold,
    keys: u64,
    blocks: u64,
    /// The games of the positions placed, which add up to the entries.
    games: u64,
    progress: &'a Progress,
}

impl Sink<'_> {
    /// Places the blocks `made` after those placed, and adds them to the
    /// table: their bytes, and where they go.
    fn place(&mut self, made: Made) -> Result<(u64, Vec<u8>), SearchError> {
        let base = self.at;
        self.at += made.out.len() as u64;
        let more = made.table.len() * BLOCK_ENTRY;
        if self.table.len() + more > self.table.capacity() {
            let step = more.max(64 << 10);
            grow(&mut self.table_memory, step, self.progress)?;
            self.table.try_reserve_exact(step).map_err(|_| Refused::Busy)?;
        }
        for b in made.table {
            Block { offset: base + b.offset, ..b }.encode(&mut self.table);
        }
        self.blocks += (more / BLOCK_ENTRY) as u64;
        self.keys += made.keys;
        self.games += made.games;
        self.progress.positions.fetch_add(made.keys, Ordering::Relaxed);
        self.progress.done.fetch_add(made.done, Ordering::Relaxed);
        Ok((base, made.out))
    }
}

/// Blocks of a part, made and handed over: their bytes and their table at
/// offsets from the first of them, the positions and the games they count,
/// and the entries of the part when they end it.
struct Made {
    out: Vec<u8>,
    table: Vec<Block>,
    keys: u64,
    games: u64,
    done: u64,
}

/// Writes parts `pass.first..end` of the sorted `buffers` and of the
/// entries the stream pass folded on up to `want` workers, in key order.
fn write_parts(
    buffers: &[Vec<Entry>],
    pass: &Pass<'_>,
    end: usize,
    counts: &[u64],
    sink: &mut Sink<'_>,
    out: &Out,
    want: usize,
) -> Result<(), SearchError> {
    let first = pass.first;
    let place = |sink: &mut &mut Sink<'_>, made: Made| sink.place(made);
    let write = |(at, bytes): (u64, Vec<u8>)| out.write(at, &bytes);
    let turns = Turns::new(end - first, want * OUT_BYTES, sink, &place, &write);
    let progress = pass.progress;
    workers::run(want, 0, &Cancel::never(), |w| {
        let stopped = || w.stopped() || progress.stop.load(Ordering::Relaxed);
        let mut made = Blocks::new().ok_or(Refused::Busy)?;
        let mut agg = Aggregate::new().ok_or(Refused::Busy)?;
        let mut heads: Vec<&[Entry]> = Vec::new();
        heads.try_reserve_exact(buffers.len() + 1).map_err(|_| Refused::Busy)?;
        while let Some(unit) = turns.take() {
            if stopped() {
                return Err(SearchError::Superseded);
            }
            let part = first + unit;
            heads.clear();
            for b in buffers.iter().map(|b| &b[..]).chain([pass.folded]) {
                let from = b.partition_point(|e| pass.part(e.key) < part);
                let to = from + b[from..].partition_point(|e| pass.part(e.key) == part);
                if to > from {
                    heads.push(&b[from..to]);
                }
            }
            // The least key of the heads, and all its entries from each.
            while let Some(key) = heads.iter().filter_map(|h| h.first()).map(|e| e.key).min() {
                for h in heads.iter_mut() {
                    let n = h.iter().take_while(|e| e.key == key).count();
                    for e in &h[..n] {
                        agg.add(e);
                    }
                    *h = &h[n..];
                }
                agg.emit(key, &mut made)?;
                if made.out.len() >= OUT_BYTES / 2 {
                    let bytes = made.out.len();
                    turns.put(unit, made.hand(0), bytes, false, &stopped)?;
                }
            }
            made.end_block();
            let bytes = made.out.len();
            turns.put(unit, made.hand(counts[part]), bytes, true, &stopped)?;
        }
        Ok(())
    })?;
    Ok(())
}

/// One position's entries, added up as a part's merge passes them.
struct Aggregate {
    count: Counts,
    moves: Vec<(u16, Counts)>,
    /// The best games so far, best first: (rating, game).
    top: Vec<(u16, u32)>,
}

impl Aggregate {
    fn new() -> Option<Aggregate> {
        let mut moves = Vec::new();
        moves.try_reserve_exact(256).ok()?;
        let mut top = Vec::new();
        top.try_reserve_exact(TOP_GAMES + 1).ok()?;
        Some(Aggregate { count: Counts::default(), moves, top })
    }

    fn add(&mut self, e: &Entry) {
        let (outcome, games) = (e.outcome(), e.games());
        self.count.add_games(outcome, games);
        if e.mv() != NO_MOVE {
            match self.moves.iter_mut().find(|m| m.0 == e.mv()) {
                Some(m) => m.1.add_games(outcome, games),
                None => {
                    let mut c = Counts::default();
                    c.add_games(outcome, games);
                    self.moves.push((e.mv(), c));
                }
            }
        }
        if e.is_weighted() {
            return;
        }
        // Higher rating first; among equal ratings, the later game.
        let item = (e.elo(), e.game());
        let at = self.top.partition_point(|&t| t > item);
        if at < TOP_GAMES {
            self.top.insert(at, item);
            self.top.truncate(TOP_GAMES);
        }
    }

    /// Writes the position `key` to `made`, its moves most played first,
    /// and starts afresh.
    fn emit(&mut self, key: u64, made: &mut Blocks) -> Result<(), SearchError> {
        self.moves.sort_unstable_by(|a, b| b.1.games.cmp(&a.1.games).then(a.0.cmp(&b.0)));
        made.push(key, &self.count, &self.moves, &self.top)?;
        made.games += self.count.games;
        self.count = Counts::default();
        self.moves.clear();
        self.top.clear();
        Ok(())
    }
}

/// The blocks of a part being made: a block's keys and records, and the
/// blocks made and not yet handed over, with their table at offsets from the
/// first of them.
struct Blocks {
    keys: Vec<u8>,
    data: Vec<u8>,
    first_key: u64,
    in_block: usize,
    out: Vec<u8>,
    table: Vec<Block>,
    keys_made: u64,
    games: u64,
}

impl Blocks {
    fn new() -> Option<Blocks> {
        fn buf<T>(n: usize) -> Option<Vec<T>> {
            let mut v = Vec::new();
            v.try_reserve_exact(n).ok()?;
            Some(v)
        }
        Some(Blocks {
            keys: buf(BLOCK_KEYS * KEY_ENTRY)?,
            data: buf(MAX_BLOCK_DATA)?,
            first_key: 0,
            in_block: 0,
            out: Vec::new(),
            table: Vec::new(),
            keys_made: 0,
            games: 0,
        })
    }

    /// The blocks made, handed over, which end their part with its `done`
    /// entries, or 0.
    fn hand(&mut self, done: u64) -> Made {
        Made {
            out: std::mem::take(&mut self.out),
            table: std::mem::take(&mut self.table),
            keys: std::mem::take(&mut self.keys_made),
            games: std::mem::take(&mut self.games),
            done,
        }
    }

    fn push(
        &mut self,
        key: u64,
        counts: &Counts,
        moves: &[(u16, Counts)],
        top: &[(u16, u32)],
    ) -> Result<(), SearchError> {
        if self.in_block == 0 {
            self.first_key = key;
        }
        let at = u32::try_from(self.data.len()).map_err(|_| SearchError::TooLarge)?;
        self.keys.extend(key.to_le_bytes());
        self.keys.extend(at.to_le_bytes());
        encode_record(&mut self.data, counts, moves, top.iter().map(|t| t.1));
        self.in_block += 1;
        self.keys_made += 1;
        if self.in_block == BLOCK_KEYS || self.data.len() >= BLOCK_DATA {
            self.end_block();
        }
        Ok(())
    }

    /// Ends the block being made, if any: a block ends with its part.
    fn end_block(&mut self) {
        if self.in_block == 0 {
            return;
        }
        let crc = !crc32_update(crc32_update(!0, &self.keys), &self.data);
        self.table.push(Block {
            first_key: self.first_key,
            offset: self.out.len() as u64,
            keys: self.in_block as u32,
            data_len: self.data.len() as u32,
            crc,
        });
        self.out.extend_from_slice(&self.keys);
        self.out.extend_from_slice(&self.data);
        self.keys.clear();
        self.data.clear();
        self.in_block = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::format::Outcome;

    /// A position's counts, its moves' counts, by move, and its best games.
    type Added = (Counts, Vec<(u16, Counts)>, Vec<(u16, u32)>);

    /// What `entries` of one position add up to.
    fn added(entries: &[Entry]) -> Added {
        let mut a = Aggregate::new().unwrap();
        for e in entries {
            a.add(e);
        }
        a.moves.sort_unstable_by_key(|m| m.0);
        (a.count, a.moves, a.top)
    }

    #[test]
    fn a_folded_position_adds_up_to_the_same() {
        let outcomes = [Outcome::White, Outcome::Draw, Outcome::Black, Outcome::Other];
        let mut all = Vec::new();
        for g in 1..=5_000u32 {
            let mv = if g % 11 == 0 { NO_MOVE } else { (g % 7) as u16 + 1 };
            all.push(Entry::new(9, g, outcomes[g as usize % 4], mv, (g * 7919 % 3000) as u16));
        }
        // A position of few entries beside it stays as it is.
        let few: Vec<Entry> = (1..=5u32).map(|g| Entry::new(10, g, Outcome::Draw, 3, 100)).collect();
        let mut buf: Vec<Entry> = all.iter().chain(&few).copied().collect();
        let mut scratch = Vec::with_capacity(FOLD_ENTRIES);
        fold(&mut buf, &mut scratch);
        assert!(buf.len() <= TOP_GAMES + 8 * 4 + few.len(), "{} entries", buf.len());
        assert_eq!(&buf[buf.len() - few.len()..], &few[..]);
        let folded: Vec<Entry> = buf.iter().filter(|e| e.key == 9).copied().collect();
        assert_eq!(added(&folded), added(&all));
        // Folded again with more of the same position, as a buffer that
        // fills again is.
        let more: Vec<Entry> = (5_001..=6_000u32).map(|g| Entry::new(9, g, Outcome::White, 2, 4000)).collect();
        let mut again: Vec<Entry> = folded.iter().chain(&more).copied().collect();
        fold(&mut again, &mut scratch);
        let everything: Vec<Entry> = all.iter().chain(&more).copied().collect();
        assert_eq!(added(&again), added(&everything));
        assert_eq!(added(&again).2.len(), TOP_GAMES);
        assert!(added(&again).2.iter().all(|t| t.0 == 4000), "the best games are the later ones");
    }

    /// A full buffer folds a crowded position and goes on; one of many
    /// positions ends the pass for every worker at the part that keeps about
    /// half of it, below where another worker ended it already; a first part
    /// that alone fills it is too large.
    #[test]
    fn a_full_buffer_folds_then_ends_the_pass_earlier() {
        let bits = 4;
        let key = |part: u64, i: u64| part << 60 | i;
        let mut scratch = Vec::with_capacity(FOLD_ENTRIES);
        let hi = AtomicUsize::new(16);
        // Part 3's start, reached by a thousand games, and a few others.
        let mut buf: Vec<Entry> = (1..=1_000).map(|g| Entry::new(key(3, 0), g, Outcome::Draw, 5, 2000)).collect();
        buf.extend((0..24).map(|i| Entry::new(key(9, i), 1, Outcome::White, 5, 2000)));
        make_room(&mut buf, 1_024, &mut scratch, 2, &hi, bits).unwrap();
        assert_eq!((buf.len(), hi.load(Ordering::Relaxed)), (TOP_GAMES + 1 + 24, 16), "folded, the pass as it was");
        // Distinct positions of parts 2 to 9: cut at the part of the middle one.
        let mut buf: Vec<Entry> =
            (0..1_024).map(|i| Entry::new(key(2 + i / 128, i), 1, Outcome::White, 5, 2000)).collect();
        make_room(&mut buf, 1_024, &mut scratch, 2, &hi, bits).unwrap();
        assert_eq!(hi.load(Ordering::Relaxed), 6);
        assert_eq!(buf.len(), 512, "parts 2 to 5 kept");
        assert!(buf.iter().all(|e| part_of(e.key, bits) < 6));
        // Another worker, whose part 7 lies beyond where the pass ends now.
        let mut other: Vec<Entry> =
            (0..1_024).map(|i| Entry::new(key(2 + i / 200, i), 1, Outcome::White, 5, 2000)).collect();
        make_room(&mut other, 1_024, &mut scratch, 2, &hi, bits).unwrap();
        assert!(hi.load(Ordering::Relaxed) <= 6 && other.iter().all(|e| part_of(e.key, bits) < 6));
        // Part 2 alone fills the buffer with distinct positions.
        let mut full: Vec<Entry> = (0..1_024).map(|i| Entry::new(key(2, i), 1, Outcome::White, 5, 2000)).collect();
        assert!(matches!(make_room(&mut full, 1_024, &mut scratch, 2, &hi, bits), Err(SearchError::TooLarge)));
    }

    /// Following a line's words alone gives the key a board gives, and the
    /// move the index packs, at every ply: through castling on both sides,
    /// captures of every kind, en passant with and without a pawn beside to
    /// take, promotions, rooks leaving and taken on their corners, from the
    /// standard start and from set-up ones, one with an en passant capture
    /// to play first, over many random games.
    #[test]
    fn a_followed_key_is_the_boards() {
        let starts = [
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
            "r3k2r/pppq1ppp/2n1bn2/3pp3/1b1PP3/2N1BN2/PPPQ1PPP/R3K2R w KQkq - 0 1",
            "r3k2r/1P4P1/8/8/8/8/1p4p1/R3K2R w KQkq - 0 1",
            "4k3/8/8/8/3pP3/8/8/4K3 b - e3 0 1",
            "r3k2r/8/8/8/8/8/8/R3K2R b KQkq - 0 1",
        ];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        // Plies, then those with an en passant part, a castling, a promotion,
        // an en passant capture, and castling rights ended.
        let mut seen = [0u32; 6];
        for fen in starts {
            let start = Board::from_fen(fen).unwrap();
            for _ in 0..400 {
                let mut board = start.clone();
                let mut keys = Keys::of(&board).unwrap();
                assert_eq!(keys.hash(), board.hash(), "{fen}");
                for _ in 0..160 {
                    let moves = board.legal_moves();
                    if moves.is_empty() {
                        break;
                    }
                    // Pawns' double steps, castling and captures often.
                    let pick = moves
                        .iter()
                        .copied()
                        .find(|m| {
                            next() % 3 == 0
                                && (m.from.rank().abs_diff(m.to.rank()) == 2 || board.piece_at(m.to).is_some())
                        })
                        .unwrap_or_else(|| moves[(next() % moves.len() as u64) as usize]);
                    let word = replay::word_of(&board, pick).unwrap();
                    assert_eq!(keys.packed(word), Some(pack_move(pick)));
                    let castles = board.piece_at(pick.to).is_some_and(|p| p.1 == board.side_to_move());
                    let en_passant =
                        board.en_passant() == Some(pick.to) && board.piece_at(pick.from).unwrap().0 == Piece::Pawn;
                    let rights = keys.rights;
                    board.play_checked(pick).unwrap();
                    keys.play(word);
                    assert_eq!(keys.hash(), board.hash(), "{pick} from {fen}");
                    let noted = [
                        true,
                        keys.en_passant != 0,
                        castles,
                        pick.promotion.is_some(),
                        en_passant,
                        keys.rights != rights,
                    ];
                    for (n, noted) in seen.iter_mut().zip(noted) {
                        *n += u32::from(noted);
                    }
                }
            }
        }
        assert!(seen[0] > 100_000 && seen.iter().all(|&n| n > 50), "{seen:?}");
        // A word that names no move of standard chess.
        assert_eq!(standard_keys().packed(0), None);
        assert_eq!(standard_keys().packed(movetable::NULL_MOVE), None);
        assert_eq!(standard_keys().packed(FIRST_CASTLE_960), None);
        // A castling rook off its corner is no start of the stream.
        let rooks = Board::from_fen("4k3/8/8/8/8/8/8/1R2K3 w - - 0 1").unwrap();
        let mut odd = chesscore::BoardBuilder::empty();
        for (i, square) in odd.squares.iter_mut().enumerate() {
            *square = Square::from_index(i as u8).and_then(|sq| rooks.piece_at(sq));
        }
        odd.castling[0][1] = Some(1);
        odd.chess960 = true;
        assert!(Keys::of(&odd.build().unwrap()).is_none());
    }

    /// A worker's entries of the shallow positions fold into its room as
    /// they come, then together with the other workers', sorted, and add up
    /// to what they did. A worker whose entries no longer fold into half its
    /// room gives them up, and so do workers whose entries together do not
    /// fit one room.
    #[test]
    fn shallow_entries_fold_as_they_come() {
        let outcomes = [Outcome::White, Outcome::Draw, Outcome::Black, Outcome::Other];
        let (room, progress) = (1_024, Progress::default());
        let (mut workers, mut all) = (Vec::new(), Vec::new());
        for w in 0..3u32 {
            let mut shallow = Shallow::new(room).unwrap();
            for g in 1..=3_000u32 {
                let game = w * 3_000 + g;
                let e = Entry::new(u64::from(game % 5), game, outcomes[game as usize % 4], (game % 7) as u16 + 1, 2000);
                assert!(shallow.add(e));
                all.push(e);
            }
            workers.push((shallow, Hold::default()));
        }
        let (merged, hold) = Shallow::merge(workers, &progress).unwrap().unwrap();
        assert!(merged.windows(2).all(|w| w[0].key <= w[1].key));
        assert_eq!(hold.bytes(), merged.len() * ENTRY_BYTES);
        for key in 0..5 {
            let of = |entries: &[Entry]| added(&entries.iter().filter(|e| e.key == key).copied().collect::<Vec<_>>());
            assert_eq!(of(&merged), of(&all), "position {key}");
        }
        // Distinct positions fill half the room.
        let mut shallow = Shallow::new(room).unwrap();
        assert!((0..2 * room as u64).any(|k| !shallow.add(Entry::new(k, 1, Outcome::Draw, 3, 100))));
        // Two workers' distinct positions, half a room each, do not fit one.
        let workers = (0..2u64)
            .map(|w| {
                let mut shallow = Shallow::new(room).unwrap();
                for k in 0..room as u64 / 2 + 1 {
                    assert!(shallow.add(Entry::new(w << 32 | k, 1, Outcome::Draw, 3, 100)));
                }
                (shallow, Hold::default())
            })
            .collect();
        assert!(Shallow::merge(workers, &progress).unwrap().is_none());
        assert!(Shallow::merge(Vec::new(), &progress).unwrap().unwrap().0.is_empty());
    }

    #[test]
    fn a_position_keeps_its_best_games_and_counts_each_move() {
        let mut a = Aggregate::new().unwrap();
        for g in 1..=20u32 {
            let outcome = [Outcome::White, Outcome::Draw, Outcome::Black, Outcome::Other][g as usize % 4];
            a.add(&Entry::new(9, g, outcome, if g % 2 == 0 { 70 } else { 71 }, (g * 100) as u16));
        }
        a.add(&Entry::weighted(9, 1_000, Outcome::White, 70));
        assert_eq!(a.count, Counts { games: 1_020, white: 1_005, draws: 5, black: 5 });
        assert_eq!(a.top.len(), TOP_GAMES);
        assert_eq!(a.top[0], (2000, 20));
        assert_eq!(a.moves.iter().map(|m| m.1.games).sum::<u64>(), 1_020);
    }
}
