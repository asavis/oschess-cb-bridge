//! The position: pieces, side to move, castling rights, en passant, clocks
//! and the Polyglot hash.

use std::fmt;

use crate::attacks;
use crate::types::{Bitboard, Color, Move, Piece, Square, squares};
use crate::zobrist::{CASTLE, EN_PASSANT, KEYS, PIECE, TURN};

/// The two castling sides. Short castling ends with the king on the g-file,
/// long castling on the c-file, in standard chess and Chess960 alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CastleSide {
    Short,
    Long,
}

impl CastleSide {
    pub const ALL: [CastleSide; 2] = [CastleSide::Short, CastleSide::Long];

    #[inline]
    const fn index(self) -> usize {
        self as usize
    }

    /// The files the king and the rook end on.
    const fn destinations(self) -> (u8, u8) {
        match self {
            CastleSide::Short => (6, 5),
            CastleSide::Long => (2, 3),
        }
    }
}

/// Why a move was refused by [`Board::play_checked`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IllegalMove {
    /// No piece of the side to move on the origin.
    NoPiece,
    /// The piece cannot move that way, or the path is blocked.
    Unreachable,
    /// The destination holds a piece of the mover's colour or a king.
    Occupied,
    /// A promotion is missing, misplaced or to an impossible piece.
    Promotion,
    /// Castling without the right, with a blocked path, or through check.
    Castling,
    /// The move leaves the mover's king attacked.
    LeavesKingInCheck,
}

impl fmt::Display for IllegalMove {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            IllegalMove::NoPiece => "no piece of the side to move on the origin",
            IllegalMove::Unreachable => "the piece cannot reach the destination",
            IllegalMove::Occupied => "the destination is occupied by a friendly piece or a king",
            IllegalMove::Promotion => "invalid promotion",
            IllegalMove::Castling => "castling is not allowed",
            IllegalMove::LeavesKingInCheck => "the move leaves the king in check",
        })
    }
}

impl std::error::Error for IllegalMove {}

/// A chess position.
///
/// A `Board` built by this crate always has exactly one king of each colour
/// and the side not to move is never in check; every constructor checks it and
/// every move keeps it.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Board {
    pieces: [Bitboard; 6],
    colors: [Bitboard; 2],
    /// The piece on each square.
    mailbox: [Option<(Piece, Color)>; 64],
    side: Color,
    /// The file of the rook each castling right belongs to, by colour and side.
    castling: [[Option<u8>; 2]; 2],
    /// The file of the last double pawn step, whether or not a capture is possible.
    ep_file: Option<u8>,
    halfmove: u16,
    fullmove: u16,
    /// The Polyglot key without its en passant part, which depends on whether
    /// a capture is possible and is added by [`Board::hash`].
    key: u64,
    /// The pieces giving check to the side to move, kept up to date by every
    /// move so that legality needs no full attack scan.
    checkers: Bitboard,
    chess960: bool,
}

#[inline]
fn piece_key(piece: Piece, color: Color, sq: Square) -> u64 {
    let kind = 2 * piece.index() + usize::from(color == Color::White);
    KEYS[PIECE + 64 * kind + sq.index()]
}

#[inline]
fn castle_key(color: Color, side: CastleSide) -> u64 {
    KEYS[CASTLE + 2 * color.index() + side.index()]
}

// Per piece kind (pawn, knight, bishop, rook, queen, king): all ones when it
// has the property, for branch-free selection.
const MOVES_STRAIGHT: [Bitboard; 6] = [0, 0, 0, !0, !0, 0];
const MOVES_DIAGONAL: [Bitboard; 6] = [0, 0, !0, 0, !0, 0];
const IS_KNIGHT: [Bitboard; 6] = [0, !0, 0, 0, 0, 0];
const IS_PAWN: [Bitboard; 6] = [!0, 0, 0, 0, 0, 0];

impl Board {
    /// A board with nothing on it, white to move. Not a valid position until
    /// pieces are added; used by the constructors.
    pub(crate) fn empty() -> Board {
        Board {
            pieces: [0; 6],
            colors: [0; 2],
            mailbox: [None; 64],
            side: Color::White,
            castling: [[None; 2]; 2],
            ep_file: None,
            halfmove: 0,
            fullmove: 1,
            key: KEYS[TURN],
            checkers: 0,
            chess960: false,
        }
    }

