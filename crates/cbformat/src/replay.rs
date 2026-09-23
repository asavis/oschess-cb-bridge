//! Playing 2CBH move words on a board.
//!
//! A move word carries the moving piece, both squares and the piece captured,
//! so every word can be checked against the position it is played in. A
//! mismatch means the record is damaged or the reader is wrong.

use std::fmt;

use chesscore::{Board, BoardBuilder, CastleSide as Side, Color as CColor, IllegalMove, Move, Piece as CPiece, Square};

use crate::movetable::{self, Captured, CastleSide, Color, MoveWord, Piece};
use crate::v2::{GameMoves, Setup, Start, Token};
use crate::{Error, Result};

fn color(c: Color) -> CColor {
    match c {
        Color::White => CColor::White,
        Color::Black => CColor::Black,
    }
}

fn piece(p: Piece) -> CPiece {
    match p {
        Piece::King => CPiece::King,
        Piece::Queen => CPiece::Queen,
        Piece::Knight => CPiece::Knight,
        Piece::Bishop => CPiece::Bishop,
        Piece::Rook => CPiece::Rook,
        Piece::Pawn => CPiece::Pawn,
    }
}

fn side(s: CastleSide) -> Side {
    match s {
        CastleSide::Short => Side::Short,
        CastleSide::Long => Side::Long,
    }
}

/// A move-table square (rank-major, below 64 by construction).
fn square(sq: movetable::Sq) -> Square {
    Square::new(sq & 7, sq >> 3)
}

/// The standard start position, built once.
fn standard_start() -> &'static Board {
    static START: std::sync::OnceLock<Board> = std::sync::OnceLock::new();
    START.get_or_init(Board::startpos)
}

/// The board a game starts from.
pub fn start_board(start: &Start) -> Result<Board> {
    match start {
        Start::Standard => Ok(standard_start().clone()),
        Start::Chess960(n) => Board::chess960(*n).ok_or_else(|| Error::Format(format!("Chess960 position {n}"))),
        Start::Setup(s) => setup_board(s),
    }
}

fn setup_board(s: &Setup) -> Result<Board> {
    let mut b = BoardBuilder::empty();
    for &(sq, c, p) in &s.pieces {
        b.set(square(sq), Some((piece(p), color(c))));
    }
    b.side_to_move = color(s.side_to_move);
    b.chess960 = s.chess960;
    for (c, long, short) in [(CColor::White, 0, 1), (CColor::Black, 2, 3)] {
        let back = c.back_rank();
        let on = |f: u8, p: CPiece| b.squares[Square::new(f, back).index()] == Some((p, c));
        let king_file = (0..8).find(|&f| on(f, CPiece::King));
        let rook_files: Vec<u8> = (0..8).filter(|&f| on(f, CPiece::Rook)).collect();
        // A right is kept only when the king and a rook stand where it needs
        // them, so a stray bit cannot make the position unbuildable.
        let Some(kf) = king_file else { continue };
        // A king the record names must stand on its named square.
        if s.castling_kings[c.index()].is_some_and(|named| named != kf) {
            continue;
        }
        let home = |f: u8| (kf == 4 && rook_files.contains(&f)).then_some(f);
        // A rook the record names must stand on its wing of the king.
        let named = |i: usize, wing: fn(u8, u8) -> bool| {
            s.castling_rooks[i].map(|f| (rook_files.contains(&f) && wing(f, kf)).then_some(f))
        };
        let rights = &mut b.castling[c.index()];
        if s.castling & (1 << short) != 0 {
            rights[Side::Short as usize] = match named(short, |f, k| f > k) {
                Some(named) => named,
                None if s.chess960 => rook_files.iter().copied().filter(|&f| f > kf).max(),
                None => home(7),
            };
        }
        if s.castling & (1 << long) != 0 {
            rights[Side::Long as usize] = match named(long, |f, k| f < k) {
                Some(named) => named,
                None if s.chess960 => rook_files.iter().copied().filter(|&f| f < kf).min(),
                None => home(0),
            };
        }
    }
    b.en_passant_file = s.en_passant_file.filter(|&f| f < 8);
    // A stored en passant file with no pawn that could just have made the
    // double step carries no information about the position; drop it.
    if !b.en_passant_is_valid() {
        b.en_passant_file = None;
    }
    b.fullmove_number = s.move_number.max(1);
    b.build().map_err(|e| Error::Format(format!("set-up position: {e}")))
}

