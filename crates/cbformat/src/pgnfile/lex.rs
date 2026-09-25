//! The tokens of PGN text, read a byte at a time so that a file of any size is
//! read in bounded memory: tag pairs, and the movetext's symbols with their
//! variation depth. Comments, NAGs, periods, suffixes and parentheses are
//! reported as movetext too, so that a game's extent is known. Each token
//! says whether its bytes are valid UTF-8, so that a reader can decide a
//! game's encoding from its tokens.
//!
//! The lexer is lenient, as PGN in the wild needs, and nothing it reads is an
//! error. A line ends at LF, CR or both. A tag pair may span lines between its
//! tokens; a tag whose value is not closed on its line ends there, and one
//! whose `]` is missing ends after its value. A `{` comment ends at its `}`,
//! whatever it holds. Only a comment still open at the end of the text is
//! taken for one left open by mistake: [`Lexer::finish`] then ends it before
//! its first line starting `[Event "`, where a game's header starts, and names
//! that offset for the text to be read again from there.

/// The longest tag value kept; the rest of a longer one is read and dropped.
pub const MAX_TAG_VALUE: usize = 4 << 10;
/// The longest tag name kept, longer than any tag the reader looks at.
pub const MAX_TAG_NAME: usize = 64;
/// The longest symbol kept: enough for any move, move number or result.
pub const MAX_SYMBOL: usize = 16;
const EVENT_TAG: &[u8] = b"[Event \"";

/// A movetext element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token<'a> {
    /// A move, a move number or a result: at most [`MAX_SYMBOL`] bytes of it.
    Symbol(&'a [u8]),
    /// The result `*`.
    Star,
    /// A `{…}` or `;` comment, or a `%` escape line.
    Comment,
    /// A NAG, a period, a suffix, a parenthesis, or any other byte.
    Other,
    /// Bytes read that make no element: a tag given up, or bytes above 0x7f
    /// outside any token. They belong to the game around them, but are no
    /// part of its moves.
    Skipped,
}

/// What the lexer finds, with the byte offsets it spans in the text and
/// whether its bytes are valid UTF-8.
pub trait Sink {
    /// A tag pair, from its `[` to the byte after its `]` (after its value
    /// when the `]` is missing). `name` is at most [`MAX_TAG_NAME`] bytes and
    /// `value` at most [`MAX_TAG_VALUE`] bytes, with its escapes read.
    fn tag(&mut self, start: u64, end: u64, name: &[u8], value: &[u8], utf8: bool);
    /// A movetext element at variation `depth`: 0 in the main line. A
    /// parenthesis is at the depth outside it.
    fn movetext(&mut self, start: u64, end: u64, depth: u32, token: Token<'_>, utf8: bool);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    /// A run of bytes above 0x7f outside any token.
    Junk,
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
    /// A `{` comment, or a `;` comment or `%` escape line, between a tag's
    /// tokens; the tag goes on in [`Lexer::tag_resume`] after it.
    TagComment,
    TagLineComment,
    Symbol,
    Nag,
}

/// A UTF-8 check, a byte at a time.
#[derive(Clone, Copy, Debug)]
struct Utf8 {
    /// Continuation bytes still due, and the range of the next one.
    need: u8,
    lo: u8,
    hi: u8,
    bad: bool,
}

impl Utf8 {
    const NEW: Utf8 = Utf8 { need: 0, lo: 0x80, hi: 0xbf, bad: false };

    fn push(&mut self, b: u8) {
        if self.need > 0 {
            if (self.lo..=self.hi).contains(&b) {
                self.need -= 1;
                self.lo = 0x80;
                self.hi = 0xbf;
                return;
            }
            // A sequence cut short; the byte starts again.
            self.bad = true;
            self.need = 0;
            self.lo = 0x80;
            self.hi = 0xbf;
        }
        match b {
            0x00..=0x7f => {}
            0xc2..=0xdf => self.need = 1,
            0xe0 => (self.need, self.lo) = (2, 0xa0),
            0xe1..=0xec | 0xee | 0xef => self.need = 2,
            0xed => (self.need, self.hi) = (2, 0x9f),
            0xf0 => (self.need, self.lo) = (3, 0x90),
            0xf1..=0xf3 => self.need = 3,
            0xf4 => (self.need, self.hi) = (3, 0x8f),
            _ => self.bad = true,
        }
    }

