//! A PGN text split into games: where each starts and ends, the tags the
//! reader looks at, and its main line's length, read in one pass over the
//! lexer's tokens ([`super::lex`]).
//!
//! A game starts at its first tag, or at its first move when it has none. It
//! ends at its result, or where the next game's tags start after its moves. A
//! tag the game already has also starts the next game, so that games without
//! movetext are told apart; a comment between tags does not. Whatever stands
//! outside games, such as a comment before the first tag, belongs to none.

use super::lex::{Sink, Token};
use crate::v2::Date;

/// The tags the reader looks at, in the order of [`Tags`].
pub const TAG_NAMES: [&str; 14] = [
    "White",
    "Black",
    "Event",
    "Site",
    "Date",
    "Round",
    "Result",
    "ECO",
    "WhiteElo",
    "BlackElo",
    "Annotator",
    "FEN",
    "SetUp",
    "Variant",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    White,
    Black,
    Event,
    Site,
    Date,
    Round,
    Result,
    Eco,
    WhiteElo,
    BlackElo,
    Annotator,
    Fen,
    SetUp,
    Variant,
}

const TAGS: [Tag; 14] = [
    Tag::White,
    Tag::Black,
    Tag::Event,
    Tag::Site,
    Tag::Date,
    Tag::Round,
    Tag::Result,
    Tag::Eco,
    Tag::WhiteElo,
    Tag::BlackElo,
    Tag::Annotator,
    Tag::Fen,
    Tag::SetUp,
    Tag::Variant,
];

impl Tag {
    fn of(name: &[u8]) -> Option<Tag> {
        TAG_NAMES.iter().position(|n| n.as_bytes() == name).map(|i| TAGS[i])
    }
}

/// The values of a game's [`TAG_NAMES`], as written: the first of a repeated
/// tag. A value is at most [`super::lex::MAX_TAG_VALUE`] bytes.
#[derive(Clone, Debug, Default)]
pub struct Tags {
    values: [Vec<u8>; 14],
    present: u16,
}

impl Tags {
    pub fn get(&self, tag: Tag) -> Option<&[u8]> {
        let i = tag as usize;
        (self.present & (1 << i) != 0).then(|| self.values[i].as_slice())
    }

    fn has(&self, tag: Tag) -> bool {
        self.present & (1 << tag as usize) != 0
    }

    fn set(&mut self, tag: Tag, value: &[u8]) {
        let i = tag as usize;
        self.values[i].clear();
        self.values[i].extend_from_slice(value);
        self.present |= 1 << i;
    }

    fn clear(&mut self) {
        self.present = 0;
    }
}

/// One game of the text.
#[derive(Clone, Debug, Default)]
pub struct Game {
    /// The byte offsets of its text: from its first tag or token to the end
    /// of its last.
    pub start: u64,
    pub end: u64,
    pub tags: Tags,
    /// Moves of the main line, counted as written, without playing them.
    pub plies: u32,
    /// The result written at the end of the movetext, if any.
    pub termination: Option<ResultCode>,
    /// Whether every token of the game is valid UTF-8: then its text is read
    /// as UTF-8, else in the code page.
    pub utf8: bool,
}

/// Splits the lexer's tokens into games and passes each to `each` as it ends.
pub struct Splitter<F: FnMut(&Game)> {
    game: Game,
    open: bool,
    /// Whether the open game has movetext, and whether its result was read.
    movetext: bool,
    ended: bool,
    /// A `0-0` read in a game whose result is both lost: the result when the
    /// game ends with it, a castling when a move follows.
    zero_zero: bool,
    each: F,
}

impl<F: FnMut(&Game)> Splitter<F> {
    pub fn new(each: F) -> Self {
        Splitter { game: Game::default(), open: false, movetext: false, ended: false, zero_zero: false, each }
    }

    /// Ends the text: the game still open ends with it.
    pub fn finish(&mut self) {
        self.close();
    }

    fn close(&mut self) {
        if self.open {
            if self.zero_zero {
                self.game.termination = Some(ResultCode::BOTH_LOST);
            }
            (self.each)(&self.game);
            self.open = false;
        }
    }

