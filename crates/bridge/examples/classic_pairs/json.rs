//! A small JSON reader for the bridge's answers, so that they are compared
//! field by field rather than as text.

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// A number as written.
    Num(String),
    Str(String),
    Arr(Vec<Json>),
    /// Members in the order written.
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

/// `text` as JSON; `None` when it is not well formed.
pub fn parse(text: &str) -> Option<Json> {
    let mut p = Parser { b: text.as_bytes(), at: 0 };
    let v = p.value()?;
    p.space();
    (p.at == p.b.len()).then_some(v)
}

struct Parser<'a> {
    b: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn space(&mut self) {
        while self.b.get(self.at).is_some_and(|c| c.is_ascii_whitespace()) {
            self.at += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Option<()> {
        self.space();
        (self.b.get(self.at) == Some(&c)).then(|| self.at += 1)
    }

    fn word(&mut self, w: &str, v: Json) -> Option<Json> {
        self.b[self.at..].starts_with(w.as_bytes()).then(|| {
            self.at += w.len();
            v
        })
    }

    fn value(&mut self) -> Option<Json> {
        self.space();
        match *self.b.get(self.at)? {
            b'{' => {
                self.at += 1;
                let mut members = Vec::new();
                if self.eat(b'}').is_some() {
                    return Some(Json::Obj(members));
                }
                loop {
                    self.space();
                    let key = self.string()?;
                    self.eat(b':')?;
                    members.push((key, self.value()?));
                    if self.eat(b'}').is_some() {
                        return Some(Json::Obj(members));
                    }
                    self.eat(b',')?;
                }
            }
            b'[' => {
                self.at += 1;
                let mut items = Vec::new();
                if self.eat(b']').is_some() {
                    return Some(Json::Arr(items));
                }
                loop {
                    items.push(self.value()?);
                    if self.eat(b']').is_some() {
                        return Some(Json::Arr(items));
                    }
                    self.eat(b',')?;
                }
            }
            b'"' => self.string().map(Json::Str),
            b't' => self.word("true", Json::Bool(true)),
            b'f' => self.word("false", Json::Bool(false)),
            b'n' => self.word("null", Json::Null),
            _ => {
                let start = self.at;
                while self.b.get(self.at).is_some_and(|c| matches!(c, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')) {
                    self.at += 1;
                }
                (self.at > start).then(|| Json::Num(String::from_utf8_lossy(&self.b[start..self.at]).into_owned()))
            }
        }
    }

    fn string(&mut self) -> Option<String> {
        if self.b.get(self.at) != Some(&b'"') {
            return None;
        }
        self.at += 1;
        let mut out: Vec<u16> = Vec::new();
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            let c = *self.b.get(self.at)?;
            self.at += 1;
            match c {
                b'"' if bytes.is_empty() => break,
                b'"' => return None,
                b'\\' => {
                    let e = *self.b.get(self.at)?;
                    self.at += 1;
                    let ch = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hex = std::str::from_utf8(self.b.get(self.at..self.at + 4)?).ok()?;
                            self.at += 4;
                            flush(&mut bytes, &mut out);
                            out.push(u16::from_str_radix(hex, 16).ok()?);
                            continue;
                        }
                        _ => return None,
                    };
                    flush(&mut bytes, &mut out);
                    out.extend(ch.encode_utf16(&mut [0; 2]).iter());
                }
                _ => {
                    bytes.push(c);
                    if std::str::from_utf8(&bytes).is_ok() {
                        flush(&mut bytes, &mut out);
                    }
                }
            }
        }
        String::from_utf16(&out).ok()
    }
}

/// Moves whole UTF-8 characters from `bytes` to `out` as UTF-16.
fn flush(bytes: &mut Vec<u8>, out: &mut Vec<u16>) {
    if let Ok(s) = std::str::from_utf8(bytes) {
        out.extend(s.encode_utf16());
        bytes.clear();
    }
}
