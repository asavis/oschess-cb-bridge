//! Playing 2CBH move words on a board.
//!
//! A move word carries the moving piece, both squares and the piece captured,
//! so every word can be checked against the position it is played in. A
//! mismatch means the record is damaged or the reader is wrong.

use chesscore::{Board, BoardBuilder, CastleSide as Side, Color as CColor, Move, Piece as CPiece, Square};

use crate::game::{Setup, Start};
use crate::movetable::{self, Captured, CastleSide, Color, MoveWord, Piece};
use crate::v2::{GameMoves, Token};
use crate::{Error, Result};

mod error;

pub use error::MoveError;

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
        b.set(square(sq), Some((p.into(), c.into())));
    }
    b.side_to_move = s.side_to_move.into();
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

/// Converts `word` to a move in `board`, checking that the word agrees with
/// the position: the side to move, the named piece on the origin, the named
/// piece (or nothing) on the destination, en passant available, a castling
/// right. Legality itself is checked when the move is played, by [`play`] or
/// [`walk`]. The null move is not handled here.
///
/// A normal move word, nearly every word a game holds, is checked from its
/// entry in a table of four bytes a word ([`normals`]), a lookup and a few
/// tests; any other word, or one that does not agree with the position, is
/// decoded in full ([`decode_move`]), which gives the error.
pub fn to_move(board: &Board, word: u16) -> std::result::Result<Move, MoveError> {
    let n = normals().get(usize::from(word)).copied().unwrap_or(0);
    if n & NORMAL != 0 {
        let (from, to) =
            (Square::new(n as u8 & 7, (n >> 3) as u8 & 7), Square::new((n >> 6) as u8 & 7, (n >> 9) as u8 & 7));
        let (piece, color) =
            (PIECES[(n >> 12 & 7) as usize], if n & BLACK == 0 { CColor::White } else { CColor::Black });
        let captured = (n >> 16 & 7) as usize;
        let holds = if captured == 0 {
            board.occupied() & to.bit() == 0
        } else {
            board.colored(PIECES[captured - 1], !color) & to.bit() != 0
        };
        if board.side_to_move() == color
            && board.colored(piece, color) & from.bit() != 0
            && board.colors(color) & to.bit() == 0
            && holds
            && (n & EN_PASSANT == 0 || board.en_passant() == Some(to))
        {
            let promotion = (n >> 20 & 7) as usize;
            return Ok(Move::new(from, to, (promotion != 0).then(|| PIECES[promotion - 1])));
        }
    }
    decode_move(board, word)
}

/// A normal move word's entry in [`normals`]: its origin (bits 0-5), its
/// destination (6-11), its piece (12-14, as [`CPiece::index`]), black
/// (15), the piece it takes plus one (16-18, 0 for none), en passant (19),
/// the piece a pawn becomes plus one (20-22, 0 for none), and a normal move
/// word (23), which every other word's entry, 0, is not.
const BLACK: u32 = 1 << 15;
const EN_PASSANT: u32 = 1 << 19;
const NORMAL: u32 = 1 << 23;
/// The pieces by index, and two more, so that three bits always name one.
const PIECES: [CPiece; 8] = [
    CPiece::Pawn,
    CPiece::Knight,
    CPiece::Bishop,
    CPiece::Rook,
    CPiece::Queen,
    CPiece::King,
    CPiece::Pawn,
    CPiece::Pawn,
];