    fn start(&mut self, at: u64) {
        self.close();
        self.game.start = at;
        self.game.end = at;
        self.game.tags.clear();
        self.game.plies = 0;
        self.game.termination = None;
        self.game.utf8 = true;
        self.open = true;
        self.movetext = false;
        self.ended = false;
        self.zero_zero = false;
    }

    /// A move after a pending `0-0`: that was a castling.
    fn castled(&mut self) {
        if std::mem::take(&mut self.zero_zero) {
            self.game.plies = self.game.plies.saturating_add(1);
        }
    }
}

impl<F: FnMut(&Game)> Sink for Splitter<F> {
    fn tag(&mut self, start: u64, end: u64, name: &[u8], value: &[u8], utf8: bool) {
        let tag = Tag::of(name);
        let again = tag.is_some_and(|t| self.game.tags.has(t));
        if !self.open || self.movetext || self.ended || again {
            self.start(start);
        }
        self.game.end = end;
        self.game.utf8 &= utf8;
        if let Some(tag) = tag {
            self.game.tags.set(tag, value);
        }
    }

    fn movetext(&mut self, start: u64, end: u64, depth: u32, token: Token<'_>, utf8: bool) {
        let moves = matches!(token, Token::Symbol(_) | Token::Star) && depth == 0;
        if !self.open || (self.ended && moves) {
            // Movetext outside any game, such as a comment between games, is
            // no game's; a move after a result starts a game without tags.
            if !moves {
                return;
            }
            self.start(start);
        }
        // A comment may stand between tags: only moves and their marks
        // start the movetext.
        if token != Token::Comment {
            self.movetext = true;
        }
        self.game.end = end;
        self.game.utf8 &= utf8;
        if depth != 0 {
            return;
        }
        match token {
            Token::Star => {
                self.castled();
                self.finish_with(ResultCode::UNKNOWN);
            }
            // A move number.
            Token::Symbol(s) if s.iter().all(u8::is_ascii_digit) => {}
            Token::Symbol(b"0-0") if is_both_lost(self.game.tags.get(Tag::Result)) => {
                self.castled();
                self.zero_zero = true;
            }
            Token::Symbol(s) => {
                self.castled();
                match ResultCode::ending(s) {
                    Some(result) => self.finish_with(result),
                    None => self.game.plies = self.game.plies.saturating_add(1),
                }
            }
            Token::Comment | Token::Other => {}
        }
    }
}

/// Whether a `Result` tag says both players lost: then the movetext's last
/// `0-0` is the result, and any other a castling.
pub fn is_both_lost(result_tag: Option<&[u8]>) -> bool {
    result_tag.is_some_and(|v| v.trim_ascii() == b"0-0")
}

impl<F: FnMut(&Game)> Splitter<F> {
    fn finish_with(&mut self, result: ResultCode) {
        self.game.termination = Some(result);
        self.ended = true;
    }
}

/// A result as the header records store it ([`crate::v2::GameResult::from_field`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResultCode(pub u8);

impl ResultCode {
    pub const BLACK: ResultCode = ResultCode(0);
    pub const DRAW: ResultCode = ResultCode(1);
    pub const WHITE: ResultCode = ResultCode(2);
    /// `0-0`, both players lost, as ChessBase writes a double forfeit.
    pub const BOTH_LOST: ResultCode = ResultCode(7);
    /// `*`: a game in progress or of unknown result; read as `Unknown`, whose
    /// PGN is `*`.
    pub const UNKNOWN: ResultCode = ResultCode(0xff);

    /// A `Result` tag's value; `None` when it is not a result.
    pub fn of_tag(value: &[u8]) -> Option<ResultCode> {
        match value.trim_ascii() {
            b"1-0" => Some(ResultCode::WHITE),
            b"0-1" => Some(ResultCode::BLACK),
            b"1/2-1/2" => Some(ResultCode::DRAW),
            b"0-0" => Some(ResultCode::BOTH_LOST),
            b"*" => Some(ResultCode::UNKNOWN),
            _ => None,
        }
    }