    /// The standard start position.
    pub fn startpos() -> Board {
        const BACK: [Piece; 8] = [
            Piece::Rook,
            Piece::Knight,
            Piece::Bishop,
            Piece::Queen,
            Piece::King,
            Piece::Bishop,
            Piece::Knight,
            Piece::Rook,
        ];
        Board::from_back_rank(BACK, false)
    }

    /// A start position with the given back rank, both sides, full castling
    /// rights on the outermost rooks.
    pub(crate) fn from_back_rank(back: [Piece; 8], chess960: bool) -> Board {
        let mut b = Board::empty();
        for (file, &piece) in back.iter().enumerate() {
            let file = file as u8;
            b.put(Square::new(file, 0), piece, Color::White);
            b.put(Square::new(file, 1), Piece::Pawn, Color::White);
            b.put(Square::new(file, 6), Piece::Pawn, Color::Black);
            b.put(Square::new(file, 7), piece, Color::Black);
        }
        let king = back.iter().position(|&p| p == Piece::King).unwrap_or(4) as u8;
        let rooks: Vec<u8> = (0..8u8).filter(|&f| back[f as usize] == Piece::Rook).collect();
        for color in Color::ALL {
            b.set_castling(color, CastleSide::Short, rooks.iter().copied().filter(|&f| f > king).max());
            b.set_castling(color, CastleSide::Long, rooks.iter().copied().filter(|&f| f < king).min());
        }
        b.chess960 = chess960;
        b
    }

    // ------------------------------------------------------------ queries

    #[inline]
    pub fn side_to_move(&self) -> Color {
        self.side
    }

    #[inline]
    pub fn piece_at(&self, sq: Square) -> Option<(Piece, Color)> {
        self.mailbox[sq.index()]
    }

    #[inline]
    pub fn pieces(&self, piece: Piece) -> Bitboard {
        self.pieces[piece.index()]
    }

    #[inline]
    pub fn colors(&self, color: Color) -> Bitboard {
        self.colors[color.index()]
    }

    #[inline]
    pub fn colored(&self, piece: Piece, color: Color) -> Bitboard {
        self.pieces[piece.index()] & self.colors[color.index()]
    }

    #[inline]
    pub fn occupied(&self) -> Bitboard {
        self.colors[0] | self.colors[1]
    }

    /// The square of `color`'s king.
    #[inline]
    pub fn king(&self, color: Color) -> Square {
        Square::at(self.colored(Piece::King, color).trailing_zeros())
    }

    /// The file of the rook `color` may castle with on `side`, if any.
    #[inline]
    pub fn castling_rook(&self, color: Color, side: CastleSide) -> Option<u8> {
        self.castling[color.index()][side.index()]
    }

    #[inline]
    pub fn halfmove_clock(&self) -> u16 {
        self.halfmove
    }

    #[inline]
    pub fn fullmove_number(&self) -> u16 {
        self.fullmove
    }

    /// Whether castling is written and parsed in the Chess960 form.
    #[inline]
    pub fn is_chess960(&self) -> bool {
        self.chess960
    }

    /// The en passant target square, when a pawn of the side to move stands
    /// beside the pawn that just made a double step. This is the Polyglot
    /// condition: the capture is possible pseudo-legally, perhaps not legally.
    pub fn en_passant(&self) -> Option<Square> {
        let file = self.ep_file?;
        let target = Square::new(file, if self.side == Color::White { 5 } else { 2 });
        // A pawn of the side to move on either side of the pawn that moved:
        // exactly the squares an enemy pawn on the target square would attack.
        (attacks::pawn(!self.side, target) & self.colored(Piece::Pawn, self.side) != 0).then_some(target)
    }

    /// The file of the last double pawn step, whether or not a capture is possible.
    #[inline]
    pub fn en_passant_file(&self) -> Option<u8> {
        self.ep_file
    }

    /// The Polyglot key of the position.
    #[inline]
    pub fn hash(&self) -> u64 {
        match self.en_passant() {
            Some(sq) => self.key ^ KEYS[EN_PASSANT + sq.file() as usize],
            None => self.key,
        }
    }

