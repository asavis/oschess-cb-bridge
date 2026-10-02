//! A position fragment and a material filter (#272): the games
//! `GET /v1/databases/{id}/games` lists for `look`, `nowhite`, `noblack`,
//! `or`, `exclude`, `material`, `mirror`, `first`, `last` and `length`, as the
//! Position and Material tabs of ChessBase's game filter find them.
//!
//! [`Filter`] reads the parameters and tells whether a position holds. A game
//! matches when its main line holds the filter for `length` consecutive plies
//! whose move numbers lie from `first` to `last`; [`Filter::first_match`]
//! replays a line from the move stream to the first such stretch. [`Games`]
//! lists every game that has one: the masks of the games rule most of them out
//! first ([`super::masks`]), and the rest are replayed.

use std::hash::{Hash, Hasher};

use chesscore::{Bitboard, Board, Color, Piece, Replayer, Square};

use crate::json::Obj;
use crate::search::memory::Cancel;
use crate::search::workers;
use crate::search::{Members, Position, SearchError};

use super::Loaded;
use super::file::Bad;
use super::masks::{self, Masks, Row};
use super::stream::{moves, standard};

/// Pieces a board may list.
pub const MAX_PIECES: usize = 32;
/// Pieces an Exclude square may list, as ChessBase's board allows.
pub const MAX_EXCLUDED: usize = 4;
/// The highest move number `first` and `last` take.
pub const MAX_MOVE: u16 = 999;
/// The longest stretch `length` asks for, in plies.
pub const MAX_LENGTH: u16 = 99;
/// The highest count a material range names: a side has 16 men at most.
pub const MAX_COUNT: u8 = 16;

/// Each side's pawns' home rank ([`Color::index`]): white's second, black's
/// seventh.
const HOME_RANKS: [Bitboard; 2] = [0x0000_0000_0000_ff00, 0x00ff_0000_0000_0000];

/// The kinds a material range names, in the order its text writes them.
const COUNTED: [Piece; 5] = [Piece::Queen, Piece::Rook, Piece::Bishop, Piece::Knight, Piece::Pawn];

/// For each colour and kind ([`Color::index`], [`Piece::index`]), the
/// squares a board lists.
type Pieces = [[Bitboard; 6]; 2];

/// For each colour and kind ([`Color::index`], [`Piece::index`]), the range
/// of its count a material filter names, if any.
type Material = [[Option<(u8, u8)>; 6]; 2];

/// One of the fragment's forms: itself, or one of its mirrors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Variant {
    pub look: Pieces,
    pub or: Pieces,
    pub exclude: Pieces,
    /// Squares without a white piece, and without a black one.
    pub nowhite: Bitboard,
    pub noblack: Bitboard,
}

impl Variant {
    pub(super) fn has_or(&self) -> bool {
        self.or.iter().flatten().any(|&b| b != 0)
    }

    /// Whether the position `r` holds this form of the fragment.
    fn holds(&self, r: &Replayer) -> bool {
        if r.colors(Color::White) & self.nowhite != 0 || r.colors(Color::Black) & self.noblack != 0 {
            return false;
        }
        let mut or = !self.has_or();
        for color in Color::ALL {
            let c = color.index();
            for k in 0..6 {
                let Some(piece) = Piece::from_index(k) else { continue };
                let on = r.colored(piece, color);
                let look = self.look[c][k];
                if on & look != look || on & self.exclude[c][k] != 0 {
                    return false;
                }
                or |= on & self.or[c][k] != 0;
            }
        }
        or
    }

    /// The men and the pawns of each side this form needs on the board.
    fn needs(&self) -> [(u32, u32); 2] {
        let men = |c: usize| self.look[c].iter().map(|b| b.count_ones()).sum::<u32>();
        [(men(0), self.look[0][0].count_ones()), (men(1), self.look[1][0].count_ones())]
    }