    /// The result a movetext symbol ends the game with, if it is one. A
    /// closing `0-0`, ChessBase's result when both lost, is told from a
    /// castling by the game's end ([`is_both_lost`]).
    pub fn ending(symbol: &[u8]) -> Option<ResultCode> {
        match symbol {
            b"1-0" => Some(ResultCode::WHITE),
            b"0-1" => Some(ResultCode::BLACK),
            b"1/2-1/2" => Some(ResultCode::DRAW),
            _ => None,
        }
    }
}

/// Whether a tag's value says nothing: empty, or `?` as PGN writes an unknown.
pub fn is_unknown(value: &str) -> bool {
    matches!(value.trim(), "" | "?" | "-")
}

/// A `Date` tag's value, `YYYY.MM.DD` with `?` for unknown parts; a part out
/// of range is unknown.
pub fn date(value: &[u8]) -> Date {
    let mut parts = value.trim_ascii().split(|&b| b == b'.' || b == b'/' || b == b'-');
    let mut next = |max: u32| parts.next().and_then(number).filter(|&n| n <= max).unwrap_or(0) as i32;
    let (year, month, day) = (next(4095), next(12), next(31));
    Date((year << 9) | (month << 5) | day)
}

/// A whole number of ASCII digits, at most 9 of them.
fn number(text: &[u8]) -> Option<u32> {
    if text.is_empty() || text.len() > 9 || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(text.iter().fold(0, |n, &d| n * 10 + u32::from(d - b'0')))
}

/// A `Round` tag's round and sub-round: `5`, and `5.2` or `5(2)` (as this
/// reader's PGN writer puts it), 0 where unknown.
pub fn round(value: &[u8]) -> (i16, i16) {
    let value = value.trim_ascii();
    let (round, sub) = match value.iter().position(|&b| b == b'.' || b == b'(') {
        Some(i) if value[i] == b'(' => (&value[..i], value[i + 1..].strip_suffix(b")").unwrap_or(b"x")),
        Some(i) => (&value[..i], &value[i + 1..]),
        None => (value, &b""[..]),
    };
    let part = |p: &[u8]| number(p).map_or(0, |n| n.min(i16::MAX as u32) as i16);
    match part(round) {
        0 => (0, 0),
        r => (r, part(sub)),
    }
}

/// A rating tag's value; 0 when it is not a number.
pub fn elo(value: &[u8]) -> i16 {
    number(value.trim_ascii()).map_or(0, |n| n.min(i16::MAX as u32) as i16)
}

/// An `ECO` tag's value, `A00` to `E99`, as the header's ECO field stores it.
pub fn eco(value: &[u8]) -> u16 {
    match value.trim_ascii() {
        [l @ b'A'..=b'E', d1 @ b'0'..=b'9', d2 @ b'0'..=b'9', ..] => {
            let code = u16::from(l - b'A') * 100 + u16::from(d1 - b'0') * 10 + u16::from(d2 - b'0');
            (code + 1) * 128
        }
        _ => 0,
    }
}

/// The ECO field of a Chess960 game: its start position is not recorded.
pub const CHESS960_ECO: u16 = 64576;

/// What a `Variant` tag names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    Standard,
    Chess960,
    Other,
}

