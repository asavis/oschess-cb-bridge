//! Chess960 start positions by Scharnagl number.

use crate::board::Board;
use crate::types::Piece;

/// Pairs of the five squares left after the bishops and queen, for the two
/// knights, in Scharnagl order.
const KNIGHTS: [(usize, usize); 10] = [(0, 1), (0, 2), (0, 3), (0, 4), (1, 2), (1, 3), (1, 4), (2, 3), (2, 4), (3, 4)];

/// The back rank of Chess960 start position `n` (0-959); 518 is the
/// standard start position.
pub fn back_rank(n: u16) -> Option<[Piece; 8]> {
    if n >= 960 {
        return None;
    }
    let mut rank: [Option<Piece>; 8] = [None; 8];
    let mut n = n as usize;
    rank[2 * (n % 4) + 1] = Some(Piece::Bishop);
    n /= 4;
    rank[2 * (n % 4)] = Some(Piece::Bishop);
    n /= 4;
    let place = |rank: &mut [Option<Piece>; 8], nth: usize, piece: Piece| {
        let file = (0..8).filter(|&f| rank[f].is_none()).nth(nth).expect("enough empty squares");
        rank[file] = Some(piece);
    };
    place(&mut rank, n % 6, Piece::Queen);
    n /= 6;
    let (a, b) = KNIGHTS[n];
    // Place the second knight first so the first one's index is unaffected.
    place(&mut rank, b, Piece::Knight);
    place(&mut rank, a, Piece::Knight);
    for piece in [Piece::Rook, Piece::King, Piece::Rook] {
        place(&mut rank, 0, piece);
    }
    Some(rank.map(|p| p.expect("all eight squares filled")))
}

impl Board {
    /// Chess960 start position `n` (0-959), with castling in the Chess960
    /// form. Position 518 is the standard start position.
    pub fn chess960(n: u16) -> Option<Board> {
        back_rank(n).map(|rank| Board::from_back_rank(rank, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn letters(n: u16) -> String {
        back_rank(n).unwrap().iter().map(|p| p.letter()).collect()
    }

    #[test]
    fn known_positions() {
        assert_eq!(letters(518), "RNBQKBNR");
        assert_eq!(letters(0), "BBQNNRKR");
        assert_eq!(letters(959), "RKRNNQBB");
        assert!(back_rank(960).is_none());
    }

    #[test]
    fn every_position_is_distinct_and_legal() {
        let mut seen = std::collections::HashSet::new();
        for n in 0..960 {
            let r = back_rank(n).unwrap();
            assert!(seen.insert(r), "{n} repeats");
            let files = |p: Piece| (0..8).filter(move |&f| r[f] == p);
            let bishops: Vec<usize> = files(Piece::Bishop).collect();
            assert_eq!(bishops.len(), 2);
            assert_ne!(bishops[0] % 2, bishops[1] % 2, "{n}: bishops on one colour");
            let rooks: Vec<usize> = files(Piece::Rook).collect();
            let king = files(Piece::King).next().unwrap();
            assert!(rooks[0] < king && king < rooks[1], "{n}: king not between the rooks");
        }
    }
}