/// Why a move word cannot be played where it stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveError {
    /// The word is neither a move word nor the null move.
    NotAMoveWord(u16),
    /// The null move, given to [`to_move`].
    NullMove,
    /// A null move while the side to move is in check.
    NullMoveInCheck,
    /// A castling word for the side not to move.
    CastlingOutOfTurn { word: u16 },
    /// A castling word without the castling right.
    NoCastlingRight { word: u16 },
    /// A move word of `color` when the other side is to move.
    OutOfTurn { word: u16, color: CColor },
    /// The word's piece is not on its origin.
    NoPiece { word: u16, color: CColor, piece: CPiece, from: Square },
    /// The destination holds a piece of the mover's colour.
    OntoOwnPiece { word: u16, color: CColor, from: Square, to: Square },
    /// The destination does not hold what the word says it captures.
    WrongCapture { word: u16, from: Square, to: Square, named: Captured, found: Option<(CPiece, CColor)> },
    /// An en passant capture where none is available.
    NoEnPassant { word: u16, from: Square, to: Square },
    /// The word agrees with the position but the move is illegal.
    Illegal { word: u16, mv: Move, why: IllegalMove },
}

impl fmt::Display for MoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            MoveError::NotAMoveWord(word) => write!(f, "{word:#06x} is not a move word"),
            MoveError::NullMove => f.write_str("null move"),
            MoveError::NullMoveInCheck => f.write_str("null move while in check"),
            MoveError::CastlingOutOfTurn { word } => write!(f, "{word:#06x}: castling for the side not to move"),
            MoveError::NoCastlingRight { word } => write!(f, "{word:#06x}: castling without the right"),
            MoveError::OutOfTurn { word, color } => write!(f, "{word:#06x}: {color:?} move with {:?} to move", !color),
            MoveError::NoPiece { word, color, piece, from } => {
                write!(f, "{word:#06x}: no {color:?} {piece:?} on {from}")
            }
            MoveError::OntoOwnPiece { word, color, from, to } => {
                write!(f, "{word:#06x}: {from}{to} lands on a {color:?} piece")
            }
            MoveError::WrongCapture { word, from, to, named, found } => {
                write!(f, "{word:#06x}: {from}{to} names capture {named:?}, square holds {found:?}")
            }
            MoveError::NoEnPassant { word, from, to } => write!(f, "{word:#06x}: en passant {from}{to} not available"),
            MoveError::Illegal { word, mv, why } => write!(f, "{word:#06x}: {mv}: {why}"),
        }
    }
}

impl std::error::Error for MoveError {}

/// Converts `word` to a move in `board`, checking that the word agrees with
/// the position: the side to move, the named piece on the origin, the named
/// piece (or nothing) on the destination, en passant available, a castling
/// right. Legality itself is checked when the move is played, by [`play`] or
/// [`walk`]. The null move is not handled here.
pub fn to_move(board: &Board, word: u16) -> std::result::Result<Move, MoveError> {
    let decoded = movetable::decode(word).ok_or(MoveError::NotAMoveWord(word))?;
    match decoded {
        MoveWord::Null => Err(MoveError::NullMove),
        MoveWord::Castle { color: c, side: s } | MoveWord::Castle960 { color: c, side: s, .. } => {
            let c = color(c);
            if board.side_to_move() != c {
                return Err(MoveError::CastlingOutOfTurn { word });
            }
            let rook = board.castling_rook(c, side(s)).ok_or(MoveError::NoCastlingRight { word })?;
            Ok(Move::new(board.king(c), Square::new(rook, c.back_rank()), None))
        }
        MoveWord::Normal { color: c, piece: p, from, to, captured, promotion } => {
            let (c, p, from, to) = (color(c), piece(p), square(from), square(to));
            if board.side_to_move() != c {
                return Err(MoveError::OutOfTurn { word, color: c });
            }
            if board.colored(p, c) & from.bit() == 0 {
                return Err(MoveError::NoPiece { word, color: c, piece: p, from });
            }
            // Castling is the king taking its own rook, so a normal move word
            // onto a friendly piece must be refused here or it would be played
            // as castling.
            if board.colors(c) & to.bit() != 0 {
                return Err(MoveError::OntoOwnPiece { word, color: c, from, to });
            }
            let named = match captured {
                Captured::Nothing | Captured::EnPassant => None,
                Captured::Queen => Some(CPiece::Queen),
                Captured::Knight => Some(CPiece::Knight),
                Captured::Bishop => Some(CPiece::Bishop),
                Captured::Rook => Some(CPiece::Rook),
                Captured::Pawn => Some(CPiece::Pawn),
            };
            let holds_named = match named {
                None => board.occupied() & to.bit() == 0,
                Some(v) => board.colored(v, !c) & to.bit() != 0,
            };
            if !holds_named {
                return Err(MoveError::WrongCapture { word, from, to, named: captured, found: board.piece_at(to) });
            }
            if captured == Captured::EnPassant && board.en_passant() != Some(to) {
                return Err(MoveError::NoEnPassant { word, from, to });
            }
            Ok(Move::new(from, to, promotion.map(piece)))
        }
    }
}