    /// Pieces of either colour that attack `sq`, given `occupied`.
    pub fn attackers_to(&self, sq: Square, occupied: Bitboard) -> Bitboard {
        let diagonal = self.pieces(Piece::Bishop) | self.pieces(Piece::Queen);
        let straight = self.pieces(Piece::Rook) | self.pieces(Piece::Queen);
        (attacks::knight(sq) & self.pieces(Piece::Knight))
            | (attacks::king(sq) & self.pieces(Piece::King))
            | (attacks::bishop(sq, occupied) & diagonal)
            | (attacks::rook(sq, occupied) & straight)
            | (attacks::pawn(Color::White, sq) & self.colored(Piece::Pawn, Color::Black))
            | (attacks::pawn(Color::Black, sq) & self.colored(Piece::Pawn, Color::White))
    }

    /// Whether `by` attacks `sq`, given `occupied`.
    #[inline]
    pub fn is_attacked(&self, sq: Square, by: Color, occupied: Bitboard) -> bool {
        let them = self.colors(by);
        (attacks::knight(sq) & self.pieces(Piece::Knight) & them) != 0
            || (attacks::pawn(!by, sq) & self.pieces(Piece::Pawn) & them) != 0
            || (attacks::king(sq) & self.pieces(Piece::King) & them) != 0
            || (attacks::bishop(sq, occupied) & (self.pieces(Piece::Bishop) | self.pieces(Piece::Queen)) & them) != 0
            || (attacks::rook(sq, occupied) & (self.pieces(Piece::Rook) | self.pieces(Piece::Queen)) & them) != 0
    }

    /// The pieces giving check to the side to move.
    #[inline]
    pub fn checkers(&self) -> Bitboard {
        self.checkers
    }

    #[inline]
    pub fn in_check(&self) -> bool {
        self.checkers != 0
    }

    /// Recomputes the checkers from scratch, for constructors.
    pub(crate) fn refresh_checkers(&mut self) {
        self.checkers = self.attackers_to(self.king(self.side), self.occupied()) & self.colors(!self.side);
    }

    // ----------------------------------------------------------- mutation

    #[inline]
    pub(crate) fn put(&mut self, sq: Square, piece: Piece, color: Color) {
        self.pieces[piece.index()] |= sq.bit();
        self.colors[color.index()] |= sq.bit();
        self.mailbox[sq.index()] = Some((piece, color));
        self.key ^= piece_key(piece, color, sq);
    }

    #[inline]
    fn remove(&mut self, sq: Square, piece: Piece, color: Color) {
        self.pieces[piece.index()] &= !sq.bit();
        self.colors[color.index()] &= !sq.bit();
        self.mailbox[sq.index()] = None;
        self.key ^= piece_key(piece, color, sq);
    }

    pub(crate) fn set_castling(&mut self, color: Color, side: CastleSide, rook: Option<u8>) {
        let slot = &mut self.castling[color.index()][side.index()];
        if slot.is_some() != rook.is_some() {
            self.key ^= castle_key(color, side);
        }
        *slot = rook;
    }

    pub(crate) fn set_side(&mut self, side: Color) {
        if side != self.side {
            self.side = side;
            self.key ^= KEYS[TURN];
        }
    }

    pub(crate) fn set_ep_file(&mut self, file: Option<u8>) {
        self.ep_file = file;
    }

    pub(crate) fn set_clocks(&mut self, halfmove: u16, fullmove: u16) {
        self.halfmove = halfmove;
        self.fullmove = fullmove.max(1);
    }

    pub(crate) fn set_chess960(&mut self, chess960: bool) {
        self.chess960 = chess960;
    }

