//! The tokens of PGN text, read a byte at a time so that a file of any size is
//! read in bounded memory: tag pairs, and the movetext's symbols with their
//! variation depth. Comments, NAGs, periods, suffixes and parentheses are
//! reported as movetext too, so that a game's extent is known, but carry no
//! text.
//!
//! The lexer is lenient, as PGN in the wild needs: a tag whose closing quote or
//! bracket is missing ends at the end of its line, and a comment left open is
//! closed by a line that starts a game's `Event` tag. Nothing it reads is an
//! error.

/// The longest tag value kept; the rest of a longer one is read and dropped.
pub const MAX_TAG_VALUE: usize = 4 << 10;
/// The longest tag name kept, longer than any tag the reader looks at.
pub const MAX_TAG_NAME: usize = 64;
/// The longest symbol kept: enough for any move, move number or result.
pub const MAX_SYMBOL: usize = 16;
/// The start of a line that closes a comment left open: a game's first tag.
const EVENT_TAG: &[u8] = b"[Event \"";

/// A movetext element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token<'a> {
    /// A move, a move number or a result: at most [`MAX_SYMBOL`] bytes of it.
    Symbol(&'a [u8]),
    /// The result `*`.
    Star,
    /// A comment, a NAG, a period, a suffix, a parenthesis, or any other byte.
    Other,
}

/// What the lexer finds, with the byte offsets it spans in the text.
pub trait Sink {
    /// A tag pair, from its `[` to the byte after its `]`. `name` is at most
    /// 64 bytes and `value` at most [`MAX_TAG_VALUE`] bytes, with its escapes
    /// read.
    fn tag(&mut self, start: u64, end: u64, name: &[u8], value: &[u8]);
    /// A movetext element at variation `depth`: 0 in the main line. A
    /// parenthesis is at the depth outside it.
    fn movetext(&mut self, start: u64, end: u64, depth: u32, token: Token<'_>);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    Comment,
    LineComment,
    Escape,
    TagName,
    /// Between a tag's name and its value.
    TagGap,
    TagValue,
    TagEscape,
    /// After a tag's value, before its `]`.
    TagClose,
    Symbol,
    Nag,
}

pub struct Lexer {
    state: State,
    /// The offset of the next byte.
    pos: u64,
    /// Whether the next byte starts a line.
    line_start: bool,
    depth: u32,
    /// Where the element being read started.
    start: u64,
    name: Vec<u8>,
    value: Vec<u8>,
    symbol: Vec<u8>,
    /// How much of [`EVENT_TAG`] a comment's line has matched, and where.
    probe: usize,
    probe_at: u64,
}

impl Default for Lexer {
    fn default() -> Lexer {
        Lexer::new()
    }
}

fn is_symbol_start(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-'
}

fn is_symbol(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'#' | b'=' | b':' | b'-' | b'/' | b'@')
}

impl Lexer {
    pub fn new() -> Lexer {
        Lexer {
            state: State::Ground,
            pos: 0,
            line_start: true,
            depth: 0,
            start: 0,
            name: Vec::with_capacity(MAX_TAG_NAME),
            value: Vec::with_capacity(MAX_TAG_VALUE),
            symbol: Vec::with_capacity(MAX_SYMBOL),
            probe: 0,
            probe_at: 0,
        }
    }

    /// A lexer whose first byte is at `offset`: for text that does not start
    /// at the beginning of its file.
    pub fn at(offset: u64) -> Lexer {
        Lexer { pos: offset, ..Lexer::new() }
    }

    /// Starts again at `offset`, as [`Lexer::at`], keeping its buffers: a
    /// caller that reads many texts allocates nothing per text.
    pub fn reset(&mut self, offset: u64) {
        self.state = State::Ground;
        self.pos = offset;
        self.line_start = true;
        self.depth = 0;
        self.probe = 0;
        self.name.clear();
        self.value.clear();
        self.symbol.clear();
    }

    /// The offset of the next byte.
    pub fn position(&self) -> u64 {
        self.pos
    }

    /// Reads `bytes`, the text after what was read before.
    pub fn feed(&mut self, bytes: &[u8], sink: &mut impl Sink) {
        for &b in bytes {
            self.byte(b, sink);
            self.line_start = b == b'\n';
            self.pos += 1;
        }
    }

    /// Ends the text: an element still open ends with it.
    pub fn finish(&mut self, sink: &mut impl Sink) {
        match self.state {
            State::Symbol => self.symbol_end(sink),
            State::Nag | State::Comment | State::LineComment => {
                sink.movetext(self.start, self.pos, self.depth, Token::Other)
            }
            State::TagValue | State::TagEscape | State::TagClose => self.tag_end(self.pos, sink),
            State::Ground | State::Escape | State::TagName | State::TagGap => {}
        }
        self.state = State::Ground;
    }