    /// This form mirrored: across the board's middle file (`a` and `h`
    /// change places), or across its middle rank with the colours changed.
    fn mirrored(&self, files: bool, ranks: bool) -> Variant {
        let flip = |b: Bitboard| {
            let mut out = 0;
            for i in 0..64u8 {
                if b & (1 << i) != 0 {
                    let (f, r) = (i & 7, i >> 3);
                    let (f, r) = (if files { 7 - f } else { f }, if ranks { 7 - r } else { r });
                    out |= 1 << (r * 8 + f);
                }
            }
            out
        };
        let pieces = |p: &Pieces| {
            let mut out: Pieces = [[0; 6]; 2];
            for (c, kinds) in p.iter().enumerate() {
                let to = if ranks { 1 - c } else { c };
                out[to] = kinds.map(flip);
            }
            out
        };
        let (nowhite, noblack) = (flip(self.nowhite), flip(self.noblack));
        Variant {
            look: pieces(&self.look),
            or: pieces(&self.or),
            exclude: pieces(&self.exclude),
            nowhite: if ranks { noblack } else { nowhite },
            noblack: if ranks { nowhite } else { noblack },
        }
    }
}

/// Whether a request names a fragment or a material filter, and the filter's
/// own parameters: the name and the message of the first one refused.
pub type Refusal = (&'static str, String);

/// A position fragment and a material filter, read from a request.
#[derive(Debug)]
pub struct Filter {
    pub(super) variants: Vec<Variant>,
    /// For each colour and kind ([`Piece::index`]), the range of its count.
    pub(super) material: Material,
    first: u16,
    last: u16,
    length: u16,
    /// The parameters as the answer acknowledges them, each in a canonical
    /// form, which is also the filter's key among the latest searches.
    text: [(&'static str, String); 7],
    pub key: u64,
}

fn refuse(name: &'static str, message: impl Into<String>) -> Refusal {
    (name, message.into())
}

/// The square of `text`, `a1` to `h8`.
fn square(text: &str) -> Option<Square> {
    text.parse().ok()
}

fn piece_of(letter: u8) -> Option<(Color, Piece)> {
    let color = if letter.is_ascii_uppercase() { Color::White } else { Color::Black };
    let piece = match letter.to_ascii_uppercase() {
        b'K' => Piece::King,
        b'Q' => Piece::Queen,
        b'R' => Piece::Rook,
        b'B' => Piece::Bishop,
        b'N' => Piece::Knight,
        b'P' => Piece::Pawn,
        _ => return None,
    };
    Some((color, piece))
}

fn letter_of(color: Color, piece: Piece) -> char {
    let l = piece.letter();
    if color == Color::White { l } else { l.to_ascii_lowercase() }
}

/// A board's pieces, `Pe4,pd5,Nd5`: a piece's letter as FEN writes it, white
/// in upper case, and its square, comma-separated, [`MAX_PIECES`] at most.
fn board(name: &'static str, text: &str) -> Result<Pieces, Refusal> {
    let mut out: Pieces = [[0; 6]; 2];
    let tokens: Vec<&str> = text.split(',').map(str::trim).filter(|t| !t.is_empty()).collect();
    if tokens.len() > MAX_PIECES {
        return Err(refuse(name, format!("{name} lists more than {MAX_PIECES} pieces")));
    }
    for token in tokens {
        let parsed = token.as_bytes().first().and_then(|&l| piece_of(l)).zip(token.get(1..).and_then(square));
        let Some(((color, piece), sq)) = parsed else {
            return Err(refuse(name, format!("{name} must list pieces as a letter and a square, such as Pe4,nd5")));
        };
        out[color.index()][piece.index()] |= sq.bit();
    }
    Ok(out)
}

/// Squares, `d4,e5`, comma-separated.
fn squares(name: &'static str, text: &str) -> Result<Bitboard, Refusal> {
    let mut out = 0;
    for token in text.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let Some(sq) = square(token) else {
            return Err(refuse(name, format!("{name} must list squares, such as d4,e5")));
        };
        out |= sq.bit();
    }
    Ok(out)
}

fn pieces_text(p: &Pieces) -> String {
    let mut tokens = Vec::new();
    for color in Color::ALL {
        for k in [5, 4, 3, 2, 1, 0] {
            let Some(piece) = Piece::from_index(k) else { continue };
            let bits = p[color.index()][k];
            for i in 0..64u8 {
                if bits & (1 << i) != 0
                    && let Some(sq) = Square::from_index(i)
                {
                    tokens.push(format!("{}{sq}", letter_of(color, piece)));
                }
            }
        }
    }
    tokens.join(",")
}

fn squares_text(b: Bitboard) -> String {
    (0..64u8)
        .filter(|i| b & (1 << i) != 0)
        .filter_map(Square::from_index)
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// A count, 0 to [`MAX_COUNT`].
fn count(text: &str) -> Option<u8> {
    text.parse::<u8>().ok().filter(|&n| n <= MAX_COUNT)
}

/// The material ranges, `Q0,q0,R1..2,p..4`: a kind's letter (no king) and a
/// count or a range `a..b`, either end open.
fn material(text: &str) -> Result<Material, Refusal> {
    let bad =
        || refuse("material", "material must list ranges such as Q0,R1..2,p..4: a letter and a count from 0 to 16");
    let mut out = [[None; 6]; 2];
    for token in text.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        // A first character outside ASCII is no letter of a kind, and no
        // place to split the token at.
        let Some((letter, range)) = token.split_at_checked(1) else { return Err(bad()) };
        let Some((color, piece)) = piece_of(letter.as_bytes()[0]).filter(|(_, p)| *p != Piece::King) else {
            return Err(bad());
        };
        let (low, high) = match range.split_once("..") {
            Some((low, high)) => (
                if low.is_empty() { Some(0) } else { count(low) },
                if high.is_empty() { Some(MAX_COUNT) } else { count(high) },
            ),
            None => (count(range), count(range)),
        };
        let (Some(low), Some(high)) = (low, high) else { return Err(bad()) };
        if low > high {
            return Err(refuse("material", format!("{token} is an empty range")));
        }
        let slot = &mut out[color.index()][piece.index()];
        if slot.is_some() {
            return Err(refuse("material", format!("material names {letter} twice")));
        }
        *slot = Some((low, high));
    }
    Ok(out)
}

fn material_text(m: &Material) -> String {
    let mut tokens = Vec::new();
    for color in Color::ALL {
        for piece in COUNTED {
            if let Some((low, high)) = m[color.index()][piece.index()] {
                let l = letter_of(color, piece);
                tokens.push(if low == high { format!("{l}{low}") } else { format!("{l}{low}..{high}") });
            }
        }
    }
    tokens.join(",")
}

fn number(name: &'static str, text: Option<&str>, max: u16, default: u16) -> Result<u16, Refusal> {
    match text {
        None => Ok(default),
        Some(t) => t
            .parse::<u16>()
            .ok()
            .filter(|n| (1..=max).contains(n))
            .ok_or_else(|| refuse(name, format!("{name} must be a whole number from 1 to {max}"))),
    }
}

impl Filter {
    /// The filter a request's parameters name, `param` reading them; `None`
    /// when it names neither a fragment nor material. `first`, `last`,
    /// `length` and `mirror` alone are refused, naming the first of them.
    pub fn parse<'r>(param: impl Fn(&str) -> Option<&'r str>) -> Result<Option<Filter>, Refusal> {
        let named = ["look", "nowhite", "noblack", "or", "exclude", "material"];
        if !named.iter().any(|n| param(n).is_some()) {
            if let Some(alone) = ["mirror", "first", "last", "length"].into_iter().find(|n| param(n).is_some()) {
                return Err(refuse(alone, format!("{alone} needs a fragment or material to apply to")));
            }
            return Ok(None);
        }
        let look = board("look", param("look").unwrap_or_default())?;
        let or = board("or", param("or").unwrap_or_default())?;
        let exclude = board("exclude", param("exclude").unwrap_or_default())?;
        for i in 0..64 {
            let on: u32 = exclude.iter().flatten().map(|b| (b >> i & 1) as u32).sum();
            if on as usize > MAX_EXCLUDED {
                return Err(refuse("exclude", format!("exclude lists more than {MAX_EXCLUDED} pieces on one square")));
            }
        }
        let nowhite = squares("nowhite", param("nowhite").unwrap_or_default())?;
        let noblack = squares("noblack", param("noblack").unwrap_or_default())?;
        let material = material(param("material").unwrap_or_default())?;
        let (files, ranks, mirror) = match param("mirror") {
            None | Some("none") => (false, false, "none"),
            Some("horizontal") => (true, false, "horizontal"),
            Some("vertical") => (false, true, "vertical"),
            Some("both") => (true, true, "both"),
            Some(_) => return Err(refuse("mirror", "mirror must be horizontal, vertical or both")),
        };
        let first = number("first", param("first"), MAX_MOVE, 1)?;
        let last = number("last", param("last"), MAX_MOVE, MAX_MOVE)?;
        if first > last {
            return Err(refuse("first", "first must not be after last"));
        }
        let length = number("length", param("length"), MAX_LENGTH, 1)?;
        let base = Variant { look, or, exclude, nowhite, noblack };
        let mut variants = vec![base];
        for (f, r) in [(true, false), (false, true), (true, true)] {
            if (f && !files) || (r && !ranks) {
                continue;
            }
            let v = base.mirrored(f, r);
            if !variants.contains(&v) {
                variants.push(v);
            }
        }
        let text = [
            ("look", pieces_text(&look)),
            ("nowhite", squares_text(nowhite)),
            ("noblack", squares_text(noblack)),
            ("or", pieces_text(&or)),
            ("exclude", pieces_text(&exclude)),
            ("material", material_text(&material)),
            ("mirror", mirror.to_string()),
        ];
        let mut hasher = std::hash::DefaultHasher::new();
        ("fragment", &text, first, last, length).hash(&mut hasher);
        Ok(Some(Filter { variants, material, first, last, length, text, key: hasher.finish() }))
    }

    /// The filter as the answer acknowledges it, with the games that match
    /// it before `q`.
    pub fn acknowledged(&self, games: u64) -> String {
        let mut o = Obj::new();
        for (name, value) in &self.text {
            o = o.str(name, value);
        }
        o.num("first", i64::from(self.first))
            .num("last", i64::from(self.last))
            .num("length", i64::from(self.length))
            .num("games", games as i64)
            .done()
    }

    /// Whether the position `r` holds the fragment, in any of its forms, and
    /// the material.
    pub fn holds(&self, r: &Replayer) -> bool {
        self.material_holds(r) && self.variants.iter().any(|v| v.holds(r))
    }

    fn material_holds(&self, r: &Replayer) -> bool {
        Color::ALL.into_iter().all(|color| {
            COUNTED.into_iter().all(|piece| match self.material[color.index()][piece.index()] {
                None => true,
                Some((low, high)) => (u32::from(low)..=u32::from(high)).contains(&r.colored(piece, color).count_ones()),
            })
        })
    }

    /// Whether no later position of a line at `r` can hold the filter: a side
    /// never gains men or pawns, and a pawn that left its home rank never
    /// comes back to it, so a side with fewer men or pawns than every form,
    /// or the material, needs, or without a home pawn every form looks for,
    /// can no longer reach it.
    fn out_of_reach(&self, r: &Replayer) -> bool {
        let have = Color::ALL.map(|c| (r.colors(c).count_ones(), r.colored(Piece::Pawn, c).count_ones()));
        let pawns = Color::ALL.map(|c| r.colored(Piece::Pawn, c));
        let material = Color::ALL.into_iter().all(|color| {
            let m = &self.material[color.index()];
            let men = 1 + m.iter().flatten().map(|&(low, _)| u32::from(low)).sum::<u32>();
            let pawns = m[Piece::Pawn.index()].map_or(0, |(low, _)| u32::from(low));
            let (h_men, h_pawns) = have[color.index()];
            h_men >= men && h_pawns >= pawns
        });
        let fragment = self.variants.iter().any(|v| {
            let need = v.needs();
            (0..2).all(|c| {
                let home = v.look[c][Piece::Pawn.index()] & HOME_RANKS[c];
                have[c].0 >= need[c].0 && have[c].1 >= need[c].1 && pawns[c] & home == home
            })
        });
        !(material && fragment)
    }

    /// The first ply of the first stretch of `length` plies at which a line
    /// from `start` (the standard start for `None`) whose move words are
    /// `words` holds the filter, each at a move number from `first` to
    /// `last`; `None` when it has none. The words were checked when the
    /// stream was written, so they are played unchecked.
    pub fn first_match(&self, start: Option<Board>, words: impl Iterator<Item = u16>) -> Result<Option<u32>, Bad> {
        let board = start.unwrap_or_else(|| standard().clone());
        let black_first = u32::from(board.side_to_move() == Color::Black);
        let fullmove = u32::from(board.fullmove_number().max(1));
        let (first, last, length) = (u32::from(self.first), u32::from(self.last), u32::from(self.length));
        let mut r = Replayer::new(board);
        let (moves, mut words) = (moves(), words);
        let mut stretch: Option<u32> = None;
        let mut ply = 0u32;
        loop {
            let number = fullmove + (ply + black_first) / 2;
            if number > last {
                return Ok(None);
            }
            if number >= first && self.holds(&r) {
                let from = *stretch.get_or_insert(ply);
                if ply - from + 1 >= length {
                    return Ok(Some(from));
                }
            } else {
                stretch = None;
                if self.out_of_reach(&r) {
                    return Ok(None);
                }
            }
            let Some(word) = words.next() else { return Ok(None) };
            let mv = moves.get(usize::from(word)).copied().flatten().ok_or(Bad::Corrupt("stream word"))?;
            r.play(mv);
            ply += 1;
        }
    }

    /// Whether a game whose mask is `row` may hold the filter: its men stood
    /// on every square a form looks for, on one an Or lists, and its counts
    /// passed through every material range. A mask is a superset of the
    /// game's positions, so a game it rules out never matches.
    pub fn may_match(&self, row: &Row) -> bool {
        if !row.indexed {
            return false;
        }
        let material = Color::ALL.into_iter().all(|color| {
            let c = color.index();
            COUNTED.into_iter().all(|piece| match self.material[c][piece.index()] {
                None => true,
                Some((low, high)) => {
                    let (least, most) = row.range(color, piece);
                    least <= high && most >= low
                }
            })
        });
        material && self.variants.iter().any(|v| row.may_hold(v))
    }

    /// The first ply of game `number`'s first matching stretch, read from the
    /// index's move stream; `None` for a game without one.
    pub fn match_of(&self, loaded: &Loaded, number: u32) -> Result<Option<u32>, Bad> {
        let record = loaded.stream.record(number)?;
        if !record.entry.indexed() {
            return Ok(None);
        }
        self.first_match(record.start()?, record.words())
    }
}

/// The games of a filter in the index `loaded`, as a list narrows to them;
/// `masks` rules games out first, or none is ruled out without it.
pub struct Games<'a> {
    pub loaded: &'a Loaded,
    pub masks: Option<&'a Masks>,
    pub filter: &'a Filter,
}

