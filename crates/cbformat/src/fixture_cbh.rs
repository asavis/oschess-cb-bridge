//! Small classic (`.cbh`) databases for tests.
//!
//! [`encode`] writes move streams from the format description, with its own
//! piece bookkeeping, so that the reader's decoder is checked against a second
//! reading of the description rather than against itself. [`Builder`] writes
//! the files. Built only with the `fixture` feature.

use std::path::PathBuf;

use chesscore::{Board, CastleSide, Color, Move, Piece, Square};

use crate::cbh::tables;
use crate::fixture::TempDb;

/// One item of a move tree in stored order: a move in UCI (`e2e4`, `e7e8q`),
/// `--` for a null move, `O-O` / `O-O-O` for castling; [`Tok::Var`] before a
/// move that has alternatives still to come; [`Tok::End`] at the end of a line.
pub enum Tok<'a> {
    Mv(&'a str),
    Var,
    End,
}

const KINDS: [Piece; 4] = [Piece::Queen, Piece::Rook, Piece::Bishop, Piece::Knight];
const KING: [(u8, u8); 8] = [(0, 1), (1, 1), (1, 0), (1, 7), (0, 7), (7, 7), (7, 0), (7, 1)];
const KNIGHT: [(u8, u8); 8] = [(2, 1), (1, 2), (7, 2), (6, 1), (6, 7), (7, 6), (1, 6), (2, 7)];

/// A line direction: the number of steps a movement `(files, ranks)`, taken
/// modulo 8, makes along it, if it lies on it.
type Dir = fn((u8, u8)) -> Option<u8>;

fn cb(s: Square) -> u8 {
    s.file() * 8 + s.rank()
}

/// The encoder's own piece lists: per side, queens, rooks, bishops and
/// knights by ChessBase square in scan order, and pawns by fixed number.
#[derive(Clone)]
struct Lists {
    kinds: [[Vec<u8>; 4]; 2],
    pawns: [[Option<u8>; 8]; 2],
}

impl Lists {
    fn new(b: &Board) -> Lists {
        let mut l = Lists { kinds: Default::default(), pawns: [[None; 8]; 2] };
        let mut n = [0; 2];
        for s in 0..64u8 {
            match b.piece_at(Square::new(s / 8, s % 8)) {
                Some((Piece::Pawn, c)) => {
                    l.pawns[c.index()][n[c.index()]] = Some(s);
                    n[c.index()] += 1;
                }
                Some((p, c)) if p != Piece::King => {
                    l.kinds[c.index()][KINDS.iter().position(|&k| k == p).unwrap()].push(s)
                }
                _ => {}
            }
        }
        l
    }

    fn apply(&mut self, b: &Board, mv: Move) {
        let us = b.side_to_move();
        let (me, them) = (us.index(), (!us).index());
        let (from, to) = (cb(mv.from), cb(mv.to));
        let moving = b.piece_at(mv.from).unwrap().0;
        let target = b.piece_at(mv.to);
        if moving == Piece::King && target == Some((Piece::Rook, us)) {
            let rook_to = if mv.to.file() > mv.from.file() { 5 } else { 3 } * 8 + mv.to.rank();
            let i = self.kinds[me][1].iter().position(|&s| s == to).unwrap();
            self.kinds[me][1][i] = rook_to;
            return;
        }
        let taken = match target {
            Some((p, _)) => Some((p, to)),
            None if moving == Piece::Pawn && mv.from.file() != mv.to.file() => {
                Some((Piece::Pawn, mv.to.file() * 8 + mv.from.rank()))
            }
            None => None,
        };
        if let Some((p, at)) = taken {
            match KINDS.iter().position(|&k| k == p) {
                Some(k) => self.kinds[them][k].retain(|&s| s != at),
                None => self.pawns[them].iter_mut().filter(|s| **s == Some(at)).for_each(|s| *s = None),
            }
        }
        if moving == Piece::Pawn {
            let slot = self.pawns[me].iter_mut().find(|s| **s == Some(from)).unwrap();
            *slot = if mv.promotion.is_some() { None } else { Some(to) };
            if let Some(p) = mv.promotion {
                self.kinds[me][KINDS.iter().position(|&k| k == p).unwrap()].push(to);
            }
        } else if let Some(k) = KINDS.iter().position(|&k| k == moving) {
            let i = self.kinds[me][k].iter().position(|&s| s == from).unwrap();
            self.kinds[me][k][i] = to;
        }
    }

