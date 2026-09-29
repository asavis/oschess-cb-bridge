//! The bridge's answers as `cbtool profile` reads them (#191): parsed whole,
//! and their members found by name, never by where they are written, so that
//! an answer whose members change order reads the same. The bridge has a JSON
//! writer only (`bridge::json`), and cbtool takes no JSON library.

use std::collections::BTreeMap;

/// A JSON value. A number keeps its text, so that values compare as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Value {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

/// What [`Value::get`] finds for a member that is not there.
static NULL: Value = Value::Null;

/// How deep arrays and objects nest at most; the bridge's answers nest a few
/// levels.
const DEPTH: usize = 64;

impl Value {
    /// `text`, one JSON value with white space around it at most; null when
    /// it is anything else, as a failed answer's HTML or a cut stream.
    pub(super) fn of(text: &[u8]) -> Value {
        let mut p = Parser { text, at: 0 };
        let value = p.value(0).filter(|_| {
            p.space();
            p.at == text.len()
        });
        value.unwrap_or(Value::Null)
    }

    /// The member `key` of an object; null when there is none.
    pub(super) fn get(&self, key: &str) -> &Value {
        self.members().and_then(|m| m.get(key)).unwrap_or(&NULL)
    }

    /// The members of an object.
    pub(super) fn members(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }

    /// The items of an array; none for anything else.
    pub(super) fn items(&self) -> &[Value] {
        match self {
            Value::Array(items) => items,
            _ => &[],
        }
    }

    pub(super) fn str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// A number that is a whole number from 0.
    pub(super) fn u64(&self) -> Option<u64> {
        match self {
            Value::Number(n) => n.parse().ok(),
            _ => None,
        }
    }
}