/// The entry of each word below the Chess960 castlings.
fn normals() -> &'static [u32] {
    static NORMALS: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    NORMALS.get_or_init(|| {
        (0..movetable::FIRST_CASTLE_960)
            .map(|word| match movetable::decode(word) {
                Some(MoveWord::Normal { color: c, piece: p, from, to, captured, promotion }) => {
                    let taken = match captured {
                        Captured::Nothing | Captured::EnPassant => 0,
                        Captured::Queen => CPiece::Queen.index() as u32 + 1,
                        Captured::Knight => CPiece::Knight.index() as u32 + 1,
                        Captured::Bishop => CPiece::Bishop.index() as u32 + 1,
                        Captured::Rook => CPiece::Rook.index() as u32 + 1,
                        Captured::Pawn => CPiece::Pawn.index() as u32 + 1,
                    };
                    u32::from(from & 63)
                        | u32::from(to & 63) << 6
                        | (CPiece::from(p).index() as u32) << 12
                        | if c == Color::Black { BLACK } else { 0 }
                        | taken << 16
                        | if captured == Captured::EnPassant { EN_PASSANT } else { 0 }
                        | promotion.map_or(0, |p| CPiece::from(p).index() as u32 + 1) << 20
                        | NORMAL
                }
                _ => 0,
            })
            .collect()
    })
}

/// [`to_move`] from the word decoded in full: the reference, and the error
/// of any word that does not agree with the position.
fn decode_move(board: &Board, word: u16) -> std::result::Result<Move, MoveError> {
    let decoded = movetable::decode(word).ok_or(MoveError::NotAMoveWord(word))?;
    match decoded {
        MoveWord::Null => Err(MoveError::NullMove),
        MoveWord::Castle { color: c, side: s } | MoveWord::Castle960 { color: c, side: s, .. } => {
            let c = CColor::from(c);
            if board.side_to_move() != c {
                return Err(MoveError::CastlingOutOfTurn { word });
            }
            let rook = board.castling_rook(c, Side::from(s)).ok_or(MoveError::NoCastlingRight { word })?;
            Ok(Move::new(board.king(c), Square::new(rook, c.back_rank()), None))
        }
        MoveWord::Normal { color: c, piece: p, from, to, captured, promotion } => {
            let (c, p, from, to) = (CColor::from(c), CPiece::from(p), square(from), square(to));
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
            Ok(Move::new(from, to, promotion.map(CPiece::from)))
        }
    }
}

/// The move `word` names in a standard game, read without a board: from, to
/// and promotion for a normal move, and for castling the king onto its rook
/// on their home squares, as [`Board::play_unchecked`] takes it. `None` for a
/// null move, a Chess960 castling, or a word that names no move. A move
/// stream replays words that were checked when it was written with it.
pub fn standard_move(word: u16) -> Option<Move> {
    match movetable::decode(word)? {
        MoveWord::Normal { from, to, promotion, .. } => {
            Some(Move::new(square(from), square(to), promotion.map(CPiece::from)))
        }
        MoveWord::Castle { color: c, side: s } => {
            let rank = CColor::from(c).back_rank();
            let rook = if s == CastleSide::Short { 7 } else { 0 };
            Some(Move::new(Square::new(4, rank), Square::new(rook, rank), None))
        }
        MoveWord::Null | MoveWord::Castle960 { .. } => None,
    }
}

