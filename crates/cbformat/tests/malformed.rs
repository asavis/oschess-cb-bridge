//! Damaged and hostile inputs must produce errors, never panics or silently
//! incomplete output.

use cbformat::fixture::{Builder, TempDb, bytes, lid_header, quiet, sq};
use cbformat::movetable::{self, Captured, CastleSide, Color, MoveWord, Piece};
use cbformat::pgn::{self, movetext_of};
use cbformat::replay::walk_tree;
use cbformat::v2::{Database, GameMoves};

const MOVES: u16 = movetable::MOVES;
const END: u16 = movetable::END_OF_LINE;
const ALT: u16 = movetable::ALTERNATIVE;
const NULL: u16 = movetable::NULL_MOVE;

// ------------------------------------------------------------- move trees

#[test]
fn deeply_nested_variations_export_without_overflowing_the_stack() {
    // Each group opens one more level of nesting; 100,000 levels.
    let mut words = vec![MOVES];
    for _ in 0..100_000 {
        words.extend([NULL, ALT, END, NULL]);
    }
    words.push(END);
    let content = bytes(&words);
    let moves = GameMoves::parse(1, &content).unwrap();
    let text = movetext_of(&moves).unwrap();
    assert_eq!(text.matches('(').count(), 100_000);
    assert_eq!(text.matches(')').count(), 100_000);
}

#[test]
fn export_refuses_what_verify_refuses() {
    let e4 = quiet(Color::White, Piece::Pawn, "e2", "e4");
    let cases: [(&str, Vec<u16>); 5] = [
        ("no final end of line", vec![MOVES, NULL]),
        ("variation left open", vec![MOVES, NULL, ALT, END]),
        ("word after the final end", vec![MOVES, END, 0xfffe]),
        ("alternative marker before any move", vec![MOVES, ALT, e4, END]),
        ("two alternative markers after one move", vec![MOVES, e4, ALT, ALT, END, END]),
    ];
    for (what, words) in cases {
        let content = bytes(&words);
        let moves = GameMoves::parse(1, &content).unwrap();
        assert!(walk_tree(&moves, |_, _, _| {}).is_err(), "verify accepts: {what}");
        assert!(movetext_of(&moves).is_err(), "export accepts: {what}");
    }
}

#[test]
fn king_taking_its_own_rook_is_not_castling() {
    // Chess960 set-up: white Kf1 Rg1 with the O-O right, black Ke8.
    let piece = |c, p, at| movetable::encode_piece_word(c, p, sq(at)).unwrap();
    let set_up = |mv: u16| {
        bytes(&[
            movetable::START_POSITION,
            1000,   // Chess960 game from a set-up position
            1,      // move number
            2 << 8, // white to move, white O-O
            0,      // no en passant
            piece(Color::White, Piece::King, "f1"),
            piece(Color::White, Piece::Rook, "g1"),
            piece(Color::Black, Piece::King, "e8"),
            MOVES,
            mv,
            END,
        ])
    };
    let king_takes_rook = movetable::encode(MoveWord::Normal {
        color: Color::White,
        piece: Piece::King,
        from: sq("f1"),
        to: sq("g1"),
        captured: Captured::Rook,
        promotion: None,
    })
    .unwrap();
    let content = set_up(king_takes_rook);
    let moves = GameMoves::parse(2, &content).unwrap();
    assert!(walk_tree(&moves, |_, _, _| {}).is_err());
    assert!(movetext_of(&moves).is_err());

    // The castling word itself is accepted.
    let castle = movetable::encode(MoveWord::Castle { color: Color::White, side: CastleSide::Short }).unwrap();
    let content = set_up(castle);
    let moves = GameMoves::parse(2, &content).unwrap();
    assert_eq!(movetext_of(&moves).unwrap(), "1. O-O");
}

// ------------------------------------------------------- whole databases

/// A one-game database (1.e4) with the `.2lid` file `lid`; `edit` adjusts
/// the game record.
fn fixture(name: &str, lid: Vec<u8>, edit: impl FnOnce(&mut [u8])) -> TempDb {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END]);
    edit(b.game(e4));
    b.lid(lid);
    b.write(name)
}

