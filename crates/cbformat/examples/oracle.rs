//! Full-database differential run: every game and analysis replayed through
//! `chesscore` (the reader's `replay::walk`) and through an independent
//! `cozy-chess` replay, comparing the position after every move and the
//! verdict on every record.
//!
//! `cargo run --release --example oracle -- <database> [threads]`
//!
//! Built on the `cozy-chess` dev-dependency; never shipped.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use cbformat::movetable::{self, Captured, CastleSide, Color, MoveWord, Piece};
use cbformat::replay::{self, TreeVisitor};
use cbformat::v2::{Database, GameMoves, RecordKind, Start, Token};
use chesscore::{Board, CastleSide as Side, Color as CColor, Move, Piece as CPiece, squares};

/// Everything compared about a position.
#[derive(Clone, PartialEq, Eq, Debug)]
struct State {
    /// Per colour, per piece kind (pawn, knight, bishop, rook, queen, king).
    pieces: [[u64; 6]; 2],
    white_to_move: bool,
    /// Short and long castling rook files, white then black.
    castling: [Option<u8>; 4],
    en_passant_file: Option<u8>,
    halfmove_clamped: u16,
    fullmove: u16,
}

fn ours(b: &Board) -> State {
    let mut pieces = [[0; 6]; 2];
    for (c, color) in [CColor::White, CColor::Black].into_iter().enumerate() {
        for (p, piece) in CPiece::ALL.into_iter().enumerate() {
            pieces[c][p] = b.colored(piece, color);
        }
    }
    State {
        pieces,
        white_to_move: b.side_to_move() == CColor::White,
        castling: [
            b.castling_rook(CColor::White, Side::Short),
            b.castling_rook(CColor::White, Side::Long),
            b.castling_rook(CColor::Black, Side::Short),
            b.castling_rook(CColor::Black, Side::Long),
        ],
        en_passant_file: b.en_passant_file(),
        // cozy-chess stops the halfmove clock at 100; chesscore does not.
        halfmove_clamped: b.halfmove_clock().min(100),
        fullmove: b.fullmove_number(),
    }
}

fn theirs(b: &cozy_chess::Board) -> State {
    use cozy_chess::{Color as K, Piece as P};
    let mut pieces = [[0; 6]; 2];
    for (c, color) in [K::White, K::Black].into_iter().enumerate() {
        for (p, piece) in [P::Pawn, P::Knight, P::Bishop, P::Rook, P::Queen, P::King].into_iter().enumerate() {
            pieces[c][p] = b.colored_pieces(color, piece).0;
        }
    }
    let rights = |c: K| {
        let r = b.castle_rights(c);
        [r.short.map(|f| f as u8), r.long.map(|f| f as u8)]
    };
    let (w, bl) = (rights(K::White), rights(K::Black));
    State {
        pieces,
        white_to_move: b.side_to_move() == K::White,
        castling: [w[0], w[1], bl[0], bl[1]],
        en_passant_file: b.en_passant().map(|f| f as u8),
        halfmove_clamped: u16::from(b.halfmove_clock()).min(100),
        fullmove: b.fullmove_number(),
    }
}

/// The positions after each move of the tree, in stored order, from chesscore.
#[derive(Default)]
struct Recorder(Vec<State>);

impl TreeVisitor for Recorder {
    fn play(&mut self, _before: &Board, _mv: Option<Move>, _main_line: bool) {}
    fn played(&mut self, after: &Board) {
        self.0.push(ours(after));
    }
}

// ------------------------------------------------ the cozy-chess replay