    /// The one-byte code of a move of a non-king piece, when it has one.
    fn code(&self, b: &Board, mv: Move) -> Option<u8> {
        let us = b.side_to_move();
        let (from, to) = (cb(mv.from), cb(mv.to));
        let d = ((to / 8 + 8 - from / 8) % 8, (to % 8 + 8 - from % 8) % 8);
        let moving = b.piece_at(mv.from)?.0;
        if moving == Piece::King {
            return KING.iter().position(|&k| k == d).map(|i| 1 + i as u8);
        }
        if moving == Piece::Pawn {
            let n = self.pawns[us.index()].iter().position(|&s| s == Some(from))? as u8;
            let f = if us == Color::White { 1 } else { 7 };
            let how = [(0, f), (0, (2 * f) % 8), (f, f), ((8 - f) % 8, f)].iter().position(|&x| x == d)? as u8;
            return (mv.promotion.is_none()).then_some(111 + 4 * n + how);
        }
        let k = KINDS.iter().position(|&k| k == moving)?;
        let i = self.kinds[us.index()][k].iter().position(|&s| s == from)?;
        let line = |dirs: &[Dir]| dirs.iter().enumerate().find_map(|(j, f)| f(d).map(|s| 7 * j as u8 + s - 1));
        let up: Dir = |d| (d.0 == 0).then_some(d.1);
        let right: Dir = |d| (d.1 == 0).then_some(d.0);
        let ur: Dir = |d| (d.0 == d.1).then_some(d.0);
        let dr: Dir = |d| (d.0 + d.1).is_multiple_of(8).then_some(d.0);
        let (offset, bases) = match moving {
            Piece::Queen => (line(&[up, right, ur, dr])?, [11, 143, 171]),
            Piece::Rook => (line(&[up, right])?, [39, 53, 199]),
            Piece::Bishop => (line(&[ur, dr])?, [67, 81, 213]),
            _ => (KNIGHT.iter().position(|&x| x == d)? as u8, [95, 103, 227]),
        };
        bases.get(i).map(|b| b + offset)
    }
}