    /// Plays a move known to be legal. An illegal move leaves the position
    /// unspecified but never panics.
    pub fn play_unchecked(&mut self, mv: Move) {
        let us = self.side;
        let them = !us;
        let Some((piece, _)) = self.piece_at(mv.from) else { return };
        self.ep_file = None;
        self.halfmove = self.halfmove.saturating_add(1);
        let back = us.back_rank();
        // Castling and en passant move or remove a second piece, so the
        // checkers they give are recomputed in full.
        let mut recompute_checkers = false;

        if piece == Piece::King && self.colors(us) & mv.to.bit() != 0 {
            // Castling: the king takes its own rook.
            let side = if mv.to.file() > mv.from.file() { CastleSide::Short } else { CastleSide::Long };
            let (king_file, rook_file) = side.destinations();
            self.remove(mv.from, Piece::King, us);
            self.remove(mv.to, Piece::Rook, us);
            self.put(Square::new(king_file, back), Piece::King, us);
            self.put(Square::new(rook_file, back), Piece::Rook, us);
            self.set_castling(us, CastleSide::Short, None);
            self.set_castling(us, CastleSide::Long, None);
            recompute_checkers = true;
        } else {
            let victim = self.piece_at(mv.to);
            if let Some((v, c)) = victim {
                self.remove(mv.to, v, c);
                self.halfmove = 0;
                if v == Piece::Rook && mv.to.rank() == them.back_rank() {
                    self.drop_castling_rook(them, mv.to.file());
                }
            }
            if piece == Piece::Pawn {
                self.halfmove = 0;
                if mv.from.file() != mv.to.file() && victim.is_none() {
                    // En passant is the only pawn capture onto an empty square.
                    let taken = Square::new(mv.to.file(), mv.from.rank());
                    if self.colored(Piece::Pawn, them) & taken.bit() != 0 {
                        self.remove(taken, Piece::Pawn, them);
                    }
                    recompute_checkers = true;
                }
                if mv.from.rank().abs_diff(mv.to.rank()) == 2 {
                    self.ep_file = Some(mv.from.file());
                }
            }
            self.remove(mv.from, piece, us);
            self.put(mv.to, mv.promotion.unwrap_or(piece), us);
            match piece {
                Piece::King => {
                    self.set_castling(us, CastleSide::Short, None);
                    self.set_castling(us, CastleSide::Long, None);
                }
                Piece::Rook if mv.from.rank() == back => self.drop_castling_rook(us, mv.from.file()),
                _ => {}
            }
        }
        if us == Color::Black {
            self.fullmove = self.fullmove.saturating_add(1);
        }
        self.side = them;
        self.key ^= KEYS[TURN];

        if recompute_checkers {
            self.refresh_checkers();
            return;
        }
        // A check now comes from the moved piece itself, or from a slider of
        // ours the vacated square was blocking. Both are computed from the
        // enemy king outwards, with masks rather than branches: which piece
        // moved and whether it lines up with the king are unpredictable.
        let king = self.king(them);
        let occupied = self.occupied();
        let landed = mv.promotion.unwrap_or(piece) as usize;
        let to = mv.to.bit();
        let d_to = attacks::direction(king, mv.to);
        let along = attacks::ray(d_to, king, occupied) & to;
        let direct = (along
            & ((attacks::straight_mask(d_to) & MOVES_STRAIGHT[landed])
                | (attacks::diagonal_mask(d_to) & MOVES_DIAGONAL[landed])))
            | (attacks::knight(king) & to & IS_KNIGHT[landed])
            | (attacks::pawn(them, king) & to & IS_PAWN[landed]);
        let d_from = attacks::direction(king, mv.from);
        let discovered = attacks::ray(d_from, king, occupied) & self.sliders(d_from) & self.colors(us);
        self.checkers = direct | discovered;
    }

    /// The pieces that attack along direction `d`: rooks and queens on ranks
    /// and files, bishops and queens on diagonals, none for no line.
    #[inline]
    fn sliders(&self, d: usize) -> Bitboard {
        let (straight, diagonal) = (attacks::straight_mask(d), attacks::diagonal_mask(d));
        (self.pieces(Piece::Rook) & straight)
            | (self.pieces(Piece::Bishop) & diagonal)
            | (self.pieces(Piece::Queen) & (straight | diagonal))
    }

    fn drop_castling_rook(&mut self, color: Color, file: u8) {
        for side in CastleSide::ALL {
            if self.castling_rook(color, side) == Some(file) {
                self.set_castling(color, side, None);
            }
        }
    }

    /// Checks that `mv` is legal and plays it. Only en passant and castling are
    /// confirmed after they are played; on that error the position is
    /// unspecified and must be discarded. Checking every move on a copy would
    /// cost a copy of the board per move.
    pub fn play_checked(&mut self, mv: Move) -> Result<(), IllegalMove> {
        let verdict = self.check(mv)?;
        let us = self.side;
        self.play_unchecked(mv);
        match verdict {
            Verdict::Legal => Ok(()),
            Verdict::IfKingSafe(error) if self.is_attacked(self.king(us), !us, self.occupied()) => Err(error),
            Verdict::IfKingSafe(_) => Ok(()),
        }
    }