/// Checks and plays `word` on `board`, including the null move. On error the
/// board is unspecified and must be discarded.
pub fn play(board: &mut Board, word: u16) -> std::result::Result<Option<Move>, MoveError> {
    if word == movetable::NULL_MOVE {
        *board = board.null_move().ok_or(MoveError::NullMoveInCheck)?;
        return Ok(None);
    }
    let mv = to_move(board, word)?;
    board.play_checked(mv).map_err(|why| MoveError::Illegal { word, mv, why })?;
    Ok(Some(mv))
}

/// The most variations open at once in one walk. Each open variation keeps a
/// saved board, so this bounds the walk's memory whatever a move record
/// holds; the deepest nesting in the whole Mega Database 2026 is 74.
pub const MAX_VARIATION_DEPTH: usize = 1024;

/// Counts from a full walk of a move tree.
#[derive(Clone, Copy, Debug, Default)]
pub struct TreeStats {
    pub main_line_plies: u32,
    pub total_plies: u32,
    pub lines: u32,
}

/// Receives the moves of a tree in stored order from [`walk`].
///
/// The tree arrives as a depth-first walk: `play` and then `played` for each
/// move, `branch` when the move just played has an alternative still to come,
/// and `resume` when a line ends and the walk returns to the position before
/// the move whose `branch` is most recent.
///
/// `play` announces a move whose word already agrees with the position; its
/// legality is checked as it is played, in place. If that check fails, the walk
/// returns the error at once without calling `played`, and whatever the
/// visitor built must be discarded. The shape of the tree is checked by the
/// walk too, so a visitor needs no checks of its own.
pub trait TreeVisitor {
    /// A move (`None` for a null move) about to be played from `before`.
    fn play(&mut self, before: &Board, mv: Option<Move>, main_line: bool);
    /// The position the move announced by the last `play` produced.
    fn played(&mut self, _after: &Board) {}
    fn branch(&mut self) {}
    fn resume(&mut self) {}
}

struct FnVisitor<F>(F);

impl<F: FnMut(&Board, Option<Move>, bool)> TreeVisitor for FnVisitor<F> {
    fn play(&mut self, before: &Board, mv: Option<Move>, main_line: bool) {
        (self.0)(before, mv, main_line)
    }
}

/// Walks every line of the tree, checking each move. `visit` sees the board
/// before each move, the move (`None` for a null move) and whether it is on
/// the main line.
pub fn walk_tree(moves: &GameMoves<'_>, visit: impl FnMut(&Board, Option<Move>, bool)) -> Result<TreeStats> {
    walk(moves, &mut FnVisitor(visit))
}

