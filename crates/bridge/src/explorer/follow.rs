//! A line of the move stream followed through its move words alone, without
//! a board: its position's key (`Keys`), which the tree's passes and the
//! games of a position follow, and its structure ([`Tracker`]), which the
//! deep section's passes follow. A word names the piece it moves, what it
//! takes and what a pawn becomes, and the stream's words were checked when it
//! was written, so what each word does is looked up in a table made once from
//! the move table.

use std::sync::OnceLock;

use chesscore::{Board, CastleSide, Color, Piece, Square, zobrist};

use cbformat::movetable::{self, Captured, FIRST_CASTLE_960, MoveWord};
use cbformat::replay;

use super::format::{NO_MOVE, pack_move, piece_shift, pieces_of, structure_of};
use super::stream;

/// What a move word does to a position's key, from the move table, in 16
/// bytes, one lookup a ply: the key's change (the piece off its square and
/// onto the other, the piece taken, a pawn promoted, the rook of a castling,
/// and the side to move); the pawns it moves and takes, which the deep
/// section's [`Tracker`] follows too: the square a pawn leaves (bits
/// 0-5), the square it reaches (6-11), the square of a pawn taken (12-17),
/// whether each is so (18-20), and whether black moves (21); the move as the
/// index packs it, [`NO_MOVE`] for a word that names no move of standard
/// chess; the castling rights the move ends ([`Keys::rights`]); and the
/// square a pawn steps two squares to, plus one, else 0.
#[derive(Clone, Copy, Default)]
pub(super) struct Step {
    key: u64,
    pawns: u32,
    pub(super) mv: u16,
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
pub(super) struct Keys {
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
    pub(super) fn of(board: &Board) -> Option<Keys> {
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

    pub(super) fn hash(&self) -> u64 {
        self.key ^ self.en_passant
    }

    /// The home pawns of the line's position ([`stream::home_pawns`]).
    pub(super) fn home(&self) -> u16 {
        stream::home_of(self.pawns[0], self.pawns[1])
    }

    /// The move `word` names, as the index packs it; `None` for a word that
    /// names no move of standard chess.
    pub(super) fn packed(&self, word: u16) -> Option<u16> {
        self.step(word).map(|s| s.mv)
    }

    /// Plays `word`, which [`Keys::packed`] took.
    pub(super) fn play(&mut self, word: u16) {
        let Some(&s) = self.steps.get(usize::from(word)) else { return };
        self.apply(s);
    }

    /// What `word` does, the move it names among it; `None` for a word that
    /// names no move of standard chess. One lookup gives both.
    #[inline]
    pub(super) fn step(&self, word: u16) -> Option<Step> {
        self.steps.get(usize::from(word)).copied().filter(|s| s.mv != NO_MOVE)
    }

    /// Plays step `s`.
    #[inline(always)]
    pub(super) fn apply(&mut self, s: Step) {
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

/// The keys of the standard start to follow.
pub(super) fn standard_keys() -> Keys {
    static START: OnceLock<Keys> = OnceLock::new();
    *START.get_or_init(|| Keys::of(stream::standard()).expect("the standard start castles from the corners"))
}

/// What a move word does to a structure, from the move table, so that a
/// word is played without a branch: the pawns it takes off their squares or
/// puts on theirs, white's then black's, which a xor applies; the change to
/// the pieces' counts, a piece taken or a pawn promoted, which a wrapping add
/// applies; and whether it may change the structure ([`CHANGED`]) and names
/// a move of standard chess ([`NAMED`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
struct Effect {
    pawns: [u64; 2],
    pieces: u64,
    flags: u64,
}

/// An [`Effect`] flag: a pawn's move or a capture.
const CHANGED: u64 = 1;
/// An [`Effect`] flag: a word that names a move of standard chess.
const NAMED: u64 = 2;

/// Room for the distinct effects of the move table's words, a power of two
/// so that an index needs no check: its 45,357 words below the Chess960
/// castlings have 1,282.
const EFFECTS: usize = 2048;

/// The index of each word's effect among the distinct ones, 0 for a word
/// that names no move of standard chess, and the effects: two lookups a
/// word, into tables of 2 bytes a word and 32 an effect of which 131 KB are
/// in use, which a core's cache holds, where an effect a word would take
/// 1.4 MB.
fn effects() -> (&'static [u16; 1 << 16], &'static [Effect; EFFECTS]) {
    type Tables = (Box<[u16; 1 << 16]>, Box<[Effect; EFFECTS]>);
    static TABLES: OnceLock<Tables> = OnceLock::new();
    let (of, effects) = TABLES.get_or_init(|| {
        let (mut of, mut effects) = (vec![0u16; 1 << 16], vec![Effect::default()]);
        let mut index = std::collections::HashMap::new();
        for word in 0..FIRST_CASTLE_960 {
            if let Some(e) = effect_of(word) {
                of[usize::from(word)] = *index.entry(e).or_insert_with(|| {
                    effects.push(e);
                    effects.len() as u16 - 1
                });
            }
        }
        assert!(effects.len() <= EFFECTS, "{} effects", effects.len());
        effects.resize(EFFECTS, Effect::default());
        let boxed = |v: Vec<u16>| v.into_boxed_slice().try_into().expect("a word's index each");
        (boxed(of), effects.into_boxed_slice().try_into().expect("room for every effect"))
    });
    (of, effects)
}

/// What `word` does to a structure; `None` when it names no move of standard
/// chess.
fn effect_of(word: u16) -> Option<Effect> {
    let kind = |p: movetable::Piece| match p {
        movetable::Piece::Knight => 0,
        movetable::Piece::Bishop => 1,
        movetable::Piece::Rook => 2,
        _ => 3,
    };
    match movetable::decode(word)? {
        MoveWord::Normal { color, piece, from, to, captured, promotion } => {
            let (us, them) = match color {
                movetable::Color::White => (Color::White, Color::Black),
                movetable::Color::Black => (Color::Black, Color::White),
            };
            let pawn = piece == movetable::Piece::Pawn;
            let (from, to) = (u64::from(from & 63), u64::from(to & 63));
            let mut pawns = [0; 2];
            // A pawn leaves its square, and stands on the other one unless it
            // becomes a piece there.
            if pawn {
                pawns[us.index()] = 1 << from | u64::from(promotion.is_none()) << to;
            }
            // The pawn taken en passant stands beside the one that takes it:
            // on the rank it leaves, the file it reaches.
            pawns[them.index()] = match captured {
                Captured::Pawn => 1 << to,
                Captured::EnPassant => 1 << (from & 56 | to & 7),
                _ => 0,
            };
            let taken_piece = match captured {
                Captured::Knight => Some(movetable::Piece::Knight),
                Captured::Bishop => Some(movetable::Piece::Bishop),
                Captured::Rook => Some(movetable::Piece::Rook),
                Captured::Queen => Some(movetable::Piece::Queen),
                _ => None,
            };
            let mut delta = 0i64;
            if let Some(p) = taken_piece {
                delta -= 1 << piece_shift(kind(p), them);
            }
            if let Some(p) = promotion.filter(|_| pawn) {
                delta += 1 << piece_shift(kind(p), us);
            }
            let changes = pawn || captured != Captured::Nothing;
            Some(Effect { pawns, pieces: delta as u64, flags: NAMED | if changes { CHANGED } else { 0 } })
        }
        MoveWord::Castle { .. } => Some(Effect { flags: NAMED, ..Effect::default() }),
        _ => None,
    }
}

/// The words a line's replay plays at a time past the tree's plies, noting
/// the changes of its structure ([`Tracker::play_noting`]).
pub(super) const CHANGES: usize = 64;

/// The parts of a line's structure after each word of a run that may have
/// changed it, each part in an array of its own, so that their structures
/// are hashed several at a time ([`super::format::structures_of`]).
pub(super) struct Changes {
    pub(super) white: [u64; CHANGES],
    pub(super) black: [u64; CHANGES],
    pub(super) pieces: [u64; CHANGES],
}

impl Default for Changes {
    fn default() -> Changes {
        Changes { white: [0; CHANGES], black: [0; CHANGES], pieces: [0; CHANGES] }
    }
}

/// A line's structure followed through its move words alone: each side's
/// pawns and its pieces counted by kind, as [`super::format::structure`]
/// hashes them. A word names the piece it moves, what it takes and what a
/// pawn becomes, and the stream's words were checked when it was written, so
/// no board is needed.
#[derive(Clone, Copy)]
pub struct Tracker {
    pawns: [u64; 2],
    pieces: u64,
    of: &'static [u16; 1 << 16],
    effects: &'static [Effect; EFFECTS],
}

impl PartialEq for Tracker {
    fn eq(&self, other: &Tracker) -> bool {
        (self.pawns, self.pieces) == (other.pawns, other.pieces)
    }
}

impl std::fmt::Debug for Tracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tracker").field("pawns", &self.pawns).field("pieces", &self.pieces).finish()
    }
}

impl Tracker {
    pub fn of(board: &Board) -> Tracker {
        let pieces = pieces_of(board);
        let pawns = |color| board.colored(Piece::Pawn, color);
        let (of, effects) = effects();
        Tracker { pawns: [pawns(Color::White), pawns(Color::Black)], pieces, of, effects }
    }

