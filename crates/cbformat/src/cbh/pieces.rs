//! The piece lists of the compact encoding. Pieces of a kind are numbered in
//! the order a scan of the start position finds them (`a1`, `a2`, … `h8`); a
//! captured piece other than a pawn makes the ones above it move down, and a
//! pawn keeps its number all game.

use chesscore::{Board, Color as CColor, Move, Piece as CPiece, Square};

use super::moves::cb_square;
use crate::{Error, Result};

/// The kinds the compact encoder numbers, in list order.
pub(super) const KINDS: [CPiece; 4] = [CPiece::Queen, CPiece::Rook, CPiece::Bishop, CPiece::Knight];

/// The pieces of one kind of one side, by ChessBase square, in encoding order.
#[derive(Clone, Copy, Default)]
pub(super) struct Kind {
    sq: [u8; 10],
    len: u8,
}

impl Kind {
    pub(super) fn get(&self, i: usize) -> Option<u8> {
        (i < self.len as usize).then(|| self.sq[i])
    }
    fn position(&self, s: u8) -> Option<usize> {
        self.sq[..self.len as usize].iter().position(|&x| x == s)
    }
    fn push(&mut self, s: u8) -> bool {
        let ok = (self.len as usize) < self.sq.len();
        if ok {
            self.sq[self.len as usize] = s;
            self.len += 1;
        }
        ok
    }
    fn remove(&mut self, s: u8) -> bool {
        let Some(i) = self.position(s) else { return false };
        self.sq.copy_within(i + 1..self.len as usize, i);
        self.len -= 1;
        true
    }
    fn relocate(&mut self, from: u8, to: u8) -> bool {
        let Some(i) = self.position(from) else { return false };
        self.sq[i] = to;
        true
    }
}

/// Queens, rooks, bishops and knights in encoding order, and the pawns by
/// their fixed numbers, for both sides.
#[derive(Clone, Copy)]
pub(super) struct Pieces {
    pub(super) kinds: [[Kind; 4]; 2],
    pub(super) pawns: [[Option<u8>; 8]; 2],
}

fn kind_index(p: CPiece) -> Option<usize> {
    KINDS.iter().position(|&k| k == p)
}

pub(super) fn to_cb(s: Square) -> u8 {
    s.file() * 8 + s.rank()
}

impl Pieces {
    pub(super) fn scan(board: &Board) -> Result<Self> {
        let mut p = Pieces { kinds: Default::default(), pawns: [[None; 8]; 2] };
        let mut next_pawn = [0usize; 2];
        for cb in 0..64u8 {
            let Some((piece, c)) = board.piece_at(cb_square(cb)) else { continue };
            let side = c.index();
            if piece == CPiece::Pawn {
                let slot = p.pawns[side].get_mut(next_pawn[side]).ok_or_else(|| lists("more than eight pawns"))?;
                *slot = Some(cb);
                next_pawn[side] += 1;
            } else if let Some(k) = kind_index(piece)
                && !p.kinds[side][k].push(cb)
            {
                return Err(lists("more than ten pieces of a kind"));
            }
        }
        Ok(p)
    }

    /// Follows `mv`, played by `us` from `before`, through the lists.
    pub(super) fn update(&mut self, before: &Board, us: CColor, mv: Move) -> Result<()> {
        let (me, them) = (us.index(), (!us).index());
        let (from, to) = (to_cb(mv.from), to_cb(mv.to));
        let moving = before.piece_at(mv.from).map(|(p, _)| p);
        let target = before.piece_at(mv.to);
        if moving == Some(CPiece::King) {
            // Castling is the king taking its own rook; the rook lands next to
            // the king's destination.
            if target == Some((CPiece::Rook, us)) {
                let file = if mv.to.file() > mv.from.file() { 5 } else { 3 };
                let rook_to = to_cb(Square::new(file, mv.to.rank()));
                return self.kinds[me][1].relocate(to, rook_to).then_some(()).ok_or_else(|| lists("castling rook"));
            }
        }
        let taken = match target {
            Some((p, c)) if c != us => Some((p, to)),
            _ if moving == Some(CPiece::Pawn) && mv.from.file() != mv.to.file() => {
                Some((CPiece::Pawn, to_cb(Square::new(mv.to.file(), mv.from.rank()))))
            }
            _ => None,
        };
        if let Some((p, at)) = taken {
            let removed = match kind_index(p) {
                Some(k) => self.kinds[them][k].remove(at),
                None => self.pawns[them].iter_mut().find(|s| **s == Some(at)).map(|s| *s = None).is_some(),
            };
            if !removed && p != CPiece::King {
                return Err(lists("captured piece"));
            }
        }
        let ok = match moving {
            Some(CPiece::Pawn) => match self.pawns[me].iter_mut().find(|s| **s == Some(from)) {
                Some(slot) => match mv.promotion {
                    Some(p) => {
                        *slot = None;
                        kind_index(p).is_some_and(|k| self.kinds[me][k].push(to))
                    }
                    None => {
                        *slot = Some(to);
                        true
                    }
                },
                None => false,
            },
            Some(p) => kind_index(p).is_none_or(|k| self.kinds[me][k].relocate(from, to)),
            None => false,
        };
        ok.then_some(()).ok_or_else(|| lists("moving piece"))
    }
}

fn lists(what: &str) -> Error {
    Error::Format(format!("piece lists: {what} does not match the board"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_variation_stack_stays_small() {
        // An open variation keeps a board and the piece lists.
        let entry = std::mem::size_of::<(Board, Pieces)>();
        assert!(entry * crate::cbh::MAX_VARIATION_DEPTH < 400 << 10, "{entry} bytes per open variation");
    }
}