impl Position for Games<'_> {
    fn key(&self) -> u64 {
        self.filter.key
    }

    fn games(&self, cancel: &Cancel) -> Result<Members, SearchError> {
        let loaded = self.loaded;
        let header = loaded.stream.header;
        let members = Members::new(loaded.records() as usize + 1)?;
        let records = header.last_record.saturating_sub(header.first_record).saturating_add(1);
        let blocks = (records as usize).div_ceil(masks::BLOCK);
        let damaged = |e: Bad| match e {
            Bad::Busy => SearchError::Busy,
            _ => SearchError::IndexDamaged,
        };
        workers::each(blocks, cancel, |block| {
            let first = header.first_record + (block * masks::BLOCK) as u32;
            let last = (first + masks::BLOCK as u32 - 1).min(header.last_record);
            let rows = match self.masks {
                Some(m) => Some(m.block(block).map_err(damaged)?),
                None => None,
            };
            for (i, number) in (first..=last).enumerate() {
                if let Some(rows) = rows
                    && !self.filter.may_match(&Row::decode(&rows[i * masks::ROW..(i + 1) * masks::ROW]))
                {
                    continue;
                }
                if self.filter.match_of(loaded, number).map_err(damaged)?.is_some() {
                    members.insert(number);
                }
            }
            Ok(())
        })?;
        Ok(members)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn filter(params: &[(&str, &str)]) -> Result<Option<Filter>, Refusal> {
        let map: HashMap<&str, &str> = params.iter().copied().collect();
        Filter::parse(|n| map.get(n).copied())
    }

    fn holds(f: &Filter, fen: &str) -> bool {
        f.holds(&Replayer::new(Board::from_fen(fen).unwrap()))
    }

    const START: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";

    #[test]
    fn parameters_are_read_and_refused_by_name() {
        assert!(filter(&[]).unwrap().is_none());
        for (name, value) in [
            ("look", "Xe4"),
            ("look", "Pe9"),
            ("or", "e4"),
            ("nowhite", "i1"),
            ("material", "K1"),
            ("material", "Q1..0"),
            ("material", "Q17"),
            ("material", "Q1,Q2"),
            ("material", "\u{e9}1"),
            ("look", "\u{e9}e4"),
            ("mirror", "diagonal"),
            ("first", "0"),
            ("last", "1000"),
            ("length", "100"),
        ] {
            let params = [("look", "Pe4"), (name, value)];
            let params = if name == "look" { &params[1..] } else { &params[..] };
            assert_eq!(filter(params).unwrap_err().0, name, "{name}={value}");
        }
        assert_eq!(filter(&[("look", "Pe4"), ("first", "9"), ("last", "8")]).unwrap_err().0, "first");
        assert_eq!(filter(&[("exclude", "Pe4,Ne4,Be4,Re4,Qe4")]).unwrap_err().0, "exclude");
        assert!(filter(&[("exclude", "Pe4,Ne4,Be4,Re4")]).is_ok());
        // Alone, the window and the mirror apply to nothing.
        assert_eq!(filter(&[("first", "3")]).unwrap_err().0, "first");
        assert_eq!(filter(&[("mirror", "both")]).unwrap_err().0, "mirror");
        let many: Vec<String> = (0..33).map(|i| format!("P{}{}", (b'a' + i % 8) as char, 1 + i / 8 % 8)).collect();
        assert_eq!(filter(&[("look", &many.join(","))]).unwrap_err().0, "look");
    }

    #[test]
    fn the_acknowledgement_writes_each_parameter_in_one_form() {
        let f = filter(&[("look", " pd6, Pe4,Nd5"), ("material", "q0,Q0,R1..2,p..4"), ("nowhite", "e5,d4")])
            .unwrap()
            .unwrap();
        let g = filter(&[("look", "Nd5,Pe4,pd6"), ("material", "Q0,R1..2,q0,p0..4"), ("nowhite", "d4,e5")])
            .unwrap()
            .unwrap();
        assert_eq!(f.key, g.key);
        assert_eq!(
            f.acknowledged(7),
            r#"{"look":"Nd5,Pe4,pd6","nowhite":"d4,e5","noblack":"","or":"","exclude":"","material":"Q0,R1..2,q0,p0..4","mirror":"none","first":1,"last":999,"length":1,"games":7}"#
        );
    }

    #[test]
    fn look_points_or_and_exclude_hold_as_chessbase_reads_them() {
        let after_e4 = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1";
        let f = |p: &[(&str, &str)]| filter(p).unwrap().unwrap();
        assert!(holds(&f(&[("look", "Pe4")]), after_e4));
        assert!(!holds(&f(&[("look", "Pe4")]), START));
        // A white point: no white piece there; a black one: no black piece; both: empty.
        assert!(holds(&f(&[("nowhite", "e2")]), after_e4));
        assert!(!holds(&f(&[("nowhite", "e4")]), after_e4));
        assert!(holds(&f(&[("noblack", "e4")]), after_e4));
        assert!(!holds(&f(&[("nowhite", "e7"), ("noblack", "e7")]), after_e4));
        assert!(holds(&f(&[("nowhite", "e5"), ("noblack", "e5")]), after_e4));
        // At least one of Or.
        assert!(holds(&f(&[("or", "Pd4,Pe4")]), after_e4));
        assert!(!holds(&f(&[("or", "Pd4,Pc4")]), after_e4));
        // None of Exclude, several pieces on one square.
        assert!(!holds(&f(&[("exclude", "Ne4,Pe4")]), after_e4));
        assert!(holds(&f(&[("exclude", "Ne4,Be4,Re4,Qe4")]), after_e4));
    }

    #[test]
    fn mirrors_flip_files_ranks_and_colours() {
        let f = |m: &str| filter(&[("look", "Bh7"), ("mirror", m)]).unwrap().unwrap();
        // Only what the fixtures need: a lone bishop on a board with both kings.
        let fen = |sq: &str, white: bool| {
            let s: Square = sq.parse().unwrap();
            let mut board = [[' '; 8]; 8];
            board[0][0] = 'K';
            board[7][4] = 'k';
            board[s.rank() as usize][s.file() as usize] = if white { 'B' } else { 'b' };
            let rows: Vec<String> = (0..8)
                .rev()
                .map(|r| {
                    let mut row = String::new();
                    let mut empty = 0;
                    for c in board[r] {
                        if c == ' ' {
                            empty += 1;
                        } else {
                            if empty > 0 {
                                row.push_str(&empty.to_string());
                                empty = 0;
                            }
                            row.push(c);
                        }
                    }
                    if empty > 0 {
                        row.push_str(&empty.to_string());
                    }
                    row
                })
                .collect();
            format!("{} w - - 0 1", rows.join("/"))
        };
        assert!(holds(&f("none"), &fen("h7", true)));
        assert!(!holds(&f("none"), &fen("a7", true)));
        assert!(holds(&f("horizontal"), &fen("a7", true)));
        assert!(!holds(&f("horizontal"), &fen("h2", false)));
        // Vertical: the rank flipped and the colours changed, a black bishop on h2.
        assert!(holds(&f("vertical"), &fen("h2", false)));
        assert!(!holds(&f("vertical"), &fen("a7", true)));
        for sq in ["h7", "a7"] {
            assert!(holds(&f("both"), &fen(sq, true)), "{sq}");
        }
        for sq in ["h2", "a2"] {
            assert!(holds(&f("both"), &fen(sq, false)), "{sq}");
        }
        assert!(!holds(&f("both"), &fen("h2", true)));
    }

    #[test]
    fn material_ranges_count_each_side_and_kind() {
        let f = |m: &str| filter(&[("material", m)]).unwrap().unwrap();
        assert!(holds(&f("Q1,q1,P8"), START));
        assert!(!holds(&f("Q0"), START));
        assert!(holds(&f("R1..2,r2.."), START));
        assert!(!holds(&f("p..7"), START));
        let rook_ending = "4k3/pp3r2/8/8/8/8/PP3R2/4K3 w - - 0 1";
        assert!(holds(&f("Q0,q0,B0,b0,N0,n0,R1,r1"), rook_ending));
    }

    /// The move stream's words of the line `ucis` from the standard start.
    fn words_of(ucis: &[&str]) -> Vec<u16> {
        let mut board = Board::startpos();
        let table = moves();
        ucis.iter()
            .map(|&uci| {
                let mv = board.legal_moves().into_iter().find(|&m| crate::explorer::uci(&board, m) == uci).unwrap();
                board.play_checked(mv).unwrap();
                table.iter().position(|m| *m == Some(mv)).unwrap() as u16
            })
            .collect()
    }

    #[test]
    fn the_window_and_the_stretch_bound_a_line() {
        // 1.e4 e5 2.Nf3 Nc6 3.Bb5: the white pawn stands on e4 from ply 1 on.
        let words = words_of(&["e2e4", "e7e5", "g1f3", "b8c6", "f1b5"]);
        let at = |p: &[(&str, &str)]| filter(p).unwrap().unwrap().first_match(None, words.iter().copied()).unwrap();
        assert_eq!(at(&[("look", "Pe4")]), Some(1));
        // The position after 2.Nf3 is ply 3, move 2.
        assert_eq!(at(&[("look", "Pe4,Nf3")]), Some(3));
        // Move numbers: ply 1 is at move 1 (black to move), ply 2 at move 2.
        assert_eq!(at(&[("look", "Pe4"), ("first", "2")]), Some(2));
        assert_eq!(at(&[("look", "Pe4"), ("last", "1"), ("length", "2")]), None);
        assert_eq!(at(&[("look", "Pe4"), ("length", "5")]), Some(1));
        assert_eq!(at(&[("look", "Pe4"), ("length", "6")]), None);
        assert_eq!(at(&[("look", "Bb5")]), Some(5));
        assert_eq!(at(&[("look", "Pd4")]), None);
        // Out of reach: two white queens never come, and the replay stops.
        assert_eq!(at(&[("material", "Q2")]), None);
        // The e-pawn left home at ply 1 and never comes back.
        assert_eq!(at(&[("look", "Pe2")]), Some(0));
        assert_eq!(at(&[("look", "Pe2"), ("first", "2")]), None);
        let f = filter(&[("look", "Pe2,pc7")]).unwrap().unwrap();
        let after = |ucis: &[&str]| {
            let mut r = Replayer::new(Board::startpos());
            let table = moves();
            for w in words_of(ucis) {
                r.play(table[usize::from(w)].unwrap());
            }
            r
        };
        assert!(!f.out_of_reach(&after(&["d2d4"])));
        assert!(f.out_of_reach(&after(&["e2e4"])));
        assert!(f.out_of_reach(&after(&["d2d4", "c7c5"])));
        // Its mirror a↔h, pawns on d2 and f7, keeps it in reach.
        let mirrored = filter(&[("look", "Pe2,pc7"), ("mirror", "horizontal")]).unwrap().unwrap();
        assert!(!mirrored.out_of_reach(&after(&["e2e4"])));
    }

    #[test]
    fn a_mask_rules_out_what_its_line_never_had_and_nothing_else() {
        // 1.e4 c5 2.Nf3 d6 3.d4 cxd4 4.Nxd4 Nf6 5.Nc3 a6 6.Be3 e5 7.Nb3 Be7 8.Nd5.
        let line = "e2e4 c7c5 g1f3 d7d6 d2d4 c5d4 f3d4 g8f6 b1c3 a7a6 c1e3 e7e5 d4b3 f8e7 c3d5";
        let row = Row::of_line(None, words_of(&line.split(' ').collect::<Vec<_>>()).into_iter()).unwrap();
        let may = |p: &[(&str, &str)]| filter(p).unwrap().unwrap().may_match(&row);
        for kept in [
            &[("look", "Nd5,pd6")][..],
            &[("look", "Be3,Pe4,pe5")],
            // Black's bishop on e7, the white one on e2 mirrored.
            &[("look", "Be2"), ("mirror", "vertical")],
            &[("or", "Qh5,Nd5")],
            // Kings, points and Exclude are never ruled out by a mask.
            &[("look", "Kh8")],
            &[("nowhite", "e4"), ("noblack", "e4")],
            &[("exclude", "Pe4,Nd5")],
            &[("material", "P8,p7,N2")],
            &[("material", "p6..7")],
        ] {
            assert!(may(kept), "{kept:?}");
        }
        for ruled_out in [
            &[("look", "Bh7")][..],
            &[("look", "Be2")],
            &[("look", "Bh7"), ("mirror", "horizontal")],
            &[("look", "Nd5,pd6,Pc4")],
            &[("look", "Qd5")],
            &[("or", "Qh5,qh4")],
            &[("material", "Q0")],
            &[("material", "p..5")],
            &[("material", "N3..")],
        ] {
            assert!(!may(ruled_out), "{ruled_out:?}");
        }
        // A record the index does not hold matches nothing.
        assert!(!filter(&[("nowhite", "e4")]).unwrap().unwrap().may_match(&Row::default()));
    }
}
