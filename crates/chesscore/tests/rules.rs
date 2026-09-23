//! The rules, case by case.

use chesscore::{Board, BoardBuilder, Color, FenError, IllegalMove, Move, Piece, SetupError, Square};

fn board(fen: &str) -> Board {
    fen.parse().unwrap_or_else(|e| panic!("{fen}: {e}"))
}

fn mv(s: &str) -> Move {
    s.parse().unwrap()
}

fn after(fen: &str, m: &str) -> Result<String, IllegalMove> {
    let mut b = board(fen);
    b.play_checked(mv(m)).map(|()| b.to_string())
}

// ------------------------------------------------------------ castling

#[test]
fn castling_both_sides() {
    let fen = "r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1";
    assert_eq!(after(fen, "e1h1").unwrap(), "r3k2r/8/8/8/8/8/8/R4RK1 b kq - 1 1");
    assert_eq!(after(fen, "e1a1").unwrap(), "r3k2r/8/8/8/8/8/8/2KR3R b kq - 1 1");
}

#[test]
fn castling_through_or_into_check_or_from_check() {
    // f1 attacked: no O-O; O-O-O unaffected.
    assert_eq!(after("4kr2/8/8/8/8/8/8/R3K2R w KQ - 0 1", "e1h1"), Err(IllegalMove::Castling));
    assert!(after("4kr2/8/8/8/8/8/8/R3K2R w KQ - 0 1", "e1a1").is_ok());
    // g1 attacked: no O-O.
    assert_eq!(after("4k1r1/8/8/8/8/8/8/R3K2R w KQ - 0 1", "e1h1"), Err(IllegalMove::Castling));
    // b1 attacked does not stop O-O-O: the king never crosses it.
    assert!(after("1r2k3/8/8/8/8/8/8/R3K2R w KQ - 0 1", "e1a1").is_ok());
    // In check: neither.
    assert_eq!(after("4k3/8/8/8/8/8/8/R3K1rR w KQ - 0 1", "e1a1"), Err(IllegalMove::Castling));
}

#[test]
fn castling_needs_an_empty_path_and_the_right() {
    assert_eq!(after("4k3/8/8/8/8/8/8/R2QK2R w KQ - 0 1", "e1a1"), Err(IllegalMove::Castling));
    assert!(after("4k3/8/8/8/8/8/8/R2QK2R w KQ - 0 1", "e1h1").is_ok());
    assert_eq!(after("4k3/8/8/8/8/8/8/R3K2R w Q - 0 1", "e1h1"), Err(IllegalMove::Castling));
    // After the rook moves away and back, the right is gone.
    let mut b = board("4k3/8/8/8/8/8/8/R3K2R w KQ - 0 1");
    for m in ["h1h2", "e8e7", "h2h1", "e7e8"] {
        b.play_checked(mv(m)).unwrap();
    }
    assert_eq!(b.clone().play_checked(mv("e1h1")), Err(IllegalMove::Castling));
    assert!(b.play_checked(mv("e1a1")).is_ok());
}

#[test]
fn capturing_a_rook_removes_that_castling_right() {
    let mut b = board("r3k2r/8/8/8/8/8/6B1/4K3 w kq - 0 1");
    b.play_checked(mv("g2a8")).unwrap();
    assert_eq!(b.to_string(), "B3k2r/8/8/8/8/8/8/4K3 b k - 0 1");
}

#[test]
fn chess960_castling() {
    // King already on its destination.
    assert_eq!(after("4k3/8/8/8/8/8/8/6KR w H - 0 1", "g1h1").unwrap(), "4k3/8/8/8/8/8/8/5RK1 b - - 1 1");
    // The king crosses the rook's destination and the rook the king's.
    assert_eq!(after("4k3/8/8/8/8/8/8/1R2K3 w B - 0 1", "e1b1").unwrap(), "4k3/8/8/8/8/8/8/2KR4 b - - 1 1");
    // The castling rook shields the king's destination from the queen on a1.
    assert_eq!(after("4k3/8/8/8/8/8/8/qR3K2 w B - 0 1", "f1b1"), Err(IllegalMove::Castling));
    // A king move onto a square next to it is not castling.
    assert!(after("4k3/8/8/8/8/8/8/1R2K3 w B - 0 1", "e1d1").is_ok());
}

#[test]
fn a_king_move_onto_its_own_rook_without_the_right_is_refused() {
    assert_eq!(after("4k3/8/8/8/8/8/8/4KR2 w - - 0 1", "e1f1"), Err(IllegalMove::Castling));
}