    /// Whether `mv` is legal. Copies the board only for en passant and castling.
    pub fn is_legal(&self, mv: Move) -> bool {
        match self.check(mv) {
            Err(_) => false,
            Ok(Verdict::Legal) => true,
            Ok(Verdict::IfKingSafe(_)) => {
                let us = self.side;
                let mut b = self.clone();
                b.play_unchecked(mv);
                !b.is_attacked(b.king(us), !us, b.occupied())
            }
        }
    }

    /// Everything about `mv` that can be decided without playing it.
    fn check(&self, mv: Move) -> Result<Verdict, IllegalMove> {
        let us = self.side;
        let them = !us;
        let piece = match self.piece_at(mv.from) {
            Some((p, c)) if c == us => p,
            _ => return Err(IllegalMove::NoPiece),
        };
        if piece == Piece::King && self.colors(us) & mv.to.bit() != 0 {
            if mv.promotion.is_some() {
                return Err(IllegalMove::Promotion);
            }
            self.check_castling(mv)?;
            // The castling rook may have been shielding the king's destination.
            return Ok(Verdict::IfKingSafe(IllegalMove::Castling));
        }
        if (self.colors(us) | self.pieces(Piece::King)) & mv.to.bit() != 0 {
            return Err(IllegalMove::Occupied);
        }
        let occupied = self.occupied();
        let reachable = match piece {
            Piece::Pawn => self.pawn_reaches(mv, occupied)?,
            Piece::Knight => attacks::knight(mv.from) & mv.to.bit() != 0,
            Piece::Bishop => attacks::bishop(mv.from, occupied) & mv.to.bit() != 0,
            Piece::Rook => attacks::rook(mv.from, occupied) & mv.to.bit() != 0,
            Piece::Queen => attacks::queen(mv.from, occupied) & mv.to.bit() != 0,
            Piece::King => attacks::king(mv.from) & mv.to.bit() != 0,
        };
        if !reachable {
            return Err(IllegalMove::Unreachable);
        }
        if piece != Piece::Pawn && mv.promotion.is_some() {
            return Err(IllegalMove::Promotion);
        }
        if piece == Piece::King {
            // The king is lifted so it cannot shield the square it moves to.
            if self.is_attacked(mv.to, them, occupied & !mv.from.bit()) {
                return Err(IllegalMove::LeavesKingInCheck);
            }
            return Ok(Verdict::Legal);
        }
        if piece == Piece::Pawn && mv.from.file() != mv.to.file() && self.colors(them) & mv.to.bit() == 0 {
            // En passant empties two squares of the fifth rank at once.
            return Ok(Verdict::IfKingSafe(IllegalMove::LeavesKingInCheck));
        }
        let king = self.king(us);
        if self.checkers != 0 {
            // Out of check only by taking the single checker or blocking it.
            if self.checkers.count_ones() > 1 {
                return Err(IllegalMove::LeavesKingInCheck);
            }
            let checker = Square::at(self.checkers.trailing_zeros());
            if (checker.bit() | attacks::between(king, checker)) & mv.to.bit() == 0 {
                return Err(IllegalMove::LeavesKingInCheck);
            }
        }
        // A piece leaving the line between its king and an enemy slider;
        // computed whether or not the piece is on such a line, since that is
        // unpredictable and the ray is cheap.
        let d = attacks::direction(king, mv.from);
        let leaves_line = u64::from(attacks::direction(king, mv.to) != d).wrapping_neg();
        let after = (occupied & !mv.from.bit()) | mv.to.bit();
        if attacks::ray(d, king, after) & self.sliders(d) & self.colors(them) & leaves_line != 0 {
            return Err(IllegalMove::LeavesKingInCheck);
        }
        Ok(Verdict::Legal)
    }