    fn byte(&mut self, b: u8, sink: &mut impl Sink) {
        match self.state {
            State::Ground => self.ground(b, sink),
            State::Comment => self.comment(b, sink),
            State::LineComment => {
                if b == b'\n' {
                    sink.movetext(self.start, self.pos, self.depth, Token::Other);
                    self.state = State::Ground;
                }
            }
            State::Escape => {
                if b == b'\n' {
                    self.state = State::Ground;
                }
            }
            State::TagName => match b {
                b' ' | b'\t' | b'\r' if self.name.is_empty() => {}
                b' ' | b'\t' | b'\r' => self.state = State::TagGap,
                b'"' => self.state = State::TagValue,
                b']' => self.tag_end(self.pos + 1, sink),
                b'\n' => self.tag_end(self.pos, sink),
                b if b.is_ascii_alphanumeric() || b == b'_' => {
                    if self.name.len() < MAX_TAG_NAME {
                        self.name.push(b);
                    }
                }
                // Not a tag after all, such as `[%clk 0:01]` outside a
                // comment: nothing was reported, and the byte is read again.
                _ => {
                    self.state = State::Ground;
                    self.ground(b, sink);
                }
            },
            State::TagGap => match b {
                b' ' | b'\t' | b'\r' => {}
                b'"' => self.state = State::TagValue,
                b']' => self.tag_end(self.pos + 1, sink),
                b'\n' => self.tag_end(self.pos, sink),
                _ => {
                    self.state = State::Ground;
                    self.ground(b, sink);
                }
            },
            State::TagValue => match b {
                b'\\' => self.state = State::TagEscape,
                b'"' => self.state = State::TagClose,
                // A value never spans lines: its closing quote is missing.
                b'\n' => self.tag_end(self.pos, sink),
                _ => self.push_value(b),
            },
            State::TagEscape => {
                if !matches!(b, b'"' | b'\\') {
                    self.push_value(b'\\');
                }
                self.push_value(b);
                self.state = State::TagValue;
            }
            State::TagClose => match b {
                b' ' | b'\t' | b'\r' => {}
                b']' => self.tag_end(self.pos + 1, sink),
                b'\n' => self.tag_end(self.pos, sink),
                // The `]` is missing: the tag ends here and the byte is read again.
                _ => {
                    self.tag_end(self.pos, sink);
                    self.ground(b, sink);
                }
            },
            State::Symbol => {
                if is_symbol(b) {
                    if self.symbol.len() < MAX_SYMBOL {
                        self.symbol.push(b);
                    }
                } else {
                    self.symbol_end(sink);
                    self.ground(b, sink);
                }
            }
            State::Nag => {
                if !b.is_ascii_digit() {
                    sink.movetext(self.start, self.pos, self.depth, Token::Other);
                    self.state = State::Ground;
                    self.ground(b, sink);
                }
            }
        }
    }

    fn ground(&mut self, b: u8, sink: &mut impl Sink) {
        let (at, depth) = (self.pos, self.depth);
        match b {
            b' ' | b'\t' | b'\r' | b'\n' | b'\x0c' | b'\x0b' => {}
            b'[' => {
                self.state = State::TagName;
                self.start = at;
                self.name.clear();
                self.value.clear();
            }
            b'{' => {
                self.state = State::Comment;
                self.start = at;
                self.probe = 0;
            }
            b';' => {
                self.state = State::LineComment;
                self.start = at;
            }
            b'%' if self.line_start => self.state = State::Escape,
            b'(' => {
                sink.movetext(at, at + 1, depth, Token::Other);
                self.depth = depth.saturating_add(1);
            }
            b')' => {
                self.depth = depth.saturating_sub(1);
                sink.movetext(at, at + 1, self.depth, Token::Other);
            }
            b'*' => sink.movetext(at, at + 1, depth, Token::Star),
            b'$' => {
                self.state = State::Nag;
                self.start = at;
            }
            b if is_symbol_start(b) => {
                self.state = State::Symbol;
                self.start = at;
                self.symbol.clear();
                self.symbol.push(b);
            }
            _ => sink.movetext(at, at + 1, depth, Token::Other),
        }
    }

    fn comment(&mut self, b: u8, sink: &mut impl Sink) {
        if b == b'}' {
            sink.movetext(self.start, self.pos + 1, self.depth, Token::Other);
            self.state = State::Ground;
            return;
        }
        if self.line_start && b == EVENT_TAG[0] {
            self.probe = 1;
            self.probe_at = self.pos;
        } else if self.probe > 0 && b == EVENT_TAG[self.probe] {
            self.probe += 1;
            if self.probe == EVENT_TAG.len() {
                // A comment left open ends before the line that starts a game.
                sink.movetext(self.start, self.probe_at, self.depth, Token::Other);
                self.state = State::TagValue;
                self.start = self.probe_at;
                self.name.clear();
                self.name.extend_from_slice(b"Event");
                self.value.clear();
            }
        } else {
            self.probe = 0;
        }
    }