// ---------------------------------------------------------- en passant

#[test]
fn en_passant() {
    let fen = "4k3/8/8/3pP3/8/8/8/4K3 w - d6 0 2";
    assert_eq!(after(fen, "e5d6").unwrap(), "4k3/8/3P4/8/8/8/8/4K3 b - - 0 2");
    // Only straight after the double step.
    let mut b = board("4k3/3p4/8/4P3/8/8/8/4K3 b - - 0 1");
    b.play_checked(mv("d7d5")).unwrap();
    assert_eq!(b.en_passant(), Some("d6".parse().unwrap()));
    b.play_checked(mv("e1e2")).unwrap();
    b.play_checked(mv("e8e7")).unwrap();
    assert_eq!(b.play_checked(mv("e5d6")), Err(IllegalMove::Unreachable));
}

#[test]
fn en_passant_exposing_the_king_along_the_rank() {
    // Both pawns leave the fifth rank, opening it to the rook.
    let b = board("8/8/8/K2pP2r/8/8/8/4k3 w - d6 0 1");
    assert_eq!(b.en_passant(), Some("d6".parse().unwrap()), "pseudo-legally possible, so hashed");
    assert!(!b.is_legal(mv("e5d6")));
    assert!(b.legal_moves().iter().all(|m| m.to_string() != "e5d6"));
}

#[test]
fn en_passant_key_only_when_a_capture_is_possible() {
    let mut b = Board::startpos();
    b.play_checked(mv("e2e4")).unwrap();
    assert_eq!(b.en_passant_file(), Some(4));
    assert_eq!(b.en_passant(), None, "no black pawn beside e4");
    assert_eq!(b.to_string(), "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq e3 0 1");
}

// ---------------------------------------------------------- promotions

#[test]
fn promotions() {
    let fen = "2r1k3/1P6/8/8/8/8/8/4K3 w - - 0 1";
    assert_eq!(after(fen, "b7b8"), Err(IllegalMove::Promotion));
    assert_eq!(after(fen, "b7b8q").unwrap(), "1Qr1k3/8/8/8/8/8/8/4K3 b - - 0 1");
    assert_eq!(after(fen, "b7c8n").unwrap(), "2N1k3/8/8/8/8/8/8/4K3 b - - 0 1");
    let to_king = Move::new("b7".parse().unwrap(), "b8".parse().unwrap(), Some(Piece::King));
    assert_eq!(board(fen).play_checked(to_king), Err(IllegalMove::Promotion));
    assert_eq!(after("4k3/8/8/8/8/8/4P3/4K3 w - - 0 1", "e2e3q"), Err(IllegalMove::Promotion));
    assert_eq!(after("4k3/8/8/8/8/8/8/R3K3 w - - 0 1", "a1a2q"), Err(IllegalMove::Promotion));
}

// ------------------------------------------------------- other refusals

#[test]
fn refusals() {
    let start = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
    assert_eq!(after(start, "e7e5"), Err(IllegalMove::NoPiece));
    assert_eq!(after(start, "e3e4"), Err(IllegalMove::NoPiece));
    assert_eq!(after(start, "g1g3"), Err(IllegalMove::Unreachable));
    assert_eq!(after(start, "f1c4"), Err(IllegalMove::Unreachable), "blocked by e2");
    assert_eq!(after(start, "e2e5"), Err(IllegalMove::Unreachable));
    assert_eq!(after(start, "d1d2"), Err(IllegalMove::Occupied));
    assert_eq!(
        after("4k3/8/8/8/8/4p3/4P3/4K3 w - - 0 1", "e2e3"),
        Err(IllegalMove::Unreachable),
        "no straight capture"
    );
    assert_eq!(after("4k3/8/8/8/8/8/3p4/4K3 w - - 0 1", "e2d3"), Err(IllegalMove::NoPiece));
    assert_eq!(after("4k3/4r3/8/8/8/8/4B3/4K3 w - - 0 1", "e2d3"), Err(IllegalMove::LeavesKingInCheck), "pinned");
    assert_eq!(after("4k3/8/8/8/8/8/3r4/4K3 w - - 0 1", "e1d1"), Err(IllegalMove::LeavesKingInCheck));
}

#[test]
fn null_move() {
    let b = board("4k3/8/8/3pP3/8/8/8/4K3 w - d6 0 2");
    let n = b.null_move().unwrap();
    assert_eq!(n.to_string(), "4k3/8/8/3pP3/8/8/8/4K3 b - - 1 2");
    assert_eq!(n.hash(), n.hash_from_scratch());
    assert!(board("4k3/8/8/8/8/8/4r3/4K3 w - - 0 1").null_move().is_none());
}