/// Encodes a move tree for encoding mode `mode` (0, 4, 5 or 10) from `start`.
/// With `two_byte`, every compact move that can be written in two bytes is.
pub fn encode(start: &Board, toks: &[Tok<'_>], mode: u8, two_byte: bool) -> Vec<u8> {
    let (tr, simple) = translator(mode);
    let (mut board, mut lists, mut stack, mut out) = (start.clone(), Lists::new(start), Vec::new(), Vec::new());
    let (mut n, mut var, mut last) = (0u8, false, None::<(usize, u16, u8)>);
    for t in toks {
        match t {
            Tok::Var if simple => {
                var = true;
                stack.push((board.clone(), lists.clone()));
            }
            Tok::Var => {
                out.push(tr(254, n));
                stack.push((board.clone(), lists.clone()));
            }
            Tok::End => {
                if simple {
                    let (at, w, wn) = last.expect("a line ends after a move");
                    let w = w | 0x4000;
                    out[at] = tr((w >> 8) as u8, wn);
                    out[at + 1] = tr(w as u8, wn);
                    last = None;
                } else {
                    out.push(tr(255, n));
                }
                if let Some((b, l)) = stack.pop() {
                    (board, lists) = (b, l);
                }
            }
            Tok::Mv(m) => {
                let us = board.side_to_move();
                let castle = match *m {
                    "O-O" => Some(CastleSide::Short),
                    "O-O-O" => Some(CastleSide::Long),
                    _ => None,
                };
                let (mv, word) = if *m == "--" {
                    (None, 0u16)
                } else if let Some(side) = castle {
                    let rook = board.castling_rook(us, side).expect("castling right");
                    let dest = Square::new(if side == CastleSide::Short { 6 } else { 2 }, us.back_rank());
                    let king = board.king(us);
                    let word = if mode == 10 {
                        u16::from(cb(dest)) * 65
                    } else {
                        u16::from(cb(king)) | u16::from(cb(dest)) << 6
                    };
                    (Some(Move::new(king, Square::new(rook, us.back_rank()), None)), word)
                } else {
                    let mv: Move = m.parse().expect("uci move");
                    let promo = mv.promotion.map_or(0, |p| {
                        [Piece::Queen, Piece::Rook, Piece::Bishop, Piece::Knight].iter().position(|&x| x == p).unwrap()
                            as u16
                    });
                    (Some(mv), u16::from(cb(mv.from)) | u16::from(cb(mv.to)) << 6 | promo << 12)
                };
                if simple {
                    let w = word | if var { 0x8000 } else { 0 };
                    last = Some((out.len(), w, n));
                    out.push(tr((w >> 8) as u8, n));
                    out.push(tr(w as u8, n));
                } else {
                    let one = match (mv, castle) {
                        (None, _) => Some(0),
                        (Some(_), Some(side)) if mode != 10 => Some(if side == CastleSide::Short { 9 } else { 10 }),
                        (Some(mv), None) => lists.code(&board, mv),
                        _ => None,
                    };
                    match one.filter(|&c| !(two_byte && c != 0)) {
                        Some(c) => out.push(tr(c, n)),
                        None => out.extend([tr(235, n), tr((word >> 8) as u8, n), tr(word as u8, n)]),
                    }
                }
                var = false;
                match mv {
                    None => board = board.null_move().expect("null move"),
                    Some(mv) => {
                        lists.apply(&board, mv);
                        board.play_checked(mv).expect("legal move");
                    }
                }
                n = n.wrapping_add(1);
            }
        }
    }
    out
}

/// The byte that a decoder in encoding mode `mode` translates to `value`
/// when `n` moves have been decoded, and whether the mode uses the simple
/// encoder.
fn translator(mode: u8) -> (impl Fn(u8, u8) -> u8, bool) {
    let (table, pre, simple) = match mode {
        0 => (&tables::MODE_0, true, false),
        4 => (&tables::MODE_4, false, false),
        5 => (&tables::MODE_5, false, true),
        10 => (&tables::MODE_10, false, false),
        _ => panic!("no table for mode {mode}"),
    };
    let mut inv = [0u8; 256];
    for (i, &v) in table.iter().enumerate() {
        inv[v as usize] = i as u8;
    }
    let tr = move |v: u8, n: u8| if pre { inv[v as usize].wrapping_add(n) } else { inv[v.wrapping_add(n) as usize] };
    (tr, simple)
}

/// A stream of raw values, each translated for mode `mode` with its move
/// counter: `(value, n)`. For hand-built records that [`encode`], which only
/// writes legal trees, cannot write.
pub fn raw(mode: u8, values: &[(u8, u8)]) -> Vec<u8> {
    let (tr, _) = translator(mode);
    values.iter().map(|&(v, n)| tr(v, n)).collect()
}

/// A 28-byte start position: `pieces` as (square, piece, colour).
pub fn start_position(pieces: &[(&str, Piece, Color)], black_to_move: bool, castling: u8, ep_file: u8) -> [u8; 28] {
    let mut board: [Option<(Piece, Color)>; 64] = [None; 64];
    for &(s, p, c) in pieces {
        let sq: Square = s.parse().expect("square");
        board[cb(sq) as usize] = Some((p, c));
    }
    let mut bits = Vec::new();
    for sq in board {
        match sq {
            None => bits.push(0),
            Some((p, c)) => {
                let code = [6, 3, 4, 5, 2, 1][p as usize];
                bits.extend([1, c as u8, code >> 2 & 1, code >> 1 & 1, code & 1]);
            }
        }
    }
    let mut s = [0u8; 28];
    s[0] = 1;
    s[1] = ep_file | if black_to_move { 0x10 } else { 0 };
    s[2] = castling;
    s[3] = 1;
    for (i, b) in bits.iter().enumerate().take(192) {
        s[4 + i / 8] |= b << (7 - i % 8);
    }
    s
}

/// A `.cbg` record: flags, size, the optional start position and extra
/// Chess960 bytes, and the stream.
pub fn move_record(flags: u8, start: Option<&[u8]>, extra: Option<&[u8]>, stream: &[u8]) -> Vec<u8> {
    let mut r = vec![flags, 0, 0, 0];
    r.extend(start.unwrap_or(&[]));
    r.extend(extra.unwrap_or(&[]));
    r.extend(stream);
    let size = (r.len() as u32).to_be_bytes();
    r[1..4].copy_from_slice(&size[1..]);
    r
}

/// A `.cba` record for game `id`: each item a position, a type and its data.
pub fn annotation_record(id: u32, items: &[(i32, u8, &[u8])]) -> Vec<u8> {
    let mut r = id.to_be_bytes()[1..].to_vec();
    r.extend([1, 0, 0x0e, 0x0e]);
    r.extend(&(items.len() as u32 + 1).to_be_bytes()[1..]);
    r.extend([0; 4]);
    for (position, t, data) in items {
        r.extend(&position.to_be_bytes()[1..]);
        r.push(*t);
        r.extend((data.len() as u16 + 6).to_be_bytes());
        r.extend(*data);
    }
    let size = (r.len() as u32).to_be_bytes();
    r[10..14].copy_from_slice(&size);
    r
}

/// Builds a classic database: game records, their move and annotation
/// records in the order added, and one player, tournament, annotator and
/// source.
pub struct Builder {
    records: Vec<[u8; 46]>,
    cbg: Vec<u8>,
    cba: Vec<u8>,
    players: Vec<Vec<u8>>,
}

impl Default for Builder {
    fn default() -> Self {
        let mut cbg = vec![0u8; 26];
        cbg[1] = 26;
        let players = vec![b"Morphy".to_vec(), b"Anderssen".to_vec()];
        Builder { records: Vec::new(), cbg, cba: vec![0u8; 26], players }
    }
}

impl Builder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a game, won by white between players 0 and 1, with move
    /// record `rec`, and returns its header record for further changes.
    pub fn game(&mut self, rec: &[u8]) -> &mut [u8; 46] {
        let mut h = [0u8; 46];
        h[0] = 1;
        h[1..5].copy_from_slice(&(self.cbg.len() as u32).to_be_bytes());
        h[0x0c..0x0f].copy_from_slice(&[0, 0, 1]);
        h[0x1b] = 2;
        self.cbg.extend(rec);
        self.records.push(h);
        self.records.last_mut().unwrap()
    }

    /// Gives the last game added the annotation record `rec`.
    pub fn annotations(&mut self, rec: &[u8]) -> &mut Self {
        let at = (self.cba.len() as u32).to_be_bytes();
        self.records.last_mut().expect("a game first")[5..9].copy_from_slice(&at);
        self.cba.extend(rec);
        self
    }

    /// Replaces player `id`'s raw 30-byte last-name field contents.
    pub fn player_name(&mut self, id: usize, raw: &[u8]) -> &mut Self {
        self.players[id] = raw.to_vec();
        self
    }

    /// Writes `db.cbh` and its companions to a new temporary directory.
    pub fn write(&self, name: &str) -> TempDb {
        let dir = std::env::temp_dir().join(format!("cbformat-cbh-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cbh = vec![0u8; 46];
        cbh[1..6].copy_from_slice(&[0, 44, 0, 46, 1]);
        cbh[6..10].copy_from_slice(&(self.records.len() as u32 + 1).to_be_bytes());
        for r in &self.records {
            cbh.extend(r);
        }
        let mut cbg = self.cbg.clone();
        let size = (cbg.len() as u32).to_be_bytes();
        cbg[2..6].copy_from_slice(&size);
        let entity = |data: usize, recs: &[Vec<u8>]| {
            let mut f = Vec::new();
            for v in [recs.len() as i32, 0, 1_234_567_890, data as i32, -1, recs.len() as i32, 0] {
                f.extend(v.to_le_bytes());
            }
            for r in recs {
                f.extend((-1i32).to_le_bytes());
                f.extend((-1i32).to_le_bytes());
                f.push(0);
                let mut d = r.clone();
                d.resize(data, 0);
                f.extend(d);
            }
            f
        };
        let path = |ext: &str| -> PathBuf { dir.join(format!("db{ext}")) };
        std::fs::write(path(".cbh"), cbh).unwrap();
        std::fs::write(path(".cbg"), cbg).unwrap();
        std::fs::write(path(".cba"), &self.cba).unwrap();
        std::fs::write(path(".cbp"), entity(58, &self.players)).unwrap();
        std::fs::write(path(".cbt"), entity(90, &[b"Paris".to_vec()])).unwrap();
        std::fs::write(path(".cbc"), entity(53, &[Vec::new()])).unwrap();
        std::fs::write(path(".cbs"), entity(59, &[Vec::new()])).unwrap();
        TempDb::at(dir)
    }
}
