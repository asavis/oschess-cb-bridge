//! Classic (`.cbh`) move records and databases built from the format
//! description by the fixture's own encoder, and read back.

use cbformat::cbh::{self, GameMoves};
use cbformat::fixture_cbh::{Tok, encode, move_record, start_position};
use cbformat::replay::{TreeStats, TreeVisitor};
use cbformat::v2::Start;
use chesscore::{Board, Color, Move, Piece};

#[derive(Default)]
struct Played(Vec<String>);

impl TreeVisitor for Played {
    fn play(&mut self, _: &Board, mv: Option<Move>, main: bool) {
        let m = mv.map_or("--".to_string(), |m| m.to_string());
        self.0.push(if main { m } else { format!("({m})") });
    }
}

fn walk(rec: &[u8]) -> cbformat::Result<(Vec<String>, TreeStats)> {
    let game = GameMoves::parse(rec)?;
    let mut p = Played::default();
    let stats = cbh::walk(&game, &mut p)?;
    Ok((p.0, stats))
}

use Tok::{End as E, Mv as M, Var as V};

/// `1.e4 c5 (1...c6 2.d4) (1...Nf6 2.e5) 2.Nf3 d6 (2...Nc6 3.Bb5) 3.d4`, the
/// example of the format description, in stored order.
fn documented() -> Vec<Tok<'static>> {
    vec![
        M("e2e4"),
        V,
        M("c7c5"),
        M("g1f3"),
        V,
        M("d7d6"),
        M("d2d4"),
        E,
        M("b8c6"),
        M("f1b5"),
        E,
        V,
        M("c7c6"),
        M("d2d4"),
        E,
        M("g8f6"),
        M("e4e5"),
        E,
    ]
}

#[test]
fn the_documented_example_reads_in_every_mode() {
    let expect = ["e2e4", "c7c5", "g1f3", "d7d6", "d2d4", "(b8c6)", "(f1b5)", "(c7c6)", "(d2d4)", "(g8f6)", "(e4e5)"];
    for (mode, two) in [(0, false), (0, true), (4, false), (4, true), (5, false)] {
        let stream = encode(&Board::startpos(), &documented(), mode, two);
        if mode == 0 && !two {
            assert_eq!(stream.len(), 18, "one byte per move and marker");
        }
        let (played, stats) = walk(&move_record(mode, None, None, &stream)).unwrap();
        assert_eq!(played, expect, "mode {mode}, two-byte {two}");
        assert_eq!((stats.main_line_plies, stats.total_plies, stats.lines), (5, 11, 4));
    }
}

/// xorshift64*, for reproducible random trees.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// A token for `mv` in `b`: castling (the king taking its own rook) as `O-O`.
fn token(b: &Board, mv: Move) -> String {
    match (b.piece_at(mv.from), b.piece_at(mv.to)) {
        (Some((Piece::King, c)), Some((Piece::Rook, r))) if c == r => {
            if mv.to.file() > mv.from.file() {
                "O-O".into()
            } else {
                "O-O-O".into()
            }
        }
        _ => mv.to_string(),
    }
}

/// A random tree below `b` in stored order, with the moves in the order the
/// walk must report them.
fn tree(rng: &mut Rng, b: &Board, plies: u32, out: &mut Vec<String>, moves: &mut Vec<Move>) {
    if plies == 0 {
        return;
    }
    let mut legal = b.legal_moves();
    if legal.is_empty() {
        return;
    }
    let k = if rng.below(8) == 0 { 2 + rng.below(2) } else { 1 };
    let mut alts = Vec::new();
    for _ in 0..k.min(legal.len()) {
        alts.push(legal.swap_remove(rng.below(legal.len())));
    }
    let null = rng.below(40) == 0 && !b.in_check();
    for (i, mv) in alts.iter().enumerate() {
        let last = i + 1 == alts.len();
        if !last {
            out.push("V".into());
        }
        let (tok, next) = if null && last {
            ("--".to_string(), b.null_move().unwrap())
        } else {
            let mut n = b.clone();
            n.play_checked(*mv).unwrap();
            (token(b, *mv), n)
        };
        out.push(tok.clone());
        moves.push(if tok == "--" { Move::new(mv.from, mv.from, None) } else { *mv });
        // The first alternative continues the main line; the others are
        // variations, kept short so the tree stays small.
        let depth = if i == 0 { plies - 1 } else { (plies - 1).min(6) };
        tree(rng, &next, depth, out, moves);
        if !last {
            out.push("E".into());
        }
    }
}