fn cozy_start(start: &Start) -> Result<cozy_chess::Board, String> {
    use cozy_chess::{BoardBuilder, BoardBuilderError, Color as K, File, Piece as P, Rank, Square as Q};
    let color = |c: Color| if c == Color::White { K::White } else { K::Black };
    let piece = |p: Piece| match p {
        Piece::King => P::King,
        Piece::Queen => P::Queen,
        Piece::Knight => P::Knight,
        Piece::Bishop => P::Bishop,
        Piece::Rook => P::Rook,
        Piece::Pawn => P::Pawn,
    };
    match start {
        Start::Standard => Ok(cozy_chess::Board::default()),
        Start::Chess960(n) if *n < 960 => Ok(cozy_chess::Board::chess960_startpos(u32::from(*n))),
        Start::Chess960(n) => Err(format!("Chess960 position {n}")),
        Start::Setup(s) => {
            let mut b = BoardBuilder::empty();
            for &(sq, c, p) in &s.pieces {
                b.board[sq as usize] = Some((piece(p), color(c)));
            }
            b.side_to_move = color(s.side_to_move);
            for (c, long_bit, short_bit) in [(K::White, 1, 2), (K::Black, 4, 8)] {
                let rank = if c == K::White { Rank::First } else { Rank::Eighth };
                let on = |f: File, p: P| b.board[Q::new(f, rank) as usize] == Some((p, c));
                let Some(kf) = File::ALL.into_iter().find(|&f| on(f, P::King)) else { continue };
                let rooks: Vec<File> = File::ALL.into_iter().filter(|&f| on(f, P::Rook)).collect();
                let home = |f: File| (kf == File::E && rooks.contains(&f)).then_some(f);
                let rights = &mut b.castle_rights[c as usize];
                if s.castling & short_bit != 0 {
                    rights.short =
                        if s.chess960 { rooks.iter().copied().filter(|&f| f > kf).max() } else { home(File::H) };
                }
                if s.castling & long_bit != 0 {
                    rights.long =
                        if s.chess960 { rooks.iter().copied().filter(|&f| f < kf).min() } else { home(File::A) };
                }
            }
            if let Some(f) = s.en_passant_file.filter(|&f| f < 8) {
                let rank = if s.side_to_move == Color::White { Rank::Sixth } else { Rank::Third };
                b.en_passant = Some(Q::new(File::index(f as usize), rank));
            }
            b.fullmove_number = s.move_number.max(1);
            match b.build() {
                Ok(board) => Ok(board),
                Err(BoardBuilderError::InvalidEnPassant) => {
                    b.en_passant = None;
                    b.build().map_err(|e| format!("set-up position: {e:?}"))
                }
                Err(e) => Err(format!("set-up position: {e:?}")),
            }
        }
    }
}

fn cozy_move(b: &cozy_chess::Board, word: u16) -> Result<Option<cozy_chess::Move>, String> {
    use cozy_chess::{Color as K, Move as M, Piece as P, Rank, Square as Q};
    let color = |c: Color| if c == Color::White { K::White } else { K::Black };
    let back = |c: K| if c == K::White { Rank::First } else { Rank::Eighth };
    match movetable::decode(word).ok_or("not a move word")? {
        MoveWord::Null => Ok(None),
        MoveWord::Castle { color: c, side } | MoveWord::Castle960 { color: c, side, .. } => {
            let c = color(c);
            let rights = b.castle_rights(c);
            let rook = if side == CastleSide::Short { rights.short } else { rights.long }.ok_or("no right")?;
            let mv = M { from: b.king(c), to: Q::new(rook, back(c)), promotion: None };
            if b.side_to_move() != c || !b.is_legal(mv) {
                return Err("illegal castling".into());
            }
            Ok(Some(mv))
        }
        MoveWord::Normal { color: c, piece, from, to, captured, promotion } => {
            let c = color(c);
            let p = match piece {
                Piece::King => P::King,
                Piece::Queen => P::Queen,
                Piece::Knight => P::Knight,
                Piece::Bishop => P::Bishop,
                Piece::Rook => P::Rook,
                Piece::Pawn => P::Pawn,
            };
            let (from, to) = (Q::index(from as usize), Q::index(to as usize));
            if b.side_to_move() != c || !b.colored_pieces(c, p).has(from) || b.colors(c).has(to) {
                return Err("wrong piece or target".into());
            }
            let named = match captured {
                Captured::Nothing | Captured::EnPassant => None,
                Captured::Queen => Some(P::Queen),
                Captured::Knight => Some(P::Knight),
                Captured::Bishop => Some(P::Bishop),
                Captured::Rook => Some(P::Rook),
                Captured::Pawn => Some(P::Pawn),
            };
            if b.piece_on(to) != named {
                return Err("wrong capture".into());
            }
            if captured == Captured::EnPassant && b.en_passant() != Some(to.file()) {
                return Err("no en passant".into());
            }
            let promo = promotion.map(|p| match p {
                Piece::Queen => P::Queen,
                Piece::Knight => P::Knight,
                Piece::Bishop => P::Bishop,
                _ => P::Rook,
            });
            let mv = M { from, to, promotion: promo };
            if !b.is_legal(mv) {
                return Err("illegal".into());
            }
            Ok(Some(mv))
        }
    }
}

