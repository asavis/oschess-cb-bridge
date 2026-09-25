//! Decoding a `.cbg` move stream while walking the game tree.
//!
//! Every byte is first translated through the table of the game's encoding
//! mode, keyed by the number of moves decoded so far. The compact encoder
//! then names a move by piece and movement: "the second rook, three squares
//! up", so the walk keeps the piece lists ([`Pieces`]) next to the board, and
//! saves both at a branch.

use std::cell::Cell;

use chesscore::{Board, CastleSide, Color as CColor, Move, Piece as CPiece, Square};

use super::moves::{GameMoves, cb_square};
use super::pieces::{KINDS, Pieces, to_cb};
use super::tables;
use crate::replay::{MoveError, TreeStats, TreeVisitor, start_board};
use crate::v2::Start;
use crate::{Error, Result};

/// Most variations open at once in a game, as for 2CBH games. Each open one
/// keeps a board and the piece lists (about 350 bytes), so the stack stays
/// under 400 KiB.
pub use crate::replay::MAX_VARIATION_DEPTH;

/// The table, modifier and encoder of an encoding mode whose table is known.
fn mode(m: u8) -> Result<(&'static [u8; 256], bool, bool)> {
    // (table, pre modifier, simple encoder)
    match m {
        0 => Ok((&tables::MODE_0, true, false)),
        4 => Ok((&tables::MODE_4, false, false)),
        5 => Ok((&tables::MODE_5, false, true)),
        10 => Ok((&tables::MODE_10, false, false)),
        _ => Err(Error::Format(format!("encoding mode {m} is not supported"))),
    }
}

struct Walker<'v, V> {
    visitor: &'v mut V,
    board: Board,
    pieces: Pieces,
    stack: Vec<(Board, Pieces)>,
    stats: TreeStats,
    main: bool,
    ended: bool,
    /// A variation starts before the next move: keep the position before it.
    branch_next: bool,
    table: &'static [u8; 256],
    pre: bool,
    /// The castling right a castling move lacked, when that stopped the walk.
    missing_right: Cell<Option<(CColor, CastleSide)>>,
}