fn toks(v: &[String]) -> Vec<Tok<'_>> {
    v.iter()
        .map(|s| match s.as_str() {
            "V" => V,
            "E" => E,
            m => M(m),
        })
        .collect()
}

/// Random trees with captures, promotions (a fourth queen needs two bytes),
/// en passant, castling and null moves: what the fixture's encoder writes
/// the reader must read back move for move.
#[test]
fn random_trees_round_trip() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    // How often the trees exercise each rule, to show that they do.
    let mut seen = [0u32; 4]; // promotions, castling, null moves, variations
    for game in 0..400 {
        let chess960 = game % 4 == 3;
        let start = if chess960 { Board::chess960(rng.below(960) as u16).unwrap() } else { Board::startpos() };
        let (mut stored, mut moves) = (Vec::new(), Vec::new());
        let plies = 60 + rng.below(80) as u32;
        tree(&mut rng, &start, plies, &mut stored, &mut moves);
        stored.push("E".into());
        seen[0] += stored.iter().filter(|t| t.len() == 5).count() as u32;
        seen[1] += stored.iter().filter(|t| t.starts_with("O-O")).count() as u32;
        seen[2] += stored.iter().filter(|t| *t == "--").count() as u32;
        seen[3] += stored.iter().filter(|t| *t == "V").count() as u32;
        let modes: &[u8] = if chess960 { &[10] } else { &[0, 4, 5] };
        for &mode in modes {
            let two = rng.below(3) == 0;
            let stream = encode(&start, &toks(&stored), mode, two);
            let (start_bytes, extra) = if chess960 {
                let s = chess960_start(&start);
                (Some(s.0), Some(s.1))
            } else {
                (None, None)
            };
            let flags = mode | if chess960 { 0x40 } else { 0 };
            let rec = move_record(flags, start_bytes.as_ref().map(|s| &s[..]), extra.as_ref().map(|e| &e[..]), &stream);
            let (played, stats) = walk(&rec).unwrap_or_else(|e| panic!("game {game} mode {mode}: {e}"));
            let expect: Vec<String> =
                moves.iter().map(|m| if m.from == m.to { "--".into() } else { m.to_string() }).collect();
            let got: Vec<String> =
                played.iter().map(|p| p.trim_matches(|c| c == '(' || c == ')').to_string()).collect();
            assert_eq!(got, expect, "game {game} mode {mode} two-byte {two}");
            assert_eq!(stats.total_plies as usize, moves.len());
        }
    }
    assert!(seen.iter().all(|&n| n > 20), "promotions, castling, null moves, variations: {seen:?}");
}

