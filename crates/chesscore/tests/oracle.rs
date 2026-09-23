//! `cozy-chess` as an oracle: the same positions, moves and verdicts.

use chesscore::{Board, Move, Piece, Square};

/// A small deterministic generator, so a failure reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn ours(b: &Board) -> Vec<String> {
    let mut v: Vec<String> = b.legal_moves().iter().map(|m| m.to_string()).collect();
    v.sort();
    v
}

fn theirs(b: &cozy_chess::Board) -> Vec<String> {
    let mut v = Vec::new();
    b.generate_moves(|moves| {
        v.extend(moves.into_iter().map(|m| m.to_string()));
        false
    });
    v.sort();
    v
}

fn cozy_perft(b: &cozy_chess::Board, depth: u32) -> u64 {
    if depth == 0 {
        return 1;
    }
    let mut n = 0;
    b.generate_moves(|moves| {
        for m in moves {
            let mut next = b.clone();
            next.play_unchecked(m);
            n += cozy_perft(&next, depth - 1);
        }
        false
    });
    n
}

/// Compares everything observable about one position.
fn same_position(ours_b: &Board, cozy: &cozy_chess::Board, context: &str) {
    // cozy-chess stops the halfmove clock at 100; FEN does not, and neither
    // does chesscore. Compare it clamped and everything else exactly.
    let fen = ours_b.to_string();
    let (rest, full) = fen.rsplit_once(' ').unwrap();
    let (rest, half) = rest.rsplit_once(' ').unwrap();
    let clamped = format!("{rest} {} {full}", half.parse::<u16>().unwrap().min(100));
    assert_eq!(clamped, cozy.to_string(), "FEN, {context}");
    assert_eq!(ours(ours_b), theirs(cozy), "legal moves, {context}");
    assert_eq!(ours_b.in_check(), !cozy.checkers().is_empty(), "check, {context}");
    assert_eq!(ours_b.hash(), ours_b.hash_from_scratch(), "incremental hash, {context}");
}

/// Candidate moves of every kind, legal or not, from the side to move's pieces.
fn candidates(b: &Board, rng: &mut Rng, n: usize) -> Vec<Move> {
    let own: Vec<Square> = chesscore::squares(b.colors(b.side_to_move())).collect();
    (0..n)
        .map(|_| {
            let from = own[rng.below(own.len())];
            let to = Square::from_index(rng.below(64) as u8).unwrap();
            let promotion = match rng.below(8) {
                0 => Some(Piece::Queen),
                1 => Some(Piece::Knight),
                _ => None,
            };
            Move::new(from, to, promotion)
        })
        .collect()
}

fn playout(start: Board, cozy: cozy_chess::Board, rng: &mut Rng, plies: usize, label: &str) {
    let (mut b, mut c) = (start, cozy);
    for ply in 0..plies {
        let context = format!("{label} ply {ply} {b}");
        same_position(&b, &c, &context);
        // Verdicts on arbitrary moves, the path a damaged record would take.
        for m in candidates(&b, rng, 24) {
            let theirs = m.to_string().parse::<cozy_chess::Move>().is_ok_and(|cm| c.is_legal(cm));
            let mut copy = b.clone();
            assert_eq!(copy.play_checked(m).is_ok(), theirs, "verdict on {m}, {context}");
            assert_eq!(b.is_legal(m), theirs, "is_legal on {m}, {context}");
        }
        let moves = b.legal_moves();
        if moves.is_empty() {
            assert_eq!(b.is_checkmate(), !c.checkers().is_empty(), "mate, {context}");
            return;
        }
        let m = moves[rng.below(moves.len())];
        b.play_checked(m).unwrap_or_else(|e| panic!("{m}: {e}, {context}"));
        c.play(m.to_string().parse().unwrap());
    }
}

#[test]
fn random_games_from_the_standard_start() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for game in 0..200 {
        playout(Board::startpos(), cozy_chess::Board::default(), &mut rng, 200, &format!("game {game}"));
    }
}

#[test]
fn random_chess960_games() {
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    for game in 0..200 {
        let n = rng.below(960) as u16;
        let label = format!("chess960 {n} game {game}");
        playout(Board::chess960(n).unwrap(), cozy_chess::Board::chess960_startpos(n as u32), &mut rng, 200, &label);
    }
}

#[test]
fn every_chess960_start_position() {
    for n in 0..960u16 {
        same_position(&Board::chess960(n).unwrap(), &cozy_chess::Board::chess960_startpos(n as u32), &n.to_string());
    }
}

#[test]
fn perft_matches() {
    for (fen, depth) in [
        ("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1", 3),
        ("r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1", 3),
        ("8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1", 4),
        ("r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1", 3),
        ("rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8", 3),
        ("8/8/8/K2pP2r/8/8/8/4k3 w - d6 0 1", 4),
        ("bqnb1rkr/pp3ppp/3ppn2/2p5/5P2/P2P4/NPP1P1PP/BQ1BNRKR w HFhf - 2 9", 3),
        ("2nnrbkr/p1qppppp/8/1ppb4/6PP/3PP3/PPP2P2/BQNNRBKR w HEhe - 1 9", 3),
        ("b1q1rrkb/pppppppp/3nn3/8/P7/1PPP4/4PPPP/BQNNRKRB w GE - 1 9", 3),
    ] {
        let b: Board = fen.parse().unwrap();
        let c = cozy_chess::Board::from_fen(fen, b.is_chess960()).unwrap();
        assert_eq!(b.perft(depth), cozy_perft(&c, depth), "{fen}");
    }
}

/// The same checks at scale; run with `cargo test --release -p chesscore -- --ignored`.
#[test]
#[ignore]
fn many_random_games() {
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    for game in 0..5_000 {
        playout(Board::startpos(), cozy_chess::Board::default(), &mut rng, 300, &format!("game {game}"));
        let n = rng.below(960) as u16;
        let label = format!("chess960 {n} game {game}");
        playout(Board::chess960(n).unwrap(), cozy_chess::Board::chess960_startpos(n as u32), &mut rng, 300, &label);
    }
}