    fn valid(&self) -> bool {
        !self.bad && self.need == 0
    }
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
    /// The UTF-8 check of the element being read.
    utf8: Utf8,
    /// The tag state a comment between a tag's tokens returns to.
    tag_resume: State,
    /// Bytes of `[Event "` matched at the start of a line in the open
    /// comment, and where that line starts.
    probe: usize,
    probe_at: u64,
    /// The comment's UTF-8 check before that line.
    header_utf8: Utf8,
    /// The first such line of the open comment, with the comment's UTF-8
    /// check before it: where the comment ends if it is never closed.
    header: Option<(u64, Utf8)>,
}

impl Default for Lexer {
    fn default() -> Lexer {
        Lexer::new()
    }
}

fn is_eol(b: u8) -> bool {
    b == b'\n' || b == b'\r'
}

fn is_blank(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'\x0b')
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
            utf8: Utf8::NEW,
            tag_resume: State::TagGap,
            probe: 0,
            probe_at: 0,
            header_utf8: Utf8::NEW,
            header: None,
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
        self.name.clear();
        self.value.clear();
        self.symbol.clear();
        self.utf8 = Utf8::NEW;
        self.probe = 0;
        self.header = None;
    }

    /// The offset of the next byte.
    pub fn position(&self) -> u64 {
        self.pos
    }

    /// Reads `bytes`, the text after what was read before.
    pub fn feed(&mut self, bytes: &[u8], sink: &mut impl Sink) {
        for &b in bytes {
            self.step(b, sink);
        }
    }

    /// Ends the text: an element still open ends with it. With `resume`, a
    /// comment still open that holds a line starting `[Event "` ends before
    /// that line instead, and its offset is returned: the text from there is
    /// then to be read again, by this lexer from [`Lexer::reset`] at it.
    pub fn finish(&mut self, sink: &mut impl Sink, resume: bool) -> Option<u64> {
        if let (State::Comment, Some((at, utf8)), true) = (self.state, self.header, resume) {
            sink.movetext(self.start, at, self.depth, Token::Comment, utf8.valid());
            self.state = State::Ground;
            self.header = None;
            return Some(at);
        }
        let (start, end, depth, valid) = (self.start, self.pos, self.depth, self.utf8.valid());
        match self.state {
            State::Symbol => self.symbol_end(end, sink),
            State::Nag => sink.movetext(start, end, depth, Token::Other, true),
            State::Comment | State::LineComment | State::Escape => {
                sink.movetext(start, end, depth, Token::Comment, valid)
            }
            // A tag with its value ends with the text.
            State::TagValue | State::TagEscape | State::TagClose => self.tag_end(end, sink),
            State::TagComment | State::TagLineComment if self.tag_resume == State::TagClose => self.tag_end(end, sink),
            // A tag without one is given up.
            State::Junk | State::TagName | State::TagGap | State::TagComment | State::TagLineComment => {
                sink.movetext(start, end, depth, Token::Skipped, valid)
            }
            State::Ground => {}
        }
        self.state = State::Ground;
        self.header = None;
        None
    }

    fn step(&mut self, b: u8, sink: &mut impl Sink) {
        let at = self.pos;
        let starts_line = self.line_start;
        self.pos += 1;
        self.line_start = is_eol(b);
        self.byte(b, at, starts_line, sink);
    }

    fn byte(&mut self, b: u8, at: u64, starts_line: bool, sink: &mut impl Sink) {
        match self.state {
            State::Ground => self.ground(b, at, starts_line, sink),
            State::Junk => {
                if b >= 0x80 {
                    self.utf8.push(b);
                } else {
                    sink.movetext(self.start, at, self.depth, Token::Skipped, self.utf8.valid());
                    self.state = State::Ground;
                    self.ground(b, at, starts_line, sink);
                }
            }
            State::Comment => self.comment(b, at, starts_line, sink),
            State::LineComment | State::Escape => {
                if is_eol(b) {
                    sink.movetext(self.start, at, self.depth, Token::Comment, self.utf8.valid());
                    self.state = State::Ground;
                } else {
                    self.utf8.push(b);
                }
            }
            State::TagName => {
                let resume = if self.name.is_empty() { State::TagName } else { State::TagGap };
                if self.tag_gap(b, starts_line, resume) {
                    return;
                }
                match b {
                    _ if is_blank(b) && self.name.is_empty() => {}
                    _ if is_blank(b) => self.state = State::TagGap,
                    b'"' => {
                        self.utf8.push(b);
                        self.state = State::TagValue;
                    }
                    b']' => self.tag_end(at + 1, sink),
                    b if b.is_ascii_alphanumeric() || b == b'_' => {
                        if self.name.len() < MAX_TAG_NAME {
                            self.name.push(b);
                        }
                    }
                    // Not a tag after all, such as `[%clk 0:01]` outside a
                    // comment.
                    _ => self.give_up_tag(b, at, starts_line, sink),
                }
            }
            State::TagGap => {
                if self.tag_gap(b, starts_line, State::TagGap) {
                    return;
                }
                match b {
                    _ if is_blank(b) => {}
                    b'"' => {
                        self.utf8.push(b);
                        self.state = State::TagValue;
                    }
                    b']' => self.tag_end(at + 1, sink),
                    _ => self.give_up_tag(b, at, starts_line, sink),
                }
            }
            State::TagValue => match b {
                b'\\' => {
                    self.utf8.push(b);
                    self.state = State::TagEscape;
                }
                b'"' => self.state = State::TagClose,
                // A value never spans lines: its closing quote is missing.
                _ if is_eol(b) => self.tag_end(at, sink),
                _ => {
                    self.utf8.push(b);
                    self.push_value(b);
                }
            },
            State::TagEscape => {
                if !matches!(b, b'"' | b'\\') {
                    self.push_value(b'\\');
                }
                self.utf8.push(b);
                self.push_value(b);
                self.state = State::TagValue;
            }
            State::TagClose => {
                if self.tag_gap(b, starts_line, State::TagClose) {
                    return;
                }
                match b {
                    _ if is_blank(b) => {}
                    b']' => self.tag_end(at + 1, sink),
                    // The `]` is missing: the tag ends with what was read of
                    // it, and the byte is read again.
                    _ => {
                        self.tag_end(at, sink);
                        self.ground(b, at, starts_line, sink);
                    }
                }
            }
            // A comment between a tag's tokens: the tag goes on after it.
            State::TagComment => {
                self.utf8.push(b);
                if b == b'}' {
                    self.state = self.tag_resume;
                }
            }
            State::TagLineComment => {
                if is_eol(b) {
                    self.state = self.tag_resume;
                } else {
                    self.utf8.push(b);
                }
            }
            State::Symbol => {
                if is_symbol(b) {
                    if self.symbol.len() < MAX_SYMBOL {
                        self.symbol.push(b);
                    }
                } else {
                    self.symbol_end(at, sink);
                    self.ground(b, at, starts_line, sink);
                }
            }
            State::Nag => {
                if !b.is_ascii_digit() {
                    sink.movetext(self.start, at, self.depth, Token::Other, true);
                    self.state = State::Ground;
                    self.ground(b, at, starts_line, sink);
                }
            }
        }
    }

    /// Starts a comment or an escape line between a tag's tokens when `b`
    /// opens one, the tag going on in `resume` after it; whether it did.
    fn tag_gap(&mut self, b: u8, starts_line: bool, resume: State) -> bool {
        self.state = match b {
            b'{' => State::TagComment,
            b';' => State::TagLineComment,
            b'%' if starts_line => State::TagLineComment,
            _ => return false,
        };
        self.utf8.push(b);
        self.tag_resume = resume;
        true
    }

    /// A tag given up at `at`: what was read of it is skipped, and `b` is read
    /// again.
    fn give_up_tag(&mut self, b: u8, at: u64, starts_line: bool, sink: &mut impl Sink) {
        sink.movetext(self.start, at, self.depth, Token::Skipped, self.utf8.valid());
        self.state = State::Ground;
        self.ground(b, at, starts_line, sink);
    }

    /// Starts an element at `at` in `state`, its UTF-8 check from `b`.
    fn begin(&mut self, state: State, at: u64, b: u8) {
        self.state = state;
        self.start = at;
        self.utf8 = Utf8::NEW;
        self.utf8.push(b);
    }

    fn ground(&mut self, b: u8, at: u64, starts_line: bool, sink: &mut impl Sink) {
        let depth = self.depth;
        match b {
            _ if is_blank(b) => {}
            b'[' => {
                self.begin(State::TagName, at, b);
                self.name.clear();
                self.value.clear();
            }
            b'{' => {
                self.begin(State::Comment, at, b);
                self.probe = 0;
                self.header = None;
            }
            b';' => self.begin(State::LineComment, at, b),
            b'%' if starts_line => self.begin(State::Escape, at, b),
            b'(' => {
                sink.movetext(at, at + 1, depth, Token::Other, true);
                self.depth = depth.saturating_add(1);
            }
            b')' => {
                self.depth = depth.saturating_sub(1);
                sink.movetext(at, at + 1, self.depth, Token::Other, true);
            }
            b'*' => sink.movetext(at, at + 1, depth, Token::Star, true),
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
            0x80.. => self.begin(State::Junk, at, b),
            _ => sink.movetext(at, at + 1, depth, Token::Other, true),
        }
    }

    fn comment(&mut self, b: u8, at: u64, starts_line: bool, sink: &mut impl Sink) {
        if b == b'}' {
            self.utf8.push(b);
            sink.movetext(self.start, at + 1, self.depth, Token::Comment, self.utf8.valid());
            self.state = State::Ground;
            self.header = None;
            return;
        }
        // The first line of the comment that starts a game's header.
        if self.header.is_none() {
            if starts_line && b == EVENT_TAG[0] {
                self.probe = 1;
                self.probe_at = at;
                self.header_utf8 = self.utf8;
            } else if self.probe > 0 && b == EVENT_TAG[self.probe] {
                self.probe += 1;
                if self.probe == EVENT_TAG.len() {
                    self.header = Some((self.probe_at, self.header_utf8));
                    self.probe = 0;
                }
            } else {
                self.probe = 0;
            }
        }
        self.utf8.push(b);
    }

    fn push_value(&mut self, b: u8) {
        if self.value.len() < MAX_TAG_VALUE {
            self.value.push(b);
        }
    }

    fn tag_end(&mut self, end: u64, sink: &mut impl Sink) {
        // A tag starts a new game's header: variations left open end.
        self.depth = 0;
        sink.tag(self.start, end, &self.name, &self.value, self.utf8.valid());
        self.state = State::Ground;
    }

    fn symbol_end(&mut self, end: u64, sink: &mut impl Sink) {
        sink.movetext(self.start, end, self.depth, Token::Symbol(&self.symbol), true);
        self.state = State::Ground;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Log(Vec<String>);

    impl Sink for Log {
        fn tag(&mut self, start: u64, end: u64, name: &[u8], value: &[u8], utf8: bool) {
            let (name, value) = (String::from_utf8_lossy(name), String::from_utf8_lossy(value));
            let bad = if utf8 { "" } else { " !utf8" };
            self.0.push(format!("{start}-{end} [{name}={value}]{bad}"));
        }
        fn movetext(&mut self, start: u64, end: u64, depth: u32, token: Token<'_>, utf8: bool) {
            let what = match token {
                Token::Symbol(s) => String::from_utf8_lossy(s).into_owned(),
                Token::Star => "*".into(),
                Token::Comment => "{}".into(),
                Token::Other => "~".into(),
                Token::Skipped => "skip".into(),
            };
            let bad = if utf8 { "" } else { " !utf8" };
            self.0.push(format!("{start}-{end}@{depth} {what}{bad}"));
        }
    }

    /// The tokens of `text`, read again from where a comment left open ends,
    /// as a reader of a file does.
    fn lex_bytes(text: &[u8]) -> Vec<String> {
        let mut log = Log::default();
        let mut lexer = Lexer::new();
        let mut from = 0;
        loop {
            // Fed a byte at a time, as chunk boundaries may fall anywhere.
            for b in &text[from..] {
                lexer.feed(std::slice::from_ref(b), &mut log);
            }
            match lexer.finish(&mut log, true) {
                Some(at) => {
                    from = at as usize;
                    lexer.reset(at);
                }
                None => return log.0,
            }
        }
    }

    fn lex(text: &str) -> Vec<String> {
        lex_bytes(text.as_bytes())
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
            "39-46@0 {}",
            "47-50@0 1-0",
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn escapes_and_broken_tags() {
        assert_eq!(lex(r#"[Event "a \"b\" \\ c"]"#), [r#"0-22 [Event=a "b" \ c]"#]);
        // A value not closed on its line ends the tag there; a missing `]`
        // ends it after its value.
        assert_eq!(lex("[Site \"x\n[Date \"y\" \n1."), ["0-8 [Site=x]", "9-20 [Date=y]", "20-21@0 1", "21-22@0 ~"]);
        // Not a tag: read again as movetext.
        assert_eq!(lex("[%clk]"), ["0-1@0 skip", "1-2@0 ~", "2-5@0 clk", "5-6@0 ~"]);
    }

    #[test]
    fn comments_may_stand_between_a_tags_tokens() {
        assert_eq!(lex("[White {note} \"Alpha\"]"), ["0-22 [White=Alpha]"]);
        assert_eq!(lex("[White ;note\n\"Alpha\"]"), ["0-21 [White=Alpha]"]);
        assert_eq!(lex("[White \"Alpha\" {note}]"), ["0-22 [White=Alpha]"]);
        assert_eq!(lex("[White{a}{b}\"Alpha\"{c}]"), ["0-23 [White=Alpha]"]);
        // Before the name too, and escape lines in any gap.
        assert_eq!(lex("[ {note} White \"Alpha\"]"), ["0-23 [White=Alpha]"]);
        assert_eq!(lex("[ ;note\nWhite \"Alpha\"]"), ["0-22 [White=Alpha]"]);
        assert_eq!(lex("[\n%esc\nWhite\n%esc\n\"Alpha\"\n%esc\n]"), ["0-32 [White=Alpha]"]);
        // Their bytes count in the tag's UTF-8 check.
        assert_eq!(lex_bytes(b"[White {\xff} \"A\"]"), ["0-15 [White=A] !utf8"]);
        // A tag given up, or ended by the text, keeps what it read.
        assert_eq!(lex_bytes(b"[Event {\xff} broken"), ["0-11@0 skip !utf8", "11-17@0 broken"]);
        assert_eq!(lex_bytes(b"[White \"A\" {\xff}"), ["0-14 [White=A] !utf8"]);
    }

    #[test]
    fn a_tag_pair_may_span_lines() {
        assert_eq!(lex("[White\n\"Alpha\"\n]\n[Black \"B\"]"), ["0-16 [White=Alpha]", "17-28 [Black=B]"]);
        assert_eq!(lex("[White\r\"Alpha\"]"), ["0-15 [White=Alpha]"]);
    }

    #[test]
    fn comments_escapes_and_nags() {
        assert_eq!(lex("; line\n%escape [x \"y\"]\n*"), ["0-6@0 {}", "7-22@0 {}", "23-24@0 *"]);
        // `%` starts an escape only at the start of a line.
        assert_eq!(lex("e4 %x"), ["0-2@0 e4", "3-4@0 ~", "4-5@0 x"]);
        assert_eq!(lex("$12e4"), ["0-3@0 ~", "3-5@0 e4"]);
        // CR alone ends a line.
        assert_eq!(lex("e4 ; c\re5"), ["0-2@0 e4", "3-6@0 {}", "7-9@0 e5"]);
        assert_eq!(lex("e4\r%x\re5"), ["0-2@0 e4", "3-5@0 {}", "6-8@0 e5"]);
    }

    #[test]
    fn a_comment_open_at_the_end_ends_at_its_first_header() {
        let got = lex("e4 {open\n[Event \"Next\"]\r\n[Site \"S\"]\nd4 {again\n[Event \"Third\"]");
        let want = [
            "0-2@0 e4",
            "3-9@0 {}",
            "9-23 [Event=Next]",
            "25-35 [Site=S]",
            "36-38@0 d4",
            "39-46@0 {}",
            "46-61 [Event=Third]",
        ];
        assert_eq!(got, want);
        // Without `resume` the comment runs to the end.
        let mut log = Log::default();
        let mut lexer = Lexer::new();
        lexer.feed(b"{open\n[Event \"N\"]", &mut log);
        assert_eq!(lexer.finish(&mut log, false), None);
        assert_eq!(log.0, ["0-17@0 {}"]);
        // A comment open at the end with no header in it runs to the end.
        assert_eq!(lex("e4 {open\n1. d4"), ["0-2@0 e4", "3-14@0 {}"]);
    }

    #[test]
    fn a_closed_comment_keeps_whatever_it_holds() {
        let text = "1. e4 {quoted tag:\n[Event \"Example\"]\n} e5 2. Nf3 *";
        let got = lex(text);
        assert_eq!((got[3].as_str(), got.len()), ("6-38@0 {}", 9), "{got:?}");
        // Quoted header lines, however many, stay the comment's.
        let text = "1. e4 {quoted header:\n[Event \"Example\"]\n[Site \"Somewhere\"]\n} e5 2. Nf3 *";
        let got = lex(text);
        assert_eq!((got[3].as_str(), got.len()), ("6-60@0 {}", 9), "{got:?}");
        // Elsewhere in a comment a bracket is text.
        assert_eq!(lex("{x\n [Event \"y\"]}"), ["0-16@0 {}"]);
    }

    #[test]
    fn tokens_say_whether_they_are_utf8() {
        let got = lex_bytes(b"[White \"\xc3\xa9\"] [Black \"\xe9\"] {\xff} e4 \xe2\x80\x94 \xe9");
        assert_eq!(
            got,
            [
                "0-12 [White=\u{e9}]",
                "13-24 [Black=\u{fffd}] !utf8",
                "25-28@0 {} !utf8",
                "29-31@0 e4",
                "32-35@0 skip",
                "36-37@0 skip !utf8",
            ]
        );
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

    #[test]
    fn an_escape_is_checked_as_written() {
        // `\xc3 \\ \xa9` is not UTF-8, though `\xc3\xa9` would be.
        assert_eq!(lex_bytes(b"[Event \"\xc3\\\xa9\"]"), ["0-13 [Event=\u{fffd}\\\u{fffd}] !utf8"]);
    }
}