    /// Whether a pawn move reaches its destination, checking its promotion.
    fn pawn_reaches(&self, mv: Move, occupied: Bitboard) -> Result<bool, IllegalMove> {
        let us = self.side;
        let (step, start, last): (i8, u8, u8) = match us {
            Color::White => (1, 1, 7),
            Color::Black => (-1, 6, 0),
        };
        let promotes = mv.to.rank() == last;
        match mv.promotion {
            None if promotes => return Err(IllegalMove::Promotion),
            Some(Piece::Knight | Piece::Bishop | Piece::Rook | Piece::Queen) if promotes => {}
            Some(_) => return Err(IllegalMove::Promotion),
            None => {}
        }
        let rank_step = mv.to.rank() as i8 - mv.from.rank() as i8;
        if mv.from.file() == mv.to.file() {
            if occupied & mv.to.bit() != 0 {
                return Ok(false);
            }
            if rank_step == step {
                return Ok(true);
            }
            let middle = Square::new(mv.from.file(), (mv.from.rank() as i8 + step) as u8);
            return Ok(rank_step == 2 * step && mv.from.rank() == start && occupied & middle.bit() == 0);
        }
        if attacks::pawn(us, mv.from) & mv.to.bit() == 0 {
            return Ok(false);
        }
        if self.colors(!us) & mv.to.bit() != 0 {
            return Ok(true);
        }
        // En passant: onto the square the enemy pawn passed over.
        Ok(self.en_passant() == Some(mv.to))
    }

    /// The castling conditions that hold before the move: the right, an empty
    /// path, and no attacked square from the king's start to its destination.
    fn check_castling(&self, mv: Move) -> Result<(), IllegalMove> {
        let us = self.side;
        let them = !us;
        let back = us.back_rank();
        if mv.from.rank() != back || mv.to.rank() != back || self.colored(Piece::Rook, us) & mv.to.bit() == 0 {
            return Err(IllegalMove::Castling);
        }
        let side = if self.castling_rook(us, CastleSide::Short) == Some(mv.to.file()) && mv.to.file() > mv.from.file() {
            CastleSide::Short
        } else if self.castling_rook(us, CastleSide::Long) == Some(mv.to.file()) && mv.to.file() < mv.from.file() {
            CastleSide::Long
        } else {
            return Err(IllegalMove::Castling);
        };
        let (king_file, rook_file) = side.destinations();
        let king_to = Square::new(king_file, back);
        let rook_to = Square::new(rook_file, back);
        // Everything the king and rook pass over or land on must be empty,
        // apart from the two of them.
        let others = self.occupied() & !mv.from.bit() & !mv.to.bit();
        let travel =
            attacks::between(mv.from, king_to) | king_to.bit() | attacks::between(mv.to, rook_to) | rook_to.bit();
        if others & travel != 0 {
            return Err(IllegalMove::Castling);
        }
        // The king may not start, pass or land on an attacked square. The
        // king is lifted so it cannot shield a square behind it.
        let without_king = self.occupied() & !mv.from.bit();
        let path = attacks::between(mv.from, king_to) | king_to.bit() | mv.from.bit();
        if squares(path).any(|sq| self.is_attacked(sq, them, without_king)) {
            return Err(IllegalMove::Castling);
        }
        Ok(())
    }

    /// The position after passing the move, or `None` when in check.
    pub fn null_move(&self) -> Option<Board> {
        if self.in_check() {
            return None;
        }
        let mut b = self.clone();
        b.ep_file = None;
        b.halfmove = b.halfmove.saturating_add(1);
        if b.side == Color::Black {
            b.fullmove = b.fullmove.saturating_add(1);
        }
        b.side = !b.side;
        b.key ^= KEYS[TURN];
        // The side that passed was not in check, and the side not to move
        // never is, so neither side is now.
        b.checkers = 0;
        Some(b)
    }

    /// The Polyglot key recomputed from scratch; equal to [`Board::hash`] on
    /// every position. For tests.
    pub fn hash_from_scratch(&self) -> u64 {
        let mut key = 0;
        for sq in squares(self.occupied()) {
            if let Some((p, c)) = self.piece_at(sq) {
                key ^= piece_key(p, c, sq);
            }
        }
        for color in Color::ALL {
            for side in CastleSide::ALL {
                if self.castling_rook(color, side).is_some() {
                    key ^= castle_key(color, side);
                }
            }
        }
        if let Some(sq) = self.en_passant() {
            key ^= KEYS[EN_PASSANT + sq.file() as usize];
        }
        if self.side == Color::White {
            key ^= KEYS[TURN];
        }
        key
    }
}

/// What [`Board::check`] could decide before a move is played.
enum Verdict {
    Legal,
    /// Legal if the mover's king is not attacked once it is played; otherwise
    /// the error given.
    IfKingSafe(IllegalMove),
}

impl fmt::Debug for Board {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Board({self})")
    }
}