impl<V: TreeVisitor> Walker<'_, V> {
    fn translate(&self, byte: u8) -> u8 {
        let n = self.stats.total_plies as u8;
        if self.pre { self.table[byte.wrapping_sub(n) as usize] } else { self.table[byte as usize].wrapping_sub(n) }
    }

    fn fail(&self, what: String) -> Error {
        Error::Format(format!("move {}: {what}", self.stats.total_plies + 1))
    }

    fn start_variation(&mut self) -> Result<()> {
        if self.branch_next {
            return Err(self.fail("two variation starts in a row".into()));
        }
        // Checked before the position is saved, so a hostile record cannot
        // make the stack grow past the bound.
        if self.stack.len() >= MAX_VARIATION_DEPTH {
            return Err(self.fail(format!("variations nested deeper than {MAX_VARIATION_DEPTH}")));
        }
        self.branch_next = true;
        Ok(())
    }

    fn end_line(&mut self) -> Result<()> {
        if self.branch_next {
            return Err(self.fail("a variation start before the end of a line".into()));
        }
        self.main = false;
        match self.stack.pop() {
            Some((b, p)) => {
                self.board = b;
                self.pieces = p;
                self.stats.lines += 1;
                self.visitor.resume();
                // A visitor with what it needs ends the walk here.
                self.ended = self.visitor.stopped();
            }
            None => self.ended = true,
        }
        Ok(())
    }

    /// Plays one move (`None` is a null move); `code` names it in errors.
    fn play(&mut self, mv: Option<Move>, code: u16) -> Result<()> {
        let ply = self.stats.total_plies + 1;
        let saved = self.branch_next.then(|| (self.board.clone(), self.pieces));
        self.branch_next = false;
        match mv {
            None => {
                let next = self.board.null_move().ok_or(Error::Move { ply, reason: MoveError::NullMoveInCheck })?;
                self.visitor.play(&self.board, None, self.main);
                self.board = next;
            }
            Some(mv) => {
                self.visitor.play(&self.board, Some(mv), self.main);
                let before = self.board.clone();
                let us = before.side_to_move();
                self.board
                    .play_checked(mv)
                    .map_err(|why| Error::Move { ply, reason: MoveError::Illegal { word: code, mv, why } })?;
                self.pieces.update(&before, us, mv)?;
            }
        }
        self.visitor.played(&self.board);
        self.stats.total_plies += 1;
        if self.main {
            self.stats.main_line_plies += 1;
        }
        if let Some(s) = saved {
            self.stack.push(s);
            self.visitor.branch();
        }
        // A visitor with what it needs ends the walk here.
        self.ended = self.visitor.stopped();
        Ok(())
    }

    fn castle(&self, side: CastleSide) -> Result<Move> {
        let us = self.board.side_to_move();
        let Some(rook) = self.board.castling_rook(us, side) else {
            self.missing_right.set(Some((us, side)));
            return Err(self.fail("castling without the right".into()));
        };
        Ok(Move::new(self.board.king(us), Square::new(rook, us.back_rank()), None))
    }

    /// A move given by its squares, as the two-byte and simple forms give it.
    ///
    /// Castling has two encodings here and no other: in a Chess960 game the
    /// king's destination, `g1` `c1` `g8` `c8` for the side to move, as both
    /// squares; in any other game the king's move from `e1` or `e8` to the
    /// `g` or `c` square of the same rank.
    fn by_squares(&self, v: u16, chess960: bool) -> Result<Option<Move>> {
        let (from, to) = ((v & 63) as u8, (v >> 6 & 63) as u8);
        let (from_sq, to_sq) = (cb_square(from), cb_square(to));
        let us = self.board.side_to_move();
        let back = us.back_rank();
        if from == to {
            return match (chess960, to_sq.file(), to_sq.rank() == back) {
                _ if v & 0x0fff == 0 => Ok(None),
                (true, 6, true) => self.castle(CastleSide::Short).map(Some),
                (true, 2, true) => self.castle(CastleSide::Long).map(Some),
                _ => Err(self.fail(format!("a move from {from_sq} to itself"))),
            };
        }
        let castling = !chess960 && from_sq == Square::new(4, back) && to_sq.rank() == back;
        match self.board.piece_at(from_sq) {
            Some((CPiece::King, c)) if c == us && castling && to_sq.file() == 6 => {
                self.castle(CastleSide::Short).map(Some)
            }
            Some((CPiece::King, c)) if c == us && castling && to_sq.file() == 2 => {
                self.castle(CastleSide::Long).map(Some)
            }
            Some((CPiece::Pawn, _)) if to_sq.rank() == 0 || to_sq.rank() == 7 => {
                let promo = [CPiece::Queen, CPiece::Rook, CPiece::Bishop, CPiece::Knight][(v >> 12 & 3) as usize];
                self.ordinary(Move::new(from_sq, to_sq, Some(promo)), v)
            }
            _ => self.ordinary(Move::new(from_sq, to_sq, None), v),
        }
    }

    /// `mv` as a move other than castling. One onto a piece of the side to
    /// move is refused here: `chesscore` reads a king onto its own rook as
    /// castling, which only the castling encodings may name.
    fn ordinary(&self, mv: Move, word: u16) -> Result<Option<Move>> {
        let us = self.board.side_to_move();
        if self.board.piece_at(mv.to).is_some_and(|(_, c)| c == us) {
            let reason = MoveError::OntoOwnPiece { word, color: us, from: mv.from, to: mv.to };
            return Err(Error::Move { ply: self.stats.total_plies + 1, reason });
        }
        Ok(Some(mv))
    }

    /// A one-byte compact code other than the markers.
    fn by_code(&self, code: u8) -> Result<Option<Move>> {
        const KING: [(i8, i8); 8] = [(0, 1), (1, 1), (1, 0), (1, -1), (0, -1), (-1, -1), (-1, 0), (-1, 1)];
        const KNIGHT: [(i8, i8); 8] = [(2, 1), (1, 2), (-1, 2), (-2, 1), (-2, -1), (-1, -2), (1, -2), (2, -1)];
        let us = self.board.side_to_move();
        let line = |k: u8, dirs: &[(i8, i8)]| {
            let (d, s) = (dirs[(k / 7) as usize], (k % 7 + 1) as i8);
            (d.0 * s, d.1 * s)
        };
        const Q: [(i8, i8); 4] = [(0, 1), (1, 0), (1, 1), (1, -1)];
        const R: [(i8, i8); 2] = [(0, 1), (1, 0)];
        const B: [(i8, i8); 2] = [(1, 1), (1, -1)];
        let (kind, index, delta) = match code {
            0 => return Ok(None),
            1..=8 => {
                let from = to_cb(self.board.king(us));
                return self.ordinary(self.step(from, KING[(code - 1) as usize], None), u16::from(code));
            }
            9 => return self.castle(CastleSide::Short).map(Some),
            10 => return self.castle(CastleSide::Long).map(Some),
            11..=38 => (0, 0, line(code - 11, &Q)),
            39..=52 => (1, 0, line(code - 39, &R)),
            53..=66 => (1, 1, line(code - 53, &R)),
            67..=80 => (2, 0, line(code - 67, &B)),
            81..=94 => (2, 1, line(code - 81, &B)),
            95..=102 => (3, 0, KNIGHT[(code - 95) as usize]),
            103..=110 => (3, 1, KNIGHT[(code - 103) as usize]),
            111..=142 => {
                let (pawn, how) = ((code - 111) / 4, (code - 111) % 4);
                let f: i8 = if us == CColor::White { 1 } else { -1 };
                let delta = [(0, f), (0, 2 * f), (f, f), (-f, f)][how as usize];
                let from = self.pawns(us)[pawn as usize].ok_or_else(|| self.fail(format!("no pawn number {pawn}")))?;
                let mv = self.step(from, delta, None);
                if mv.to.rank() == 0 || mv.to.rank() == 7 {
                    return Err(self.fail("a promotion in a one-byte move".into()));
                }
                return self.ordinary(mv, u16::from(code));
            }
            143..=170 => (0, 1, line(code - 143, &Q)),
            171..=198 => (0, 2, line(code - 171, &Q)),
            199..=212 => (1, 2, line(code - 199, &R)),
            213..=226 => (2, 2, line(code - 213, &B)),
            227..=234 => (3, 2, KNIGHT[(code - 227) as usize]),
            _ => return Err(self.fail(format!("code {code} is not a move"))),
        };
        let from = self.pieces.kinds[us.index()][kind]
            .get(index)
            .ok_or_else(|| self.fail(format!("no {:?} number {}", KINDS[kind], index + 1)))?;
        self.ordinary(self.step(from, delta, None), u16::from(code))
    }

    fn pawns(&self, us: CColor) -> [Option<u8>; 8] {
        self.pieces.pawns[us.index()]
    }

    /// The move from ChessBase square `from` by `delta`, each coordinate
    /// taken modulo 8.
    fn step(&self, from: u8, delta: (i8, i8), promotion: Option<CPiece>) -> Move {
        let x = (from / 8) as i8 + delta.0;
        let y = (from % 8) as i8 + delta.1;
        Move::new(cb_square(from), Square::new(x.rem_euclid(8) as u8, y.rem_euclid(8) as u8), promotion)
    }

    fn compact(&mut self, s: &[u8], chess960: bool) -> Result<()> {
        let mut i = 0;
        while i < s.len() {
            let v = self.translate(s[i]);
            i += 1;
            if v == 236 {
                continue;
            }
            if self.ended {
                // Some records carry a few bytes after the final end of line,
                // likely left over from a longer version of the game; the tree
                // is complete, so they are ignored.
                break;
            }
            match v {
                254 => self.start_variation()?,
                255 => self.end_line()?,
                235 => {
                    let pair =
                        s.get(i..i + 2).ok_or_else(|| self.fail("a two-byte move runs past the record".into()))?;
                    let word = u16::from(self.translate(pair[0])) << 8 | u16::from(self.translate(pair[1]));
                    i += 2;
                    let mv = self.by_squares(word, chess960)?;
                    self.play(mv, word)?;
                }
                237..=253 => return Err(self.fail(format!("code {v} is not used"))),
                code => {
                    let mv = self.by_code(code)?;
                    self.play(mv, u16::from(code))?;
                }
            }
        }
        Ok(())
    }

    fn simple(&mut self, s: &[u8]) -> Result<()> {
        // A cut last byte is reported where it is, after the moves before it,
        // so a visitor that stops earlier never meets it.
        let (pairs, rest) = s.as_chunks::<2>();
        for pair in pairs {
            if self.ended {
                break;
            }
            let word = u16::from(self.translate(pair[0])) << 8 | u16::from(self.translate(pair[1]));
            if word & 0x8000 != 0 {
                self.start_variation()?;
            }
            let mv = self.by_squares(word & 0x3fff, false)?;
            self.play(mv, word)?;
            if word & 0x4000 != 0 {
                self.end_line()?;
            }
        }
        if !rest.is_empty() && !self.ended {
            return Err(Error::Format("move stream: odd length for two-byte moves".into()));
        }
        Ok(())
    }
}