/// Captures renumber the pieces of a kind; a fourth queen, from a promotion,
/// is written in two bytes; a pawn keeps its number.
#[test]
fn piece_numbers_follow_captures_and_promotions() {
    use Color::{Black, White};
    let s = start_position(
        &[
            ("g1", Piece::King, White),
            ("a3", Piece::Queen, White),
            ("b3", Piece::Queen, White),
            ("c3", Piece::Queen, White),
            ("e7", Piece::Pawn, White),
            ("h6", Piece::King, Black),
            ("a8", Piece::Rook, Black),
        ],
        false,
        0,
        0,
    );
    let board = Board::from_fen("r7/4P3/7k/8/8/QQQ5/8/6K1 w - - 0 1").unwrap();
    let line = [
        "e7e8q", // the fourth queen: two bytes
        "a8e8",  // ...which black takes at once
        "a3a4",  // the first queen
        "e8e4", "b3b5", // the second queen
        "e4a4", // takes the first queen: the second becomes the first
        "b5b6", // the (new) first queen
        "h6h7", "c3c7", // the (new) second queen
    ];
    let mut toks: Vec<Tok<'_>> = line.iter().map(|m| M(m)).collect();
    toks.push(E);
    for mode in [0, 4] {
        let stream = encode(&board, &toks, mode, false);
        // One two-byte move (three bytes), eight one-byte moves, the end.
        assert_eq!(stream.len(), 3 + 8 + 1);
        let (played, _) = walk(&move_record(mode | 0x40, Some(&s), None, &stream)).unwrap();
        assert_eq!(played, line);
    }
}

/// The start position and Chess960 bytes of a Chess960 start board.
fn chess960_start(b: &Board) -> ([u8; 28], [u8; 8]) {
    let mut pieces = Vec::new();
    let names: Vec<String> = (0..64u8).map(|i| format!("{}{}", (b'a' + i % 8) as char, i / 8 + 1)).collect();
    for (i, n) in names.iter().enumerate() {
        if let Some((p, c)) = b.piece_at(n.parse().unwrap()) {
            pieces.push((i, p, c));
        }
    }
    let list: Vec<(&str, Piece, Color)> = pieces.iter().map(|&(i, p, c)| (names[i].as_str(), p, c)).collect();
    let n = (0..960).find(|&n| Board::chess960(n).as_ref() == Some(b)).unwrap();
    let mut extra = [0u8; 8];
    extra[6..8].copy_from_slice(&n.to_be_bytes());
    (start_position(&list, false, 0x0f, 0), extra)
}

#[test]
fn chess960_starts_are_reported_as_their_number() {
    let b = Board::chess960(518).unwrap();
    let (s, e) = chess960_start(&b);
    let stream = encode(&b, &[M("e2e4"), E], 10, false);
    let rec = move_record(0x4a, Some(&s), Some(&e), &stream);
    let g = GameMoves::parse(&rec).unwrap();
    assert!(g.is_chess960());
    assert_eq!(g.start().unwrap(), Start::Chess960(518));
    assert_eq!(walk(&rec).unwrap().0, ["e2e4"]);
}

/// White Ke1 Rh1, black Ke8, white to move; `castling` is the stored byte.
fn rook_ending(castling: u8, king: &str) -> [u8; 28] {
    use Color::{Black, White};
    start_position(
        &[(king, Piece::King, White), ("h1", Piece::Rook, White), ("e8", Piece::King, Black)],
        false,
        castling,
        0,
    )
}

#[test]
fn set_up_castling_without_a_stored_right_is_read_with_the_right() {
    let with_right = rook_ending(2, "e1");
    let stream =
        encode(&Board::from_fen("4k3/8/8/8/8/8/8/4K2R w K - 0 1").unwrap(), &[M("O-O"), M("e8d8"), E], 0, false);
    // Stored without the right, as older databases store set-ups.
    let rec = move_record(0x40, Some(&rook_ending(0, "e1")), None, &stream);
    let g = GameMoves::parse(&rec).unwrap();
    assert!(matches!(g.start().unwrap(), Start::Setup(s) if s.castling == 0));
    assert!(matches!(cbh::start_as_played(&g).unwrap(), Start::Setup(s) if s.castling == 2));
    assert_eq!(walk(&rec).unwrap().0, ["e1h1", "e8d8"]);
    assert_eq!(walk(&move_record(0x40, Some(&with_right), None, &stream)).unwrap().0, ["e1h1", "e8d8"]);
    // A king away from its square cannot have had the right: still an error.
    let rec = move_record(0x40, Some(&rook_ending(0, "f1")), None, &stream);
    assert!(walk(&rec).unwrap_err().to_string().contains("castling without the right"));
}

