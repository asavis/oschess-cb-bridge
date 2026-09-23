//! A small JSON writer: responses are built in order, with no parsing needed.

use std::fmt::Write;

/// A JSON object written field by field.
pub struct Obj {
    out: String,
    empty: bool,
}

impl Default for Obj {
    fn default() -> Self {
        Obj { out: String::from("{"), empty: true }
    }
}

impl Obj {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(&mut self, key: &str) {
        if !self.empty {
            self.out.push(',');
        }
        self.empty = false;
        push_string(&mut self.out, key);
        self.out.push(':');
    }

    pub fn str(mut self, key: &str, value: &str) -> Self {
        self.key(key);
        push_string(&mut self.out, value);
        self
    }

    pub fn num(mut self, key: &str, value: impl Into<i64>) -> Self {
        self.key(key);
        let _ = write!(self.out, "{}", value.into());
        self
    }

    pub fn bool(mut self, key: &str, value: bool) -> Self {
        self.key(key);
        self.out.push_str(if value { "true" } else { "false" });
        self
    }

    /// A value that is already JSON: a nested object or an array.
    pub fn raw(mut self, key: &str, json: &str) -> Self {
        self.key(key);
        self.out.push_str(json);
        self
    }

    pub fn done(mut self) -> String {
        self.out.push('}');
        self.out
    }
}

/// A JSON array of values that are already JSON.
pub fn array<I: IntoIterator<Item = String>>(items: I) -> String {
    let mut out = String::from("[");
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&item);
    }
    out.push(']');
    out
}

/// `value` as a quoted JSON string.
pub fn string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    push_string(&mut out, value);
    out
}

fn push_string(out: &mut String, value: &str) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn objects_arrays_and_escapes() {
        let inner = Obj::new().num("n", -3).bool("b", true).done();
        let json = Obj::new().str("s", "a\"b\\c\n\u{1}").raw("o", &inner).raw("a", &array([string("x")])).done();
        assert_eq!(json, r#"{"s":"a\"b\\c\n\u0001","o":{"n":-3,"b":true},"a":["x"]}"#);
        assert_eq!(Obj::new().done(), "{}");
        assert_eq!(array(Vec::new()), "[]");
    }
}
