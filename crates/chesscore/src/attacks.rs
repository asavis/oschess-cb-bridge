//! Attack tables, built at compile time.
//!
//! Leapers (knight, king, pawn) are looked up. Sliders use classical rays: a
//! ray from the square in each direction, cut at the first blocker, which is
//! the lowest set bit for directions that increase the square index and the
//! highest for those that decrease it.

use crate::types::{Bitboard, Color, Square};

const fn leaper(deltas: &[(i8, i8)]) -> [Bitboard; 64] {
    let mut t = [0; 64];
    let mut sq = 0;
    while sq < 64 {
        let (f, r) = ((sq % 8) as i8, (sq / 8) as i8);
        let mut i = 0;
        while i < deltas.len() {
            let (nf, nr) = (f + deltas[i].0, r + deltas[i].1);
            if nf >= 0 && nf < 8 && nr >= 0 && nr < 8 {
                t[sq] |= 1 << (nr * 8 + nf);
            }
            i += 1;
        }
        sq += 1;
    }
    t
}

const KNIGHT: [Bitboard; 64] = leaper(&[(1, 2), (2, 1), (2, -1), (1, -2), (-1, -2), (-2, -1), (-2, 1), (-1, 2)]);
const KING: [Bitboard; 64] = leaper(&[(1, 0), (1, 1), (0, 1), (-1, 1), (-1, 0), (-1, -1), (0, -1), (1, -1)]);
const PAWN: [[Bitboard; 64]; 2] = [leaper(&[(-1, 1), (1, 1)]), leaper(&[(-1, -1), (1, -1)])];

/// Directions as (file, rank) steps. The first four increase the square index.
const DIRECTIONS: [(i8, i8); 8] = [(0, 1), (1, 1), (1, 0), (-1, 1), (0, -1), (-1, -1), (-1, 0), (1, -1)];
const N: usize = 0;
const NE: usize = 1;
const E: usize = 2;
const NW: usize = 3;
const S: usize = 4;
const SW: usize = 5;
const W: usize = 6;
const SE: usize = 7;

const RAYS: [[Bitboard; 64]; 8] = {
    let mut t = [[0; 64]; 8];
    let mut d = 0;
    while d < 8 {
        let mut sq = 0;
        while sq < 64 {
            let (mut f, mut r) = ((sq % 8) as i8, (sq / 8) as i8);
            loop {
                f += DIRECTIONS[d].0;
                r += DIRECTIONS[d].1;
                if f < 0 || f >= 8 || r < 0 || r >= 8 {
                    break;
                }
                t[d][sq] |= 1 << (r * 8 + f);
            }
            sq += 1;
        }
        d += 1;
    }
    t
};

/// The squares strictly between two squares on a common line, or none.
static BETWEEN: [[Bitboard; 64]; 64] = {
    let mut t = [[0; 64]; 64];
    let mut from = 0;
    while from < 64 {
        let mut d = 0;
        while d < 8 {
            let (mut f, mut r) = ((from % 8) as i8, (from / 8) as i8);
            let mut acc: Bitboard = 0;
            loop {
                f += DIRECTIONS[d].0;
                r += DIRECTIONS[d].1;
                if f < 0 || f >= 8 || r < 0 || r >= 8 {
                    break;
                }
                let to = (r * 8 + f) as usize;
                t[from][to] = acc;
                acc |= 1 << to;
            }
            d += 1;
        }
        from += 1;
    }
    t
};

#[inline]
fn ray(d: usize, sq: Square, occupied: Bitboard) -> Bitboard {
    let r = RAYS[d][sq.index()];
    let blockers = r & occupied;
    if blockers == 0 {
        return r;
    }
    let first = if d < 4 { blockers.trailing_zeros() } else { 63 - blockers.leading_zeros() };
    r ^ RAYS[d][first as usize]
}

#[inline]
pub fn knight(sq: Square) -> Bitboard {
    KNIGHT[sq.index()]
}

#[inline]
pub fn king(sq: Square) -> Bitboard {
    KING[sq.index()]
}

/// The squares a pawn of `color` on `sq` attacks.
#[inline]
pub fn pawn(color: Color, sq: Square) -> Bitboard {
    PAWN[color.index()][sq.index()]
}

#[inline]
pub fn rook(sq: Square, occupied: Bitboard) -> Bitboard {
    ray(N, sq, occupied) | ray(E, sq, occupied) | ray(S, sq, occupied) | ray(W, sq, occupied)
}

#[inline]
pub fn bishop(sq: Square, occupied: Bitboard) -> Bitboard {
    ray(NE, sq, occupied) | ray(NW, sq, occupied) | ray(SW, sq, occupied) | ray(SE, sq, occupied)
}

#[inline]
pub fn queen(sq: Square, occupied: Bitboard) -> Bitboard {
    rook(sq, occupied) | bishop(sq, occupied)
}

/// The squares strictly between `a` and `b` if they share a rank, file or
/// diagonal, otherwise none.
#[inline]
pub fn between(a: Square, b: Square) -> Bitboard {
    BETWEEN[a.index()][b.index()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::squares;

    fn sq(s: &str) -> Square {
        s.parse().unwrap()
    }

    fn names(b: Bitboard) -> Vec<String> {
        squares(b).map(|s| s.to_string()).collect()
    }

    #[test]
    fn leapers() {
        assert_eq!(names(knight(sq("a1"))), ["c2", "b3"]);
        assert_eq!(knight(sq("d4")).count_ones(), 8);
        assert_eq!(names(king(sq("h8"))), ["g7", "h7", "g8"]);
        assert_eq!(names(pawn(Color::White, sq("a2"))), ["b3"]);
        assert_eq!(names(pawn(Color::Black, sq("e5"))), ["d4", "f4"]);
        assert_eq!(pawn(Color::White, sq("e8")), 0);
    }

    #[test]
    fn sliders_stop_at_the_first_blocker() {
        let occ = sq("d6").bit() | sq("f4").bit() | sq("b2").bit();
        assert_eq!(names(rook(sq("d4"), occ)), ["d1", "d2", "d3", "a4", "b4", "c4", "e4", "f4", "d5", "d6"]);
        assert_eq!(
            names(bishop(sq("d4"), occ)),
            ["g1", "b2", "f2", "c3", "e3", "c5", "e5", "b6", "f6", "a7", "g7", "h8"]
        );
        assert_eq!(rook(sq("a1"), 0).count_ones(), 14);
        assert_eq!(bishop(sq("a1"), 0).count_ones(), 7);
        assert_eq!(queen(sq("d4"), 0).count_ones(), 27);
    }

    #[test]
    fn squares_between() {
        assert_eq!(names(between(sq("a1"), sq("a4"))), ["a2", "a3"]);
        assert_eq!(names(between(sq("h8"), sq("e5"))), ["f6", "g7"]);
        assert_eq!(between(sq("a1"), sq("b3")), 0);
        assert_eq!(between(sq("e1"), sq("f1")), 0);
        assert_eq!(between(sq("e1"), sq("e1")), 0);
    }
}