/// The positions after each move, in stored order, from cozy-chess, and
/// whether the whole tree was valid.
fn cozy_walk(moves: &GameMoves<'_>) -> (Vec<State>, bool) {
    let mut out = Vec::new();
    let Ok(start) = moves.start() else { return (out, false) };
    let Ok(mut board) = cozy_start(&start) else { return (out, false) };
    let mut stack: Vec<cozy_chess::Board> = Vec::new();
    let mut before_last: Option<cozy_chess::Board> = None;
    let mut ended = false;
    for token in moves.tokens() {
        if ended {
            return (out, false);
        }
        match token {
            Token::Move(w) => {
                let before = board.clone();
                match cozy_move(&board, w) {
                    Ok(Some(mv)) => board.play_unchecked(mv),
                    Ok(None) => match board.null_move() {
                        Some(b) => board = b,
                        None => return (out, false),
                    },
                    Err(_) => return (out, false),
                }
                out.push(theirs(&board));
                before_last = Some(before);
            }
            Token::Alternative => match before_last.take() {
                Some(b) => stack.push(b),
                None => return (out, false),
            },
            Token::EndOfLine => {
                before_last = None;
                match stack.pop() {
                    Some(b) => board = b,
                    None => ended = true,
                }
            }
        }
    }
    (out, ended)
}

// ------------------------------------------------------------------ main

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let db = Database::open(&args[1]).expect("open the database");
    let threads: usize = args.get(2).map_or(8, |t| t.parse().expect("thread count"));
    let n = db.record_count();
    let next = AtomicU64::new(1);
    let (records, plies, differences) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
    let examples: Mutex<Vec<String>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let first = next.fetch_add(4096, Ordering::Relaxed);
                    if first > u64::from(n) {
                        break;
                    }
                    let last = (first + 4095).min(u64::from(n)) as u32;
                    let Ok(batch) = db.batch(first as u32, last) else { continue };
                    for id in first as u32..=last {
                        let Ok(r) = batch.record(id) else { continue };
                        if !matches!(r.kind(), RecordKind::Game | RecordKind::Analysis) {
                            continue;
                        }
                        let Ok(data) = batch.moves_of(&r) else { continue };
                        let Ok(moves) = data.moves() else { continue };
                        let mut rec = Recorder::default();
                        let ours_ok = replay::walk(&moves, &mut rec).is_ok();
                        let (cozy_states, cozy_ok) = cozy_walk(&moves);
                        records.fetch_add(1, Ordering::Relaxed);
                        plies.fetch_add(rec.0.len() as u64, Ordering::Relaxed);
                        // chesscore stops at the first illegal move, cozy-chess
                        // just before it records one; compare what both played.
                        let common = rec.0.len().min(cozy_states.len());
                        let first_diff = (0..common).find(|&i| rec.0[i] != cozy_states[i]);
                        if ours_ok != cozy_ok || rec.0.len() != cozy_states.len() || first_diff.is_some() {
                            differences.fetch_add(1, Ordering::Relaxed);
                            let mut e = examples.lock().unwrap();
                            if e.len() < 20 {
                                e.push(format!(
                                    "record {id}: verdict ours {ours_ok} cozy {cozy_ok}, plies {} vs {}, first difference at {:?}",
                                    rec.0.len(),
                                    cozy_states.len(),
                                    first_diff
                                ));
                            }
                        }
                    }
                }
            });
        }
    });
    // Squares iterate from a1, as a guard that both sides number them alike.
    assert_eq!(squares(1).next().map(|s| s.index()), Some(0));
    println!("records compared   {}", records.load(Ordering::Relaxed));
    println!("positions compared {}", plies.load(Ordering::Relaxed));
    println!("differences        {}", differences.load(Ordering::Relaxed));
    for e in examples.into_inner().unwrap() {
        println!("  {e}");
    }
}