fn set_player_ids(rec: &mut [u8], id: i64) {
    rec[0x18..0x20].copy_from_slice(&id.to_le_bytes());
    rec[0x20..0x28].copy_from_slice(&id.to_le_bytes());
}

#[test]
fn negative_container_size_is_refused() {
    let f = fixture("negative-container", lid_header(-185, 2), |r| set_player_ids(r, 1));
    assert!(Database::open(f.base()).is_err());
}

#[test]
fn entity_ids_beyond_the_file_read_as_missing() {
    for id in [1, 1 << 40, i64::MAX - 1] {
        let f = fixture(&format!("far-id-{id}"), lid_header(1024, i64::MAX), |r| set_player_ids(r, id));
        let db = Database::open(f.base()).unwrap();
        assert_eq!(db.entities().player(id).unwrap(), None);
        let text = pgn::game(&db, 1).unwrap();
        assert!(text.contains("[White \"?\"]"), "{text}");
    }
}

#[test]
fn an_entity_file_truncated_after_opening_is_an_error() {
    // Player 0, the white and black of the game: "Tester, Ann".
    let mut lid = lid_header(1024, 1);
    let mut player = Vec::new();
    for s in [&b"Tester"[..], b"Ann"] {
        player.extend((s.len() as i32).to_le_bytes());
        player.extend(s);
    }
    lid.extend((player.len() as i32).to_le_bytes());
    lid.extend(&player);
    let f = fixture("truncated-lid", lid, |_| {});
    let db = Database::open(f.base()).unwrap();
    let text = pgn::game(&db, 1).unwrap();
    assert!(text.contains("[White \"Tester, Ann\"]"), "{text}");
    std::fs::OpenOptions::new().write(true).open(f.dir().join("db.2lid")).unwrap().set_len(184).unwrap();
    assert!(db.entities().player(0).is_err());
    assert!(pgn::game(&db, 1).is_err());
}

#[test]
fn malformed_eco_is_left_out_of_the_pgn() {
    for v in [1u16, 127, 64128] {
        let f = fixture(&format!("eco-{v}"), lid_header(1024, 0), |r| r[0x80..0x82].copy_from_slice(&v.to_le_bytes()));
        let db = Database::open(f.base()).unwrap();
        let text = pgn::game(&db, 1).unwrap();
        assert!(!text.contains("[ECO"), "{text}");
        assert!(text.ends_with("1. e4 1-0\n"), "{text}");
    }
}

fn words(hex: &str) -> Vec<u8> {
    hex.split_whitespace().flat_map(|w| u16::from_str_radix(w, 16).unwrap().to_le_bytes()).collect()
}

#[test]
fn en_passant_is_dropped_when_a_check_predates_the_double_step() {
    // White Ke6 Pe5, black Ka8 Ra6 Pd5, white to move, en passant d6 stored:
    // the rook's check on e6 existed before ...d7-d5, so exd6 is not available.
    let content = words("fffb 0001 0000 0004 c052 c174 c272 c2f2 c2c8 fffc ad67 ffff");
    let moves = GameMoves::parse(1, &content).unwrap();
    let err = walk_tree(&moves, |_, _, _| {}).unwrap_err().to_string();
    assert!(err.contains("en passant e5d6 not available"), "{err}");
    assert!(movetext_of(&moves).is_err());
}

#[test]
fn set_up_positions_with_too_many_pieces_are_refused() {
    // White Ke1 with pawns a2-h2 and a3: nine pawns.
    let content = words("fffb 0001 0000 0000 c04d c194 c2ad c2b3 c2b9 c2bf c2c5 c2cb c2d1 c2d7 c2ae fffc ffff");
    let moves = GameMoves::parse(1, &content).unwrap();
    let err = walk_tree(&moves, |_, _, _| {}).unwrap_err().to_string();
    assert!(err.contains("set-up position"), "{err}");
    assert!(movetext_of(&moves).is_err());
}