struct Parser<'a> {
    text: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    /// Takes `byte` when it comes next.
    fn eat(&mut self, byte: u8) -> bool {
        let next = self.peek() == Some(byte);
        self.at += usize::from(next);
        next
    }

    /// Takes `word` when it comes next.
    fn word(&mut self, word: &[u8]) -> bool {
        let next = self.text[self.at..].starts_with(word);
        if next {
            self.at += word.len();
        }
        next
    }

    fn value(&mut self, depth: usize) -> Option<Value> {
        self.space();
        match self.peek()? {
            b'{' if depth < DEPTH => {
                self.at += 1;
                let mut members = BTreeMap::new();
                self.space();
                if self.eat(b'}') {
                    return Some(Value::Object(members));
                }
                loop {
                    self.space();
                    let key = self.string()?;
                    self.space();
                    if !self.eat(b':') {
                        return None;
                    }
                    members.insert(key, self.value(depth + 1)?);
                    self.space();
                    if self.eat(b'}') {
                        return Some(Value::Object(members));
                    }
                    if !self.eat(b',') {
                        return None;
                    }
                }
            }
            b'[' if depth < DEPTH => {
                self.at += 1;
                let mut items = Vec::new();
                self.space();
                if self.eat(b']') {
                    return Some(Value::Array(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.space();
                    if self.eat(b']') {
                        return Some(Value::Array(items));
                    }
                    if !self.eat(b',') {
                        return None;
                    }
                }
            }
            b'"' => self.string().map(Value::String),
            b't' if self.word(b"true") => Some(Value::Bool(true)),
            b'f' if self.word(b"false") => Some(Value::Bool(false)),
            b'n' if self.word(b"null") => Some(Value::Null),
            b'-' | b'0'..=b'9' => self.number().map(Value::Number),
            _ => None,
        }
    }

    /// A number's text: `-`, its digits, and its fraction and exponent.
    fn number(&mut self) -> Option<String> {
        let from = self.at;
        self.eat(b'-');
        let digits = |p: &mut Self| {
            let start = p.at;
            while p.peek().is_some_and(|b| b.is_ascii_digit()) {
                p.at += 1;
            }
            p.at > start
        };
        if !digits(self) {
            return None;
        }
        if self.eat(b'.') && !digits(self) {
            return None;
        }
        if self.eat(b'e') || self.eat(b'E') {
            let _ = self.eat(b'+') || self.eat(b'-');
            if !digits(self) {
                return None;
            }
        }
        std::str::from_utf8(&self.text[from..self.at]).ok().map(str::to_string)
    }

    /// A string, unescaped. A surrogate that is not half of a pair is the
    /// replacement character.
    fn string(&mut self) -> Option<String> {
        if !self.eat(b'"') {
            return None;
        }
        let mut out = Vec::new();
        loop {
            let byte = self.peek()?;
            self.at += 1;
            match byte {
                b'"' => return String::from_utf8(out).ok(),
                b'\\' => {
                    let escaped = self.peek()?;
                    self.at += 1;
                    let c = match escaped {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => self.unicode()?,
                        _ => return None,
                    };
                    out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                }
                0..=0x1f => return None,
                other => out.push(other),
            }
        }
    }

    /// The character of a `\u` escape whose `\u` is read: one code unit, or
    /// a surrogate pair's two.
    fn unicode(&mut self) -> Option<char> {
        let high = self.hex()?;
        if !(0xd800..0xdc00).contains(&high) {
            return Some(char::from_u32(high).unwrap_or('\u{fffd}'));
        }
        let pair = self.at;
        if self.word(b"\\u") {
            let low = self.hex()?;
            if (0xdc00..0xe000).contains(&low) {
                return char::from_u32(0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00));
            }
            // Not the pair's low half: read again as a character of its own.
            self.at = pair;
        }
        Some('\u{fffd}')
    }

    fn hex(&mut self) -> Option<u32> {
        let digits = self.text.get(self.at..self.at + 4).filter(|d| d.iter().all(u8::is_ascii_hexdigit))?;
        let code = u32::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()?;
        self.at += 4;
        Some(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(members: &[(&str, Value)]) -> Value {
        Value::Object(members.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
    }

    #[test]
    fn values_are_read_whole() {
        let v = Value::of(
            br#" {"total":1234,"rows":[{"value":"Tal, Mikhail"},{"value":"Caf\u00e9 \"X\" \ud83d\ude00"}],
            "flags":{"deleted":false,"chess960":true},"year":null,"score":-1.5e+3} "#,
        );
        assert_eq!(v.get("total").u64(), Some(1234));
        assert_eq!(v.get("none"), &Value::Null);
        assert_eq!(v.get("total").get("x"), &Value::Null);
        let values: Vec<_> = v.get("rows").items().iter().map(|r| r.get("value").str()).collect();
        assert_eq!(values, [Some("Tal, Mikhail"), Some("Caf\u{e9} \"X\" \u{1f600}")]);
        assert_eq!(v.get("flags"), &object(&[("chess960", Value::Bool(true)), ("deleted", Value::Bool(false))]));
        assert_eq!(v.get("score"), &Value::Number("-1.5e+3".into()));
        assert_eq!(v.get("score").u64(), None);
        assert!(v.get("total").items().is_empty());
        // A surrogate that is not half of a pair is a replacement character,
        // and what follows it stays.
        assert_eq!(Value::of(br#""\ud83d\u0041""#), Value::String("\u{fffd}A".into()));
        assert_eq!(Value::of(br#""\ude00x""#), Value::String("\u{fffd}x".into()));
    }

    /// Members are found by name wherever they are written, and objects
    /// compare member for member in any order.
    #[test]
    fn member_order_does_not_matter() {
        let a = Value::of(br#"{"number":7,"white":"A","flags":{"deleted":false,"chess960":false}}"#);
        let b = Value::of(br#"{"flags":{"chess960":false,"deleted":false},"white":"A","number":7}"#);
        assert_eq!(a, b);
        assert_eq!(b.get("number").u64(), Some(7));
        assert_ne!(a, Value::of(br#"{"number":7,"white":"B","flags":{"deleted":false,"chess960":false}}"#));
    }

    /// Anything but one JSON value is null, never a panic: a failed answer's
    /// HTML, a cut stream, a nesting deeper than any answer's.
    #[test]
    fn anything_else_is_null() {
        let deep = format!("{}{}", "[".repeat(DEPTH + 1), "]".repeat(DEPTH + 1));
        let fits = format!("{}{}", "[".repeat(DEPTH), "]".repeat(DEPTH));
        assert_ne!(Value::of(fits.as_bytes()), Value::Null);
        for text in [
            &b"<html>C:\\secret</html>"[..],
            b"",
            b"{\"a\":1",
            b"{\"a\":1}}",
            b"{\"a\" 1}",
            b"[1,]",
            b"\"\\u12\"",
            b"\"\\u+123\"",
            b"\"a\nb\"",
            b"\"\\x\"",
            b"-",
            b"1.",
            b"1e",
            b"tru",
            b"\"\xff\"",
            deep.as_bytes(),
        ] {
            assert_eq!(Value::of(text), Value::Null, "{}", String::from_utf8_lossy(text));
        }
    }
}
