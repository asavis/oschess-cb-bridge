//! Damaged and hostile inputs must produce errors, never panics or silently
//! incomplete output.

use cbformat::Error;
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

/// `depth` variations open at once: each move but the last has an alternative
/// still to come, and every line then ends.
fn nested(depth: usize) -> Vec<u8> {
    let mut words = vec![MOVES];
    for _ in 0..depth {
        words.extend([NULL, ALT]);
    }
    words.extend(std::iter::repeat_n(END, depth + 1));
    bytes(&words)
}

#[test]
fn variation_nesting_is_bounded() {
    use cbformat::replay::MAX_VARIATION_DEPTH;
    let content = nested(MAX_VARIATION_DEPTH);
    let moves = GameMoves::parse(1, &content).unwrap();
    assert!(walk_tree(&moves, |_, _, _| {}).is_ok());
    let content = nested(MAX_VARIATION_DEPTH + 1);
    let moves = GameMoves::parse(1, &content).unwrap();
    let err = walk_tree(&moves, |_, _, _| {}).unwrap_err().to_string();
    assert!(err.contains("nested deeper than 1024"), "{err}");
    assert!(movetext_of(&moves).is_err());
    // A hostile record of a million open variations stops at the bound too.
    let content = nested(1 << 20);
    let moves = GameMoves::parse(1, &content).unwrap();
    assert!(walk_tree(&moves, |_, _, _| {}).is_err());
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

/// A player record: last name `last`, empty first name.
fn player_record(last: &[u8]) -> Vec<u8> {
    let mut r = (last.len() as i32).to_le_bytes().to_vec();
    r.extend(last);
    r.extend(0i32.to_le_bytes());
    let mut c = (r.len() as i32).to_le_bytes().to_vec();
    c.extend(r);
    c
}

#[test]
fn a_record_longer_than_the_limit_is_not_read() {
    // Player 0 is short, player 1 fills its 64 KiB container.
    let container = 64 << 10;
    let mut lid = lid_header(container, 2);
    let mut short = player_record(b"Tal");
    short.resize(container as usize, 0);
    let mut long = player_record(&vec![b'x'; container as usize - 12]);
    long.resize(container as usize, 0);
    lid.extend(short);
    lid.extend(long);
    let f = fixture("record-limit", lid, |_| {});
    let db = Database::open(f.base()).unwrap();
    let e = db.entities();
    assert_eq!(e.player_within(0, 64).unwrap().map(|p| p.last), Some("Tal".to_string()));
    assert_eq!(e.player_within(1, 4 << 10).unwrap(), None, "longer than the limit");
    assert_eq!(e.player(1).unwrap().map(|p| p.last.len()), Some(container as usize - 12), "no limit");
    assert_eq!(e.raw_within(0, 1, 4 << 10).unwrap(), None);
    assert_eq!(e.tournament_within(0, 16).unwrap(), None, "no tournament table");
    assert_eq!(e.title_within(0, 16).unwrap(), None, "no title table");
}

#[test]
fn stored_counts_are_bounded_by_the_file() {
    // The header claims i64::MAX players; the file holds one container, cut short.
    let mut lid = lid_header(1024, i64::MAX);
    lid.extend([0u8; 10]);
    let f = fixture("stored-count", lid, |_| {});
    let db = Database::open(f.base()).unwrap();
    assert_eq!(db.entities().stored_count(0), 1);
    assert_eq!(db.entities().stored_count(7), 0);
    let f = fixture("stored-count-none", lid_header(1024, i64::MAX), |_| {});
    assert_eq!(Database::open(f.base()).unwrap().entities().stored_count(0), 0);
}

/// Entity records read many at a time are the ones read one by one: players
/// in 64-byte containers beside 48-byte tournaments, so an id's block is 112
/// bytes, with an unused id, damaged lengths, a name longer than a small
/// limit, ids past the header's count, and a file ending inside player 9.
#[test]
fn entities_read_many_at_once_read_as_each_alone() {
    let (player, tournament) = (64usize, 48usize);
    let mut lid = 184i32.to_be_bytes().to_vec();
    lid.extend(2i32.to_be_bytes());
    for size in [player, tournament] {
        lid.extend((size as i32).to_be_bytes());
        lid.extend(12i64.to_be_bytes());
        lid.extend((-1i64).to_be_bytes());
    }
    lid.resize(184, 0);
    let field = |s: &[u8]| [(s.len() as i32).to_le_bytes().to_vec(), s.to_vec()].concat();
    let container = |record: Vec<u8>, length: i32, size: usize| {
        let mut c = length.to_le_bytes().to_vec();
        c.extend(record);
        c.resize(size, 0);
        c
    };
    for id in 0..10 {
        let name = format!("Player{id}").into_bytes();
        let record = [field(&name), field(b"Ann")].concat();
        let length = match id {
            1 => 0,
            2 => 1000,
            4 => -5,
            _ => record.len() as i32,
        };
        let record = if id == 3 { [field(&[b'x'; 40]), field(b"")].concat() } else { record };
        let length = if id == 3 { record.len() as i32 } else { length };
        let mut block = container(record, length, player);
        let place =
            [field(b"Wijk"), field(format!("Open {id}").as_bytes()), 20240101i32.to_le_bytes().to_vec()].concat();
        block.extend(container(place.clone(), place.len() as i32, tournament));
        if id == 9 {
            block.truncate(24);
        }
        lid.extend(block);
    }
    let f = fixture("entity-runs", lid, |_| {});
    let db = Database::open(f.base()).unwrap();
    let e = db.entities();
    let mut buf = vec![0u8; 1 << 12];
    for typ in [0, 1, 7] {
        for len in [0, 10, 63, 64, 112, 176, 400, 1 << 12] {
            for limit in [8, 20, 4 << 10] {
                for (first, end) in [(-1, 3), (0, 15), (3, 9), (8, 12), (9, 10), (12, 20), (5, 5)] {
                    let mut got = Vec::new();
                    let mut id = first;
                    while id < end {
                        let read = e
                            .read_within(typ, id..end, &mut buf[..len], limit, &mut |r| {
                                got.push(r.map(<[u8]>::to_vec));
                                Ok::<(), Error>(())
                            })
                            .unwrap();
                        assert!(read >= 1 && id + read as i64 <= end, "{typ} {len} {limit} {id}..{end}: {read}");
                        id += read as i64;
                    }
                    let each: Vec<_> = (first..end).map(|id| e.raw_within(typ, id, limit).unwrap()).collect();
                    assert_eq!(got, each, "type {typ}, {len}-byte buffer, limit {limit}, ids {first}..{end}");
                }
            }
        }
    }
    let mut read =
        |len: usize| e.read_within(0, 0..15, &mut buf[..len], 4 << 10, &mut |_| Ok::<(), Error>(())).unwrap();
    assert_eq!(read(400), 4, "three whole blocks and a player's container");
    assert_eq!(read(63), 1, "less than a container: one alone");
    assert_eq!(e.read_within(0, 5..5, &mut buf, 64, &mut |_| Ok::<(), Error>(())).unwrap(), 0);
    let mut players = Vec::new();
    e.read_players_within(0..12, &mut buf, 4 << 10, &mut |p| {
        players.push(p);
        Ok::<(), Error>(())
    })
    .unwrap();
    assert_eq!(players, (0..12).map(|id| e.player_within(id, 4 << 10).unwrap()).collect::<Vec<_>>());
    assert_eq!(players[0].as_ref().map(|p| p.last.as_str()), Some("Player0"));
    let mut tournaments = Vec::new();
    e.read_tournaments_within(0..12, &mut buf, 4 << 10, &mut |t| {
        tournaments.push(t);
        Ok::<(), Error>(())
    })
    .unwrap();
    assert_eq!(tournaments, (0..12).map(|id| e.tournament_within(id, 4 << 10).unwrap()).collect::<Vec<_>>());
    assert_eq!(tournaments[8].as_ref().map(|t| t.title.as_str()), Some("Open 8"));
}

/// A block larger than 64 KiB, which no real file has, is read a container
/// at a time, so that a small limit bounds what is read for each id.
#[test]
fn huge_entity_blocks_are_read_one_container_at_a_time() {
    let container = 64 << 10;
    let mut lid = lid_header(container + 1, 3);
    for id in 0..3 {
        let mut c = player_record(format!("P{id}").as_bytes());
        c.resize(container as usize + 1, 0);
        lid.extend(c);
    }
    let f = fixture("entity-huge-blocks", lid, |_| {});
    let db = Database::open(f.base()).unwrap();
    let mut buf = vec![0u8; 1 << 20];
    let mut names = Vec::new();
    let read = db
        .entities()
        .read_players_within(0..3, &mut buf, 64, &mut |p| {
            names.push(p.map(|p| p.last));
            Ok::<(), Error>(())
        })
        .unwrap();
    assert_eq!(read, 1);
    assert_eq!(names, [Some("P0".to_string())]);
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
    let each = &mut |_| Ok::<(), cbformat::Error>(());
    assert!(db.entities().read_players_within(0..1, &mut [0u8; 4096], 64, each).is_err());
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