pub fn variant(value: &[u8]) -> Variant {
    let v = String::from_utf8_lossy(value.trim_ascii()).to_lowercase();
    let v: String = v.chars().filter(|c| c.is_alphanumeric()).collect();
    match v.as_str() {
        "" | "standard" | "normal" | "chess" | "fromposition" => Variant::Standard,
        v if v.contains("960") || v.contains("fischerandom") || v.contains("fischerrandom") => Variant::Chess960,
        _ => Variant::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::super::lex::Lexer;
    use super::*;
    use crate::v2::Eco;

    fn split(text: &str) -> Vec<(String, u32, Option<u8>)> {
        let mut games = Vec::new();
        let mut splitter = Splitter::new(|g: &Game| {
            let body = text[g.start as usize..g.end as usize].to_string();
            games.push((body, g.plies, g.termination.map(|r| r.0)));
        });
        // Read again from where a comment left open ends, as a build does.
        let mut lexer = Lexer::new();
        let mut from = 0;
        loop {
            lexer.feed(&text.as_bytes()[from..], &mut splitter);
            match lexer.finish(&mut splitter, true) {
                Some(at) => {
                    from = at as usize;
                    lexer.reset(at);
                }
                None => break,
            }
        }
        splitter.finish();
        games
    }

    #[test]
    fn games_end_at_results_and_at_tags() {
        let text = "[Event \"a\"]\n[White \"x\"]\n\n1. e4 e5 {c} 2. Nf3 (2. f4) 1-0\n\n\
                    [Event \"b\"]\n1. d4 *\n\
                    [Event \"c\"]\n1. c4\n\
                    [Event \"d\"]\n";
        let got = split(text);
        assert_eq!(got.len(), 4);
        assert_eq!(got[0], ("[Event \"a\"]\n[White \"x\"]\n\n1. e4 e5 {c} 2. Nf3 (2. f4) 1-0".into(), 3, Some(2)));
        assert_eq!(got[1], ("[Event \"b\"]\n1. d4 *".into(), 1, Some(0xff)));
        assert_eq!(got[2], ("[Event \"c\"]\n1. c4".into(), 1, None));
        assert_eq!(got[3], ("[Event \"d\"]".into(), 0, None));
    }

    #[test]
    fn games_without_tags_or_movetext() {
        // Games of tags only are told apart by a repeated tag.
        let got = split("[Event \"a\"]\n[Site \"s\"]\n[Event \"b\"]\n");
        assert_eq!(
            got.iter().map(|g| g.0.as_str()).collect::<Vec<_>>(),
            ["[Event \"a\"]\n[Site \"s\"]", "[Event \"b\"]"]
        );
        // Movetext alone, one game after another; a comment between games is
        // no game's, and one after a result stays with its game.
        let got = split("{intro} 1. e4 1-0 {end}\n1. d4 d5 0-1");
        assert_eq!(got.iter().map(|g| g.0.as_str()).collect::<Vec<_>>(), ["1. e4 1-0 {end}", "1. d4 d5 0-1"]);
        assert_eq!(got[1].1, 2);
        assert!(split("{only a comment}\n").is_empty());
    }

    #[test]
    fn zero_zero_ends_a_game_whose_result_is_both_lost() {
        // ChessBase's double forfeit: `0-0` ends the movetext, not a castling.
        let got = split(
            "[Result \"0-0\"]\n\n1. e4 d6 2. Nf3 0-0\n\n[Result \"*\"]\n\n1. Nf3 d5 2. g3 c6 3. Bg2 e6 4. 0-0 *\n",
        );
        assert_eq!(got[0].1, 3);
        assert_eq!(got[0].2, Some(7));
        // Elsewhere `0-0` castles.
        assert_eq!(got[1].1, 7);
        assert_eq!(ResultCode::of_tag(b"0-0"), Some(ResultCode::BOTH_LOST));
        assert_eq!(ResultCode::ending(b"0-0"), None);
        // An earlier `0-0` in such a game is a castling: only the last ends it.
        let got = split("[Result \"0-0\"]\n\n1. e4 e5 2. Nf3 Nc6 3. Bc4 Bc5 4. 0-0 Nf6 5. d3 0-0");
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].1, got[0].2), (9, Some(7)));
    }

    #[test]
    fn comments_line_ends_and_tags_across_lines_keep_one_game() {
        let one = |text: &str| {
            let got = split(text);
            assert_eq!(got.len(), 1, "{text:?}: {got:?}");
            got[0].1
        };
        // Comments between tags, of either kind.
        assert_eq!(one("[Event \"Actual\"]\n; comment\n[White \"Alpha\"]\n[Black \"Beta\"]\n\n1. e4 e5 *"), 2);
        assert_eq!(one("[Event \"Actual\"]\n{comment}\n[White \"Alpha\"]\n\n1. e4 e5 *"), 2);
        // A closed comment that quotes an `Event` line.
        assert_eq!(one("[Event \"Actual\"]\n\n1. e4 {quoted tag:\n[Event \"Example\"]\n} e5 2. Nf3 *"), 3);
        // Comments between a tag's own tokens.
        assert_eq!(one("[Event \"Synthetic\"]\n[White {note} \"Alpha\"]\n[Black \"Beta\"]\n\n1. e4 e5 *"), 2);
        assert_eq!(one("[Event \"Synthetic\"]\n[White ;note\n\"Alpha\"]\n[Black \"Beta\"]\n\n1. e4 e5 *"), 2);
        assert_eq!(one("[Event \"Synthetic\"]\n[White \"Alpha\" {note}]\n[Black \"Beta\"]\n\n1. e4 e5 *"), 2);
        // A tag pair across lines.
        assert_eq!(one("[Event \"A\"]\n[White\n\"Alpha\"]\n[Black \"Beta\"\n]\n\n1. e4 e5 *"), 2);
        // CR alone ends lines.
        assert_eq!(one("[Event \"Actual\"]\r\r1. e4 ; comment\re5 2. Nf3 *\r"), 3);
    }

    #[test]
    fn a_game_is_utf8_when_all_its_tokens_are() {
        let utf8 = |bytes: &[u8]| {
            let mut out = Vec::new();
            let mut splitter = Splitter::new(|g: &Game| out.push(g.utf8));
            let mut lexer = Lexer::new();
            lexer.feed(bytes, &mut splitter);
            lexer.finish(&mut splitter, false);
            splitter.finish();
            out
        };
        assert_eq!(utf8(b"[White \"\xc3\xa9\"]\n\n1. e4 {\xc3\xa9} *\n"), [true]);
        assert_eq!(utf8(b"[White \"\xc3\xa9\"]\n\n1. e4 {\xff} *\n[White \"\xc3\xa9\"]\n\n1. e4 *"), [false, true]);
    }

    #[test]
    fn tag_values() {
        let d = |v: &str| date(v.as_bytes()).pgn();
        assert_eq!(d("1858.??.??"), "1858.??.??");
        assert_eq!(d("2024.02.29"), "2024.02.29");
        assert_eq!(d("2024.13.40"), "2024.??.??");
        assert_eq!(d("????.??.??"), "????.??.??");
        assert_eq!(d("1999"), "1999.??.??");
        assert_eq!(round(b"5"), (5, 0));
        assert_eq!(round(b"5.2"), (5, 2));
        assert_eq!(round(b"5(2)"), (5, 2));
        assert_eq!(round(b"5(2"), (5, 0));
        assert_eq!(round(b"?"), (0, 0));
        assert_eq!(round(b"-"), (0, 0));
        assert_eq!(round(b"99999"), (i16::MAX, 0));
        assert_eq!(elo(b" 2750 "), 2750);
        assert_eq!(elo(b"-"), 0);
        assert_eq!(Eco::from_field(eco(b"B90")).pgn().as_deref(), Some("B90"));
        assert_eq!(Eco::from_field(eco(b"E99a")).pgn().as_deref(), Some("E99"));
        assert_eq!(eco(b"F00"), 0);
        assert!(matches!(Eco::from_field(CHESS960_ECO), Eco::Chess960(0)));
        assert_eq!(ResultCode::of_tag(b"1/2-1/2"), Some(ResultCode::DRAW));
        assert_eq!(ResultCode::of_tag(b"draw"), None);
        assert_eq!(variant(b"Chess960"), Variant::Chess960);
        assert_eq!(variant(b"Fischerandom"), Variant::Chess960);
        assert_eq!(variant(b"From Position"), Variant::Standard);
        assert_eq!(variant(b"Crazyhouse"), Variant::Other);
        assert!(is_unknown(" ? ") && is_unknown("") && !is_unknown("Tal"));
    }
}