#[test]
fn clocks() {
    let mut b = Board::startpos();
    b.play_checked(mv("g1f3")).unwrap();
    assert_eq!((b.halfmove_clock(), b.fullmove_number()), (1, 1));
    b.play_checked(mv("g8f6")).unwrap();
    assert_eq!((b.halfmove_clock(), b.fullmove_number()), (2, 2));
    b.play_checked(mv("e2e4")).unwrap();
    assert_eq!((b.halfmove_clock(), b.fullmove_number()), (0, 2));
}

// ------------------------------------------------------------ positions

#[test]
fn set_up_positions_are_validated() {
    let err = |fen: &str| match Board::from_fen(fen) {
        Err(FenError::Setup(e)) => e,
        other => panic!("{fen}: {other:?}"),
    };
    assert_eq!(err("4k3/8/8/8/8/8/8/4KK2 w - - 0 1"), SetupError::KingCount);
    assert_eq!(err("8/8/8/8/8/8/8/4K3 w - - 0 1"), SetupError::KingCount);
    assert_eq!(err("P3k3/8/8/8/8/8/8/4K3 w - - 0 1"), SetupError::PawnOnBackRank);
    assert_eq!(err("4k3/4R3/8/8/8/8/8/4K3 w - - 0 1"), SetupError::OpponentInCheck);
    assert_eq!(err("4k3/8/8/8/8/8/8/4K3 w K - 0 1"), SetupError::Castling);
    assert_eq!(err("4k3/8/8/8/8/8/8/4K3 w - e6 0 1"), SetupError::EnPassant);
    assert!(matches!(Board::from_fen("4k3/8/8/8/8/8/8/4K3 x - - 0 1"), Err(FenError::Syntax(_))));
    assert!(matches!(Board::from_fen("4k3/8/8/8/8/8/8/4K4 w - - 0 1"), Err(FenError::Syntax(_))));

    let mut builder = BoardBuilder::empty();
    builder.set("e1".parse().unwrap(), Some((Piece::King, Color::White)));
    builder.set("e8".parse().unwrap(), Some((Piece::King, Color::Black)));
    builder.castling[Color::White.index()][0] = Some(7);
    assert!(!builder.castling_is_valid(Color::White, chesscore::CastleSide::Short));
    assert_eq!(builder.build(), Err(SetupError::Castling));
    builder.castling = [[None; 2]; 2];
    assert!(builder.build().is_ok());
}

#[test]
fn fen_round_trips() {
    for fen in [
        "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
        "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
        "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
        "4k3/8/8/3pP3/8/8/8/4K3 w - d6 0 2",
    ] {
        assert_eq!(board(fen).to_string(), fen);
    }
    // Chess960, written with rook files.
    let b = board("bqnb1rkr/pp3ppp/3ppn2/2p5/5P2/P2P4/NPP1P1PP/BQ1BNRKR w HFhf - 2 9");
    assert!(b.is_chess960());
    assert_eq!(b.shredder_fen(), "bqnb1rkr/pp3ppp/3ppn2/2p5/5P2/P2P4/NPP1P1PP/BQ1BNRKR w HFhf - 2 9");
    assert_eq!(b.to_string(), "bqnb1rkr/pp3ppp/3ppn2/2p5/5P2/P2P4/NPP1P1PP/BQ1BNRKR w KQkq - 2 9");
    // Missing clocks default to 0 and 1.
    assert_eq!(board("4k3/8/8/8/8/8/8/4K3 b - -").to_string(), "4k3/8/8/8/8/8/8/4K3 b - - 0 1");
}

#[test]
fn x_fen_names_a_castling_rook_that_is_not_the_outermost() {
    // The f1 rook has the right although h1 holds a rook farther out.
    let fen = "4k3/8/8/8/8/8/8/4KR1R w F - 0 1";
    let b = board(fen);
    assert_eq!(b.to_string(), fen);
    assert_eq!(b.shredder_fen(), fen);
    assert!(b.is_legal(mv("e1f1")));
    assert!(board(&b.to_string()).is_legal(mv("e1f1")));
    // The outermost rook is still written with a letter.
    assert_eq!(board("4k3/8/8/8/8/8/8/R3KR1R w HA - 0 1").to_string(), "4k3/8/8/8/8/8/8/R3KR1R w KQ - 0 1");
}