#[test]
fn null_moves_and_empty_games() {
    let stream = encode(&Board::startpos(), &[M("e2e4"), M("--"), M("d2d4"), E], 0, false);
    assert_eq!(walk(&move_record(0, None, None, &stream)).unwrap().0, ["e2e4", "--", "d2d4"]);
    let (played, stats) = walk(&move_record(0, None, None, &[])).unwrap();
    assert!(played.is_empty() && stats.total_plies == 0);
    let only_end = encode(&Board::startpos(), &[E], 0, false);
    assert!(walk(&move_record(0, None, None, &only_end)).unwrap().0.is_empty());
    // A null move while in check: black is checked by the rook on e1.
    use Color::{Black, White};
    let s = start_position(
        &[("e1", Piece::Rook, White), ("a1", Piece::King, White), ("e8", Piece::King, Black)],
        true,
        0,
        0,
    );
    // The null move's code does not depend on the position.
    let null = encode(&Board::startpos(), &[M("--"), E], 0, false);
    assert!(
        walk(&move_record(0x40, Some(&s), None, &null)).unwrap_err().to_string().contains("null move while in check")
    );
}

#[test]
fn stray_bytes_after_the_end_are_ignored_but_damage_is_refused() {
    let mut stream = encode(&Board::startpos(), &[M("e2e4"), E], 0, false);
    stream.extend([0x11, 0x22, 0x33]);
    assert_eq!(walk(&move_record(0, None, None, &stream)).unwrap().0, ["e2e4"]);
    let unended = encode(&Board::startpos(), &[M("e2e4")], 0, false);
    assert!(walk(&move_record(0, None, None, &unended)).unwrap_err().to_string().contains("not terminated"));
    let var_only = encode(&Board::startpos(), &[V], 0, false);
    assert!(walk(&move_record(0, None, None, &var_only)).is_err());
    // Code 235 with its two bytes missing, and an unknown mode.
    let two = encode(&Board::startpos(), &[M("e2e4"), E], 0, true);
    assert_eq!(two.len(), 4);
    assert!(walk(&move_record(0, None, None, &two[..2])).unwrap_err().to_string().contains("runs past"));
    assert!(walk(&move_record(1, None, None, &[0])).unwrap_err().to_string().contains("mode 1 is not supported"));
}

/// Every one-byte change and every truncation of a record with variations,
/// set-up and promotions gives a result or an error, never a panic.
#[test]
fn mutated_records_never_panic() {
    let mut rng = Rng(7);
    let (mut stored, mut moves) = (Vec::new(), Vec::new());
    tree(&mut rng, &Board::startpos(), 120, &mut stored, &mut moves);
    stored.push("E".into());
    for (flags, start) in [(0u8, None), (0x40, Some(rook_ending(0x0f, "e1")))] {
        let board = match start {
            None => Board::startpos(),
            Some(_) => Board::from_fen("4k3/8/8/8/8/8/8/4K2R w K - 0 1").unwrap(),
        };
        let toks_here: Vec<String> = if start.is_some() { vec!["O-O".into(), "E".into()] } else { stored.clone() };
        let stream = encode(&board, &toks(&toks_here), 0, false);
        let rec = move_record(flags, start.as_ref().map(|s| &s[..]), None, &stream);
        for i in 0..rec.len() {
            for v in [0u8, 0xff, rec[i] ^ 1, rec[i] ^ 0x80] {
                let mut m = rec.clone();
                m[i] = v;
                let _ = walk(&m);
            }
        }
        for n in 0..rec.len() {
            let mut m = rec[..n].to_vec();
            if n >= 4 {
                let size = (n as u32).to_be_bytes();
                m[1..4].copy_from_slice(&size[1..]);
            }
            let _ = walk(&m);
        }
    }
}