/// Walks every line of the tree, checking each move and the tree's shape: an
/// alternative marker must follow a move, the tree must end with its final
/// end-of-line marker, and nothing may follow that.
///
/// Moves are checked and played in place on one board. The position before a
/// move is copied only when an alternative marker follows it, which is the one
/// time it is needed again: a copy per move costs as much as the move itself.
pub fn walk(moves: &GameMoves<'_>, visitor: &mut impl TreeVisitor) -> Result<TreeStats> {
    let mut board = start_board(&moves.start()?)?;
    let mut stack: Vec<Board> = Vec::new();
    // The position before the last move, kept when an alternative marker follows it.
    let mut saved: Option<Board> = None;
    let mut stats = TreeStats { lines: 1, ..Default::default() };
    let mut main = true;
    let mut ended = false;
    let mut tokens = moves.tokens().peekable();
    while let Some(token) = tokens.next() {
        if ended {
            return Err(Error::Format("words after the final end of line".into()));
        }
        match token {
            Token::Move(w) => {
                let ply = stats.total_plies + 1;
                let fail = |reason: MoveError| Error::Move { ply, reason };
                let step = if w == movetable::NULL_MOVE {
                    Step::Null(Box::new(board.null_move().ok_or_else(|| fail(MoveError::NullMoveInCheck))?))
                } else {
                    Step::Normal(to_move(&board, w).map_err(fail)?)
                };
                saved = (tokens.peek() == Some(&Token::Alternative)).then(|| board.clone());
                match step {
                    Step::Normal(mv) => {
                        visitor.play(&board, Some(mv), main);
                        board.play_checked(mv).map_err(|why| fail(MoveError::Illegal { word: w, mv, why }))?;
                    }
                    Step::Null(next) => {
                        visitor.play(&board, None, main);
                        board = *next;
                    }
                }
                visitor.played(&board);
                stats.total_plies += 1;
                if main {
                    stats.main_line_plies += 1;
                }
            }
            Token::Alternative => {
                // `take` leaves None, so a second marker after the same move is refused too.
                let b = saved.take().ok_or_else(|| Error::Format("alternative marker not after a move".into()))?;
                if stack.len() >= MAX_VARIATION_DEPTH {
                    return Err(Error::Format(format!("variations nested deeper than {MAX_VARIATION_DEPTH}")));
                }
                stack.push(b);
                visitor.branch();
            }
            Token::EndOfLine => {
                main = false;
                saved = None;
                match stack.pop() {
                    Some(b) => {
                        board = b;
                        stats.lines += 1;
                        visitor.resume();
                    }
                    None => ended = true,
                }
            }
        }
    }
    if !ended {
        return Err(Error::Format("move tree not terminated".into()));
    }
    Ok(stats)
}

enum Step {
    Normal(Move),
    /// A null move, with the position it produces. Boxed: null moves are rare
    /// and a board is large.
    Null(Box<Board>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_unchanged() {
        let (e4, e5, d6) = (Square::new(4, 3), Square::new(4, 4), Square::new(3, 5));
        let cases = [
            (MoveError::NotAMoveWord(0xc02d), "0xc02d is not a move word"),
            (MoveError::NullMove, "null move"),
            (MoveError::NullMoveInCheck, "null move while in check"),
            (MoveError::CastlingOutOfTurn { word: 0xb12a }, "0xb12a: castling for the side not to move"),
            (MoveError::NoCastlingRight { word: 0xb12a }, "0xb12a: castling without the right"),
            (MoveError::OutOfTurn { word: 0x0001, color: CColor::White }, "0x0001: White move with Black to move"),
            (
                MoveError::NoPiece { word: 0x0001, color: CColor::Black, piece: CPiece::Knight, from: e4 },
                "0x0001: no Black Knight on e4",
            ),
            (
                MoveError::OntoOwnPiece { word: 0x0001, color: CColor::White, from: e4, to: e5 },
                "0x0001: e4e5 lands on a White piece",
            ),
            (
                MoveError::WrongCapture {
                    word: 0x0001,
                    from: e4,
                    to: e5,
                    named: Captured::Knight,
                    found: Some((CPiece::Pawn, CColor::Black)),
                },
                "0x0001: e4e5 names capture Knight, square holds Some((Pawn, Black))",
            ),
            (MoveError::NoEnPassant { word: 0xad67, from: e5, to: d6 }, "0xad67: en passant e5d6 not available"),
            (
                MoveError::Illegal { word: 0x0001, mv: Move::new(e4, e5, None), why: IllegalMove::LeavesKingInCheck },
                &format!("0x0001: e4e5: {}", IllegalMove::LeavesKingInCheck),
            ),
        ];
        for (e, text) in cases {
            assert_eq!(e.to_string(), text);
        }
    }
}