/// Walks every line of the tree, checking each move and the tree's shape, and
/// reports it to `visitor` exactly as [`crate::replay::walk`] reports a 2CBH
/// tree: the stored order of both formats is the same depth-first order, the
/// main line first at every position. The game starts from
/// [`start_as_played`].
pub fn walk(game: &GameMoves<'_>, visitor: &mut impl TreeVisitor) -> Result<TreeStats> {
    run(game, &start_as_played(game)?, visitor).0
}

struct Silent;

impl TreeVisitor for Silent {
    fn play(&mut self, _: &Board, _: Option<Move>, _: bool) {}
}

/// Where the game starts, as its moves play it. ChessBase writes castling
/// moves in set-up games whose stored castling rights lack them; older
/// databases store no rights at all. Such a game starts with the rights its
/// castling moves use added, as long as the king and the rook stand where a
/// right needs them: on their home squares, or in Chess960 on the squares the
/// record names. Otherwise, and for every other game, this is
/// [`GameMoves::start`].
pub fn start_as_played(game: &GameMoves<'_>) -> Result<Start> {
    let start = game.start()?;
    let Start::Setup(mut s) = start else { return Ok(start) };
    for _ in 0..4 {
        if s.castling == 0x0f {
            break;
        }
        let (result, missing) = run(game, &Start::Setup(s.clone()), &mut Silent);
        let Some((color, side)) = missing.filter(|_| result.is_err()) else { break };
        let bit = match (color, side) {
            (CColor::White, CastleSide::Long) => 1,
            (CColor::White, CastleSide::Short) => 2,
            (CColor::Black, CastleSide::Long) => 4,
            (CColor::Black, CastleSide::Short) => 8,
        };
        if s.castling & bit != 0 {
            break;
        }
        // Only a right the position can hold is added: king and rook on the
        // squares it needs, the named ones when the record names them.
        s.castling |= bit;
        if !start_board(&Start::Setup(s.clone())).is_ok_and(|b| b.castling_rook(color, side).is_some()) {
            s.castling &= !bit;
            break;
        }
    }
    Ok(Start::Setup(s))
}