#[test]
fn malformed_fen_is_refused() {
    let syntax = |fen: &str| matches!(Board::from_fen(fen), Err(FenError::Syntax(_)));
    // En passant square on the wrong rank for the side to move.
    for fen in
        ["4k3/8/8/3pP3/8/8/8/4K3 w - d1 0 2", "4k3/8/8/3pP3/8/8/8/4K3 w - d3 0 2", "4k3/8/8/8/3Pp3/8/8/4K3 b - d6 0 2"]
    {
        assert!(syntax(fen), "{fen}");
    }
    assert!(board("4k3/8/8/8/3Pp3/8/8/4K3 b - d3 0 2").is_legal(mv("e4d3")));
    // Oversized input is refused without being collected.
    assert!(syntax(&"x ".repeat(4_000_000)));
    assert!(syntax(&format!("{} w - -", "/".repeat(4_000_000))));
    assert!(syntax(&format!("4k3/8/8/8/8/8/8/R3K2R w {} - 0 1", "K".repeat(1_000_000))));
    assert!(syntax("4k3/8/8/8/8/8/8/4K3 w - - 0 1 extra"));
    // Every field, oversized: refused, with a short message.
    let huge = "\u{1}".repeat(1_000_000);
    for fen in [
        format!("4k3/8/8/8/8/8/8/4K3 {} - - 0 1", "x".repeat(1_000_000)),
        format!("4k3/8/8/8/8/8/8/4K3 w - {huge} 0 1"),
        format!("4k3/8/8/8/8/8/8/4K3 w - - {} 1", "9".repeat(1_000_000)),
        format!("4k3/8/8/8/8/8/8/4K3 w - - 0 {}", "9".repeat(1_000_000)),
    ] {
        let err = Board::from_fen(&fen).unwrap_err().to_string();
        assert!(err.len() < 100, "{} bytes: {}", err.len(), &err[..100.min(err.len())]);
    }
    // Short but wrong fields still name what is wrong.
    assert!(Board::from_fen("4k3/8/8/8/8/8/8/4K3 x - - 0 1").unwrap_err().to_string().contains("bad side \"x\""));
    assert!(Board::from_fen("4k3/8/8/8/8/8/8/4K3 w - - z 1").unwrap_err().to_string().contains("bad number \"z\""));
    let long_square = "a1".repeat(1_000).parse::<Square>().unwrap_err().to_string();
    assert!(long_square.len() < 40, "{long_square}");
}

#[test]
fn chess960_start_positions() {
    let standard = Board::chess960(518).unwrap();
    assert_eq!(standard.to_string(), Board::startpos().to_string());
    assert_eq!(standard.hash(), Board::startpos().hash());
    assert_eq!(Board::chess960(0).unwrap().shredder_fen(), "bbqnnrkr/pppppppp/8/8/8/8/PPPPPPPP/BBQNNRKR w HFhf - 0 1");
    assert!(Board::chess960(960).is_none());
}

// -------------------------------------------------------------- hashing

#[test]
fn polyglot_test_vectors() {
    // From the Polyglot book format description; confirmed with python-chess.
    let vectors: [(&str, u64); 9] = [
        ("", 0x463b96181691fc9c),
        ("e2e4", 0x823c9b50fd114196),
        ("e2e4 d7d5", 0x0756b94461c50fb0),
        ("e2e4 d7d5 e4e5", 0x662fafb965db29d4),
        ("e2e4 d7d5 e4e5 f7f5", 0x22a48b5a8e47ff78),
        ("e2e4 d7d5 e4e5 f7f5 e1e2", 0x652a607ca3f242c1),
        ("e2e4 d7d5 e4e5 f7f5 e1e2 e8f7", 0x00fdd303c946bdd9),
        ("a2a4 b7b5 h2h4 b5b4 c2c4", 0x3c8123ea7b067637),
        ("a2a4 b7b5 h2h4 b5b4 c2c4 b4c3 a1a3", 0x5c3f9b829b279560),
    ];
    for (moves, key) in vectors {
        let mut b = Board::startpos();
        for m in moves.split_whitespace() {
            b.play_checked(mv(m)).unwrap();
        }
        assert_eq!(b.hash(), key, "{moves}");
        assert_eq!(b.hash_from_scratch(), key, "{moves}");
    }
}

#[test]
fn square_helpers() {
    let e4: Square = "e4".parse().unwrap();
    assert_eq!((e4.file(), e4.rank(), e4.index()), (4, 3, 28));
}