    /// The standard start's, counted once.
    pub fn standard() -> Tracker {
        static STANDARD: OnceLock<Tracker> = OnceLock::new();
        *STANDARD.get_or_init(|| Tracker::of(stream::standard()))
    }

    /// The effect of `word`.
    #[inline(always)]
    fn effect(&self, word: u16) -> &'static Effect {
        &self.effects[usize::from(self.of[usize::from(word)]) & (EFFECTS - 1)]
    }

    /// Plays `word`: whether the structure may have changed, which only a
    /// pawn's move or a capture does; `None` for a word that names no move
    /// of standard chess.
    #[inline]
    pub fn play(&mut self, word: u16) -> Option<bool> {
        let e = self.effect(word);
        if e.flags & NAMED == 0 {
            return None;
        }
        self.apply(e);
        Some(e.flags & CHANGED != 0)
    }

    /// Plays `words`, as [`Tracker::play`] plays each, without a branch a
    /// word: `None` when one names no move of standard chess.
    #[inline]
    pub(super) fn play_all(&mut self, words: &[[u8; 2]]) -> Option<()> {
        let mut named = NAMED;
        for w in words {
            let e = self.effect(u16::from_le_bytes(*w));
            named &= e.flags;
            self.apply(e);
        }
        (named & NAMED != 0).then_some(())
    }