/// Walks the tree from `start`; also returns the castling right whose absence
/// stopped it, if one did.
fn run(
    game: &GameMoves<'_>,
    start: &Start,
    visitor: &mut impl TreeVisitor,
) -> (Result<TreeStats>, Option<(CColor, CastleSide)>) {
    let setup = || -> Result<(&'static [u8; 256], bool, bool, Board, Pieces)> {
        let (table, pre, simple) = mode(game.mode())?;
        let board = start_board(start)?;
        let pieces = Pieces::scan(&board)?;
        Ok((table, pre, simple, board, pieces))
    };
    let (table, pre, simple, board, pieces) = match setup() {
        Ok(v) => v,
        Err(e) => return (Err(e), None),
    };
    let mut w = Walker {
        visitor,
        board,
        pieces,
        stack: Vec::new(),
        stats: TreeStats { lines: 1, ..Default::default() },
        main: true,
        ended: false,
        branch_next: false,
        table,
        pre,
        missing_right: Cell::new(None),
    };
    let s = game.stream();
    let result = if simple { w.simple(s) } else { w.compact(s, game.is_chess960()) }.and_then(|()| {
        // A game without moves may be stored with no bytes at all.
        if !w.ended && !s.is_empty() {
            return Err(Error::Format("move stream: tree not terminated".into()));
        }
        Ok(w.stats)
    });
    (result, w.missing_right.get())
}
