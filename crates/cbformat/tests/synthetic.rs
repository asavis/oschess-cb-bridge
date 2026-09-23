//! Move records built by hand, following the format description, and read back.

use cbformat::fixture::{bytes, quiet, sq};
use cbformat::movetable::{self, Captured, Color, MoveWord, Piece};
use cbformat::pgn::movetext_of;
use cbformat::replay::{start_board, walk_tree};
use cbformat::v2::{GameMoves, Setup, Start};
use chesscore::{CastleSide, Color as CColor, Move as CMove, Square};

use Color::{Black as B, White as W};
use Piece::{Bishop, Knight, Pawn};

#[test]
fn variations_follow_the_documented_order() {
    // 1.e4 c5 (1...c6 2.d4) (1...Nf6 2.e5) 2.Nf3 d6 (2...Nc6 3.Bb5) 3.d4
    let (alt, end) = (movetable::ALTERNATIVE, movetable::END_OF_LINE);
    let stream = [
        movetable::MOVES,
        quiet(W, Pawn, "e2", "e4"),
        quiet(B, Pawn, "c7", "c5"),
        alt,
        quiet(W, Knight, "g1", "f3"),
        quiet(B, Pawn, "d7", "d6"),
        alt,
        quiet(W, Pawn, "d2", "d4"),
        end,
        quiet(B, Knight, "b8", "c6"),
        quiet(W, Bishop, "f1", "b5"),
        end,
        quiet(B, Pawn, "c7", "c6"),
        alt,
        quiet(W, Pawn, "d2", "d4"),
        end,
        quiet(B, Knight, "g8", "f6"),
        quiet(W, Pawn, "e4", "e5"),
        end,
    ];
    let content = bytes(&stream);
    let moves = GameMoves::parse(1, &content).unwrap();
    assert_eq!(moves.start().unwrap(), Start::Standard);
    assert_eq!(
        movetext_of(&moves).unwrap(),
        "1. e4 c5 (1... c6 2. d4) (1... Nf6 2. e5) 2. Nf3 d6 (2... Nc6 3. Bb5) 3. d4"
    );
    let stats = walk_tree(&moves, |_, _, _| {}).unwrap();
    assert_eq!((stats.main_line_plies, stats.total_plies, stats.lines), (5, 11, 4));
    assert_eq!(moves.main_line().count(), 5);
}

#[test]
fn empty_game() {
    let content = bytes(&[movetable::MOVES, movetable::END_OF_LINE]);
    let moves = GameMoves::parse(1, &content).unwrap();
    assert_eq!(movetext_of(&moves).unwrap(), "");
}

#[test]
fn truncated_tree_is_an_error() {
    let content = bytes(&[movetable::MOVES, quiet(W, Pawn, "e2", "e4")]);
    let moves = GameMoves::parse(1, &content).unwrap();
    assert!(walk_tree(&moves, |_, _, _| {}).is_err());
}

#[test]
fn wrong_capture_type_is_rejected() {
    // 1.e4 d5 2.exd5 written as capturing a knight rather than a pawn.
    let pawn_x_knight = MoveWord::Normal {
        color: W,
        piece: Pawn,
        from: sq("e4"),
        to: sq("d5"),
        captured: Captured::Knight,
        promotion: None,
    };
    let w = movetable::encode(pawn_x_knight).unwrap();
    let content =
        bytes(&[movetable::MOVES, quiet(W, Pawn, "e2", "e4"), quiet(B, Pawn, "d7", "d5"), w, movetable::END_OF_LINE]);
    let moves = GameMoves::parse(1, &content).unwrap();
    let err = walk_tree(&moves, |_, _, _| {}).unwrap_err().to_string();
    assert!(err.contains("move 3"), "{err}");
}

fn setup(castling: u8, ep_raw: u16) -> Setup {
    // White: Ke1, Rh1, Pe5. Black: Ke8, Pd5 (just played ...d7-d5). White to move.
    Setup {
        chess960: false,
        move_number: 20,
        side_to_move: W,
        castling,
        castling_rooks: [None; 4],
        en_passant_file: if (1..=8).contains(&ep_raw) { Some(ep_raw as u8 - 1) } else { None },
        en_passant_raw: ep_raw,
        pieces: vec![
            (sq("e1"), W, Piece::King),
            (sq("h1"), W, Piece::Rook),
            (sq("e5"), W, Pawn),
            (sq("e8"), B, Piece::King),
            (sq("d5"), B, Pawn),
        ],
    }
}