    /// [`Tracker::play_noting`], not inlined, so that the few registers it
    /// plays in are all its own, when the changes' structures are hashed
    /// apart from it, in vectors.
    #[inline(never)]
    pub(super) fn play_noting_apart(&mut self, words: &[[u8; 2]], changes: &mut Changes) -> Option<usize> {
        self.play_noting(words, changes)
    }

    /// Plays `words`, [`CHANGES`] at most, and notes in `changes` the parts
    /// of the structure after each word that may have changed it, a pawn's
    /// move or a capture: without a branch a word, since which words do is
    /// unpredictable. The changes noted; `None` when a word names no move of
    /// standard chess.
    #[inline(always)]
    pub(super) fn play_noting(&mut self, words: &[[u8; 2]], changes: &mut Changes) -> Option<usize> {
        let (mut noted, mut named) = (0, NAMED);
        for w in words {
            let e = self.effect(u16::from_le_bytes(*w));
            named &= e.flags;
            self.apply(e);
            // No more noted than words played, so within `changes`.
            let at = noted & (CHANGES - 1);
            (changes.white[at], changes.black[at], changes.pieces[at]) = (self.pawns[0], self.pawns[1], self.pieces);
            noted += (e.flags & CHANGED) as usize;
        }
        (named & NAMED != 0).then_some(noted)
    }

    /// Plays effect `e`, as [`Tracker::play`] does: a pawn leaves a square it
    /// stands on and reaches one no pawn of its side stands on, and one taken
    /// stood where it is taken, as the words were checked.
    #[inline(always)]
    fn apply(&mut self, e: &Effect) {
        self.pawns[0] ^= e.pawns[0];
        self.pawns[1] ^= e.pawns[1];
        self.pieces = self.pieces.wrapping_add(e.pieces);
    }