/// The word that names `mv` in `board`, the inverse of [`to_move`]: a normal
/// move names its piece, its squares, what it takes and its promotion;
/// castling, the king onto its own rook, names its side. `None` when no word
/// names the move, as for one from an empty square.
pub fn word_of(board: &Board, mv: Move) -> Option<u16> {
    let (p, c) = board.piece_at(mv.from)?;
    let us = Color::from(c);
    if p == CPiece::King && board.colors(c) & mv.to.bit() != 0 {
        let side = if mv.to.file() > mv.from.file() { CastleSide::Short } else { CastleSide::Long };
        return movetable::encode(MoveWord::Castle { color: us, side });
    }
    let captured = match board.piece_at(mv.to).map(|(v, _)| v) {
        Some(CPiece::Queen) => Captured::Queen,
        Some(CPiece::Rook) => Captured::Rook,
        Some(CPiece::Bishop) => Captured::Bishop,
        Some(CPiece::Knight) => Captured::Knight,
        Some(CPiece::Pawn) => Captured::Pawn,
        Some(CPiece::King) => return None,
        None if p == CPiece::Pawn && mv.from.file() != mv.to.file() => Captured::EnPassant,
        None => Captured::Nothing,
    };
    movetable::encode(MoveWord::Normal {
        color: us,
        piece: p.into(),
        from: mv.from.index() as movetable::Sq,
        to: mv.to.index() as movetable::Sq,
        captured,
        promotion: mv.promotion.map(Piece::from),
    })
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
    /// Whether the visitor has what it needs. The walk then ends at once and
    /// successfully, without reading or checking the rest of the tree.
    fn stopped(&self) -> bool {
        false
    }
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
        if visitor.stopped() {
            return Ok(stats);
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
    // A visitor that stopped on the last word has what it needs too.
    if !ended && !visitor.stopped() {
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

    /// Every legal move of positions with castling both ways, en passant,
    /// promotions with and without a capture, and captures of every kind has
    /// a word that names it there and plays it back, and the word read
    /// without a board is the same move.
    #[test]
    fn a_word_names_each_legal_move_and_plays_it_back() {
        let mut seen = 0;
        for fen in [
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R b KQkq - 0 1",
            "n1n5/PPPk4/8/8/8/8/4Kppp/5N1N b - - 0 1",
            "n1n5/PPPk4/8/8/8/8/4Kppp/5N1N w - - 0 1",
            "rnbqkbnr/ppp1p1pp/8/3pPp2/8/8/PPPP1PPP/RNBQKBNR w KQkq f6 0 3",
        ] {
            let board = Board::from_fen(fen).unwrap();
            for mv in board.legal_moves() {
                let word = word_of(&board, mv).unwrap_or_else(|| panic!("{fen}: no word for {mv}"));
                assert!(word < movetable::FIRST_CASTLE_960, "{fen}: {mv}");
                assert_eq!(to_move(&board, word), Ok(mv), "{fen}: {mv}");
                assert_eq!(standard_move(word), Some(mv), "{fen}: {mv}");
                let (mut checked, mut unchecked) = (board.clone(), board.clone());
                assert_eq!(play(&mut checked, word), Ok(Some(mv)));
                unchecked.play_unchecked(mv);
                assert_eq!(checked, unchecked, "{fen}: {mv}");
                seen += 1;
            }
        }
        assert!(seen > 150, "{seen}");
        assert_eq!(standard_move(movetable::NULL_MOVE), None);
        assert_eq!(standard_move(movetable::FIRST_CASTLE_960), None);
        assert_eq!(standard_move(0), None);
    }

    /// Every word, in positions with castling both ways, en passant,
    /// promotions with and without a capture, checks, and those of random
    /// games, gives from its table entry what the word decoded in full
    /// gives: the move, or the same error.
    #[test]
    fn a_words_entry_agrees_with_the_word_decoded() {
        let mut boards: Vec<Board> = [
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R b KQkq - 0 1",
            "n1n5/PPPk4/8/8/8/8/4Kppp/5N1N b - - 0 1",
            "n1n5/PPPk4/8/8/8/8/4Kppp/5N1N w - - 0 1",
            "rnbqkbnr/ppp1p1pp/8/3pPp2/8/8/PPPP1PPP/RNBQKBNR w KQkq f6 0 3",
            "4k3/8/8/8/3pP3/8/8/4K3 b - e3 0 1",
            "4k3/8/8/1b6/8/8/8/4K2R w K - 0 1",
        ]
        .iter()
        .map(|fen| Board::from_fen(fen).unwrap())
        .collect();
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..8 {
            let mut board = Board::startpos();
            for _ in 0..90 {
                let moves = board.legal_moves();
                if moves.is_empty() {
                    break;
                }
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                board.play_unchecked(moves[(x % moves.len() as u64) as usize]);
                if x.is_multiple_of(7) {
                    boards.push(board.clone());
                }
            }
        }
        assert!(boards.len() > 60, "{}", boards.len());
        let mut agreed = 0;
        for board in &boards {
            for word in 0..=u16::MAX {
                let full = decode_move(board, word);
                assert_eq!(to_move(board, word), full, "{word:#06x} in {board:?}");
                agreed += u32::from(full.is_ok());
            }
        }
        assert!(agreed > 1_000, "{agreed}");
    }
}