#[test]
fn set_up_en_passant() {
    let board = start_board(&Start::Setup(setup(0, 4))).unwrap();
    assert_eq!(board.en_passant(), Some("d6".parse::<Square>().unwrap()));
    assert!(board.is_legal("e5d6".parse::<CMove>().unwrap()));
}

#[test]
fn set_up_en_passant_that_cannot_exist_is_dropped() {
    // A file with no pawn that could just have made a double step, and values
    // out of range.
    for raw in [3u16, 12, 15, 0xffff] {
        let board = start_board(&Start::Setup(setup(0, raw))).unwrap();
        assert_eq!(board.en_passant(), None, "raw {raw}");
    }
}

#[test]
fn set_up_castling_rights_need_king_and_rook_at_home() {
    // Bits 1-8 all set: only white O-O has its king and rook in place.
    let board = start_board(&Start::Setup(setup(15, 0))).unwrap();
    assert_eq!(board.castling_rook(CColor::White, CastleSide::Short), Some(7));
    assert_eq!(board.castling_rook(CColor::White, CastleSide::Long), None);
    assert_eq!(board.castling_rook(CColor::Black, CastleSide::Short), None);
    assert!(board.is_legal("e1h1".parse::<CMove>().unwrap()));
}

#[test]
fn set_up_section_round_trip() {
    // fffb; move number; side to move | castling << 8; en passant file; pieces; fffc.
    let stream = [
        movetable::START_POSITION,
        20,
        2 << 8,
        4,
        0xc02d + 4 * 8,          // white king e1: piece 0, ChessBase square e1 = 4*8+0
        0xc02d + 4 * 64 + 7 * 8, // white rook h1: piece 4
        0xc2ad + 6 * 4 + 3,      // white pawn e5: file e, rank 5
        0xc16d + 4 * 8 + 7,      // black king e8
        0xc2dd + 6 * 3 + 3,      // black pawn d5
        movetable::MOVES,
        movetable::END_OF_LINE,
    ];
    let content = bytes(&stream);
    let moves = GameMoves::parse(1, &content).unwrap();
    let Start::Setup(s) = moves.start().unwrap() else { panic!("expected a set-up start") };
    assert_eq!(s, setup(2, 4));
    assert_eq!(format!("{}", start_board(&Start::Setup(s)).unwrap()), "4k3/8/8/3pP3/8/8/8/4K2R w K d6 0 20");
}

#[test]
fn san_disambiguation_and_check_suffixes() {
    use cbformat::pgn::san;
    use chesscore::{Board, Move};
    let cases = [
        // Two knights on one rank: the file tells them apart.
        ("7k/8/8/8/8/8/8/N1N4K w - - 0 1", "a1b3", "Nab3"),
        // Two knights on one file: the rank does.
        ("7k/8/8/N7/8/8/8/N6K w - - 0 1", "a1b3", "N1b3"),
        // A pinned knight cannot move there, so there is nothing to tell apart.
        ("4k3/8/8/b7/8/2N3N1/8/4K3 w - - 0 1", "g3e2", "Ne2"),
        // Three queens: one shares the rank, one the file, so both are needed.
        ("8/8/6k1/8/8/Q7/8/Q1Q4K w - - 0 1", "a1b2", "Qa1b2"),
        ("6k1/6pp/8/8/8/8/8/R5K1 w - - 0 1", "a1a8", "Ra8+"),
        ("6k1/5ppp/8/8/8/8/8/R5K1 w - - 0 1", "a1a8", "Ra8#"),
        ("4k3/8/8/8/8/8/8/R3K2R w KQ - 0 1", "e1h1", "O-O"),
        ("4k3/8/8/8/8/8/8/R3K2R w KQ - 0 1", "e1a1", "O-O-O"),
        ("4k3/1P6/8/8/8/8/8/4K3 w - - 0 1", "b7b8q", "b8=Q+"),
        ("4k3/8/8/3pP3/8/8/8/4K3 w - d6 0 2", "e5d6", "exd6"),
    ];
    for (fen, mv, want) in cases {
        let board: Board = fen.parse().unwrap();
        let mv: Move = mv.parse().unwrap();
        assert!(board.is_legal(mv), "{fen} {mv}");
        assert_eq!(san(&board, mv), want, "{fen}");
    }
}