    pub fn structure(&self) -> u64 {
        structure_of(self.pawns[0], self.pawns[1], self.pieces)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::format::structure;
    use chesscore::Move;

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

    /// Following a line's words alone gives the structure a board gives, at
    /// every ply, through captures of every kind, en passant, castling and
    /// promotions to every piece, with and without a capture, from a set-up
    /// start too; and it says so whenever the structure changes.
    #[test]
    fn a_tracked_structure_is_the_boards() {
        let lines: [(Option<&str>, &str); 4] = [
            (
                None,
                "e2e4 d7d5 e4d5 d8d5 b1c3 d5a5 d2d4 c7c6 g1f3 c8f5 f1c4 e7e6 e1h1 g8f6 c1d2 f8b4 c3e4 a5b6 \
                 e4f6 g7f6 c4b3 b8d7 d2b4 b6b4 c2c3",
            ),
            (None, "e2e4 a7a6 e4e5 d7d5 e5d6 c7d6 d1g4 c8g4 f1a6 b8a6 g1f3 d8b6 e1h1 e8c8"),
            (Some("r6r/1P4P1/8/8/2k5/8/1p4p1/R3K2R w KQ - 0 1"), "b7a8n g2h1q e1d2 b2a1r g7h8b c4b3 a8b6"),
            (Some("4k3/1P6/8/8/8/8/6p1/R3K3 b Q - 0 1"), "g2g1q e1d2 e8d7 b7b8n d7c7 a1a7"),
        ];
        for (fen, ucis) in lines {
            let mut board = fen.map_or_else(Board::startpos, |f| Board::from_fen(f).unwrap());
            let mut tracked = Tracker::of(&board);
            for uci in ucis.split_whitespace() {
                let mut mv: Move = uci.parse().unwrap();
                // Castling is the king onto its rook.
                if board.piece_at(mv.from).is_some_and(|p| p.0 == Piece::King)
                    && mv.from.file().abs_diff(mv.to.file()) == 2
                {
                    mv = Move::new(
                        mv.from,
                        chesscore::Square::new(if mv.to.file() > 4 { 7 } else { 0 }, mv.from.rank()),
                        None,
                    );
                }
                let word = cbformat::replay::word_of(&board, mv).unwrap();
                let before = structure(&board);
                board.play_checked(mv).unwrap();
                let changed = tracked.play(word).unwrap();
                assert_eq!(tracked.structure(), structure(&board), "{uci} in {ucis}");
                if structure(&board) != before {
                    assert!(changed, "{uci} changed the structure");
                }
            }
        }
        assert_eq!(Tracker::standard(), Tracker::of(&Board::startpos()));
        assert_eq!(Tracker::of(&Board::startpos()).play(0), None, "word 0 names no move");
        assert_eq!(Tracker::of(&Board::startpos()).play(movetable::NULL_MOVE), None);
        assert_eq!(Tracker::of(&Board::startpos()).play(FIRST_CASTLE_960), None);
    }

    /// Each word's effect is looked up as it is made from the word, and a
    /// word that names no move of standard chess, a Chess960 castling among
    /// them, looks up none.
    #[test]
    fn each_word_looks_up_its_effect() {
        let tracker = Tracker::of(&Board::startpos());
        let mut named = 0;
        for word in 0..=u16::MAX {
            let made = if word < FIRST_CASTLE_960 { effect_of(word) } else { None };
            named += usize::from(made.is_some());
            assert_eq!(made.unwrap_or_default(), *tracker.effect(word), "{word:#x}");
        }
        assert!(named > 40_000, "{named}");
    }

    /// Words played a run at a time note the parts after each word that may
    /// change the structure, as the words played one at a time say, and
    /// lead to the same structure played all at once, over random games long
    /// enough to promote; a run with a word that names no move fails.
    #[test]
    fn a_run_of_words_notes_each_change() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut changed = 0;
        for _ in 0..60 {
            let mut board = Board::startpos();
            let mut words = Vec::new();
            for _ in 0..300 {
                let moves = board.legal_moves();
                if moves.is_empty() {
                    break;
                }
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let mv = moves[(x % moves.len() as u64) as usize];
                words.push(cbformat::replay::word_of(&board, mv).unwrap());
                board.play_unchecked(mv);
            }
            let mut one = Tracker::of(&Board::startpos());
            let mut expected = Vec::new();
            for &w in &words {
                if one.play(w).unwrap() {
                    expected.push((one.pawns, one.pieces));
                }
            }
            let bytes: Vec<[u8; 2]> = words.iter().map(|w| w.to_le_bytes()).collect();
            let (mut run, mut noted) = (Tracker::of(&Board::startpos()), Vec::new());
            let mut changes = Changes::default();
            for words in bytes.chunks(CHANGES) {
                let n = run.play_noting(words, &mut changes).unwrap();
                let c = &changes;
                noted.extend((0..n).map(|i| ([c.white[i], c.black[i]], c.pieces[i])));
            }
            assert_eq!(noted, expected);
            assert_eq!(run, one);
            let mut all = Tracker::of(&Board::startpos());
            assert_eq!(all.play_all(&bytes), Some(()));
            assert_eq!(all, one);
            changed += noted.len();
        }
        assert!(changed > 1_000, "{changed}");
        let mut run = Tracker::of(&Board::startpos());
        let mut changes = Changes::default();
        let e2e4 = cbformat::replay::word_of(&Board::startpos(), "e2e4".parse().unwrap()).unwrap();
        assert_eq!(run.play_noting(&[e2e4.to_le_bytes(), [0, 0]], &mut changes), None);
        assert_eq!(run.play_all(&[e2e4.to_le_bytes(), FIRST_CASTLE_960.to_le_bytes()]), None);
    }
}