    fn push_value(&mut self, b: u8) {
        if self.value.len() < MAX_TAG_VALUE {
            self.value.push(b);
        }
    }

    fn tag_end(&mut self, end: u64, sink: &mut impl Sink) {
        // A tag starts a new game's header: variations left open end.
        self.depth = 0;
        sink.tag(self.start, end, &self.name, &self.value);
        self.state = State::Ground;
    }

    fn symbol_end(&mut self, sink: &mut impl Sink) {
        sink.movetext(self.start, self.pos, self.depth, Token::Symbol(&self.symbol));
        self.state = State::Ground;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Log(Vec<String>);

    impl Sink for Log {
        fn tag(&mut self, start: u64, end: u64, name: &[u8], value: &[u8]) {
            let (name, value) = (String::from_utf8_lossy(name), String::from_utf8_lossy(value));
            self.0.push(format!("{start}-{end} [{name}={value}]"));
        }
        fn movetext(&mut self, start: u64, end: u64, depth: u32, token: Token<'_>) {
            let what = match token {
                Token::Symbol(s) => String::from_utf8_lossy(s).into_owned(),
                Token::Star => "*".into(),
                Token::Other => "~".into(),
            };
            self.0.push(format!("{start}-{end}@{depth} {what}"));
        }
    }

    fn lex(text: &str) -> Vec<String> {
        let mut log = Log::default();
        let mut lexer = Lexer::new();
        // Fed a byte at a time, as chunk boundaries may fall anywhere.
        for b in text.as_bytes() {
            lexer.feed(std::slice::from_ref(b), &mut log);
        }
        lexer.finish(&mut log);
        log.0
    }

    #[test]
    fn tags_symbols_and_depth() {
        let got = lex("[White \"Tal, M\"]\n1. e4 (1. d4 $1) e5!? {a [b]} 1-0");
        let want = [
            "0-16 [White=Tal, M]",
            "17-18@0 1",
            "18-19@0 ~",
            "20-22@0 e4",
            "23-24@0 ~",
            "24-25@1 1",
            "25-26@1 ~",
            "27-29@1 d4",
            "30-32@1 ~",
            "32-33@0 ~",
            "34-36@0 e5",
            "36-37@0 ~",
            "37-38@0 ~",
            "39-46@0 ~",
            "47-50@0 1-0",
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn escapes_and_broken_tags() {
        assert_eq!(lex(r#"[Event "a \"b\" \\ c"]"#), [r#"0-22 [Event=a "b" \ c]"#]);
        // A missing quote or bracket ends the tag at the end of its line.
        assert_eq!(lex("[Site \"x\n[Date \"y\" \n"), ["0-8 [Site=x]", "9-19 [Date=y]"]);
        // Not a tag: read again as movetext.
        assert_eq!(lex("[%clk]"), ["1-2@0 ~", "2-5@0 clk", "5-6@0 ~"]);
    }

    #[test]
    fn comments_escapes_and_nags() {
        assert_eq!(lex("; line\n%escape [x \"y\"]\n*"), ["0-6@0 ~", "23-24@0 *"]);
        // `%` starts an escape only at the start of a line.
        assert_eq!(lex("e4 %x"), ["0-2@0 e4", "3-4@0 ~", "4-5@0 x"]);
        assert_eq!(lex("$12e4"), ["0-3@0 ~", "3-5@0 e4"]);
    }

    #[test]
    fn an_open_comment_ends_at_a_games_event_tag() {
        let got = lex("e4 {open\n[Event \"Next\"]\nd4");
        assert_eq!(got, ["0-2@0 e4", "3-9@0 ~", "9-23 [Event=Next]", "24-26@0 d4"]);
        // Elsewhere in a comment a bracket is text.
        assert_eq!(lex("{x\n [Event \"y\"]}"), ["0-16@0 ~"]);
    }

    #[test]
    fn long_values_and_symbols_are_cut() {
        let long = "x".repeat(MAX_TAG_VALUE + 10);
        let got = lex(&format!("[Event \"{long}\"]"));
        assert_eq!(got[0], format!("0-{} [Event={}]", MAX_TAG_VALUE + 20, &long[..MAX_TAG_VALUE]));
        let sym = "a".repeat(40);
        assert_eq!(lex(&sym), [format!("0-40@0 {}", &sym[..MAX_SYMBOL])]);
    }

    #[test]
    fn a_tag_closes_open_variations() {
        assert_eq!(lex("((e4 [A \"b\"] d4"), ["0-1@0 ~", "1-2@1 ~", "2-4@2 e4", "5-12 [A=b]", "13-15@0 d4"]);
    }
}
