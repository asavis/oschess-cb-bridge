//! The search text as a typed query (`docs/search-grammar.md`).
//!
//! The grammar is lenient: nothing a user types is a syntax error. An unknown
//! qualifier is literal text, an empty value is dropped and a malformed `sort:`
//! is ignored. The one refusal is a qualifier that only the oschess Library has.

/// Characters of the text that are read; the rest is ignored.
pub const MAX_QUERY_CHARS: usize = 1024;
/// Terms that are kept; later ones are ignored.
pub const MAX_TERMS: usize = 32;
/// Characters of one value that are kept.
pub const MAX_VALUE_CHARS: usize = 256;

pub use super::sort::{Sort, SortKey};

/// Qualifiers of the oschess Library that ChessBase databases do not have.
pub const LIBRARY_ONLY: [&str; 6] = ["tag", "created", "updated", "is", "has", "no"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// A bare word or phrase: players, tournament or annotator.
    Text,
    White,
    Black,
    /// Either player.
    Player,
    Event,
    Result,
    Eco,
    Date,
    Round,
    Annotator,
    Moves,
    /// Either player's rating.
    Elo,
}

impl Field {
    fn of(qualifier: &str) -> Option<Field> {
        Some(match qualifier {
            "white" => Field::White,
            "black" => Field::Black,
            "player" => Field::Player,
            "event" | "tournament" => Field::Event,
            "result" => Field::Result,
            "eco" => Field::Eco,
            "date" => Field::Date,
            "round" => Field::Round,
            "annotator" => Field::Annotator,
            "moves" => Field::Moves,
            "elo" => Field::Elo,
            _ => return None,
        })
    }

    /// Fields that take `>`, `>=`, `<`, `<=` and `a..b`.
    pub fn is_comparable(self) -> bool {
        matches!(self, Field::Eco | Field::Date | Field::Moves | Field::Elo)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cmp {
    Equal,
    Greater,
    GreaterOrEqual,
    Less,
    LessOrEqual,
    /// Both bounds inclusive; the upper bound is in `Value::upper`.
    Range,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Value {
    pub text: String,
    pub cmp: Cmp,
    pub upper: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Term {
    pub field: Field,
    /// Alternatives, any of which matches.
    pub values: Vec<Value>,
    pub negated: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    pub terms: Vec<Term>,
    /// The last valid `sort:` token.
    pub sort: Option<Sort>,
}

/// A qualifier the query may not use, as typed (lower case).
#[derive(Debug, PartialEq, Eq)]
pub struct Unsupported(pub String);

pub fn parse(text: &str) -> Result<Query, Unsupported> {
    let text: String = text.chars().take(MAX_QUERY_CHARS).collect();
    let mut query = Query::default();
    for token in tokens(&text) {
        let qualifier = token.qualifier.as_deref().map(str::to_ascii_lowercase);
        if qualifier.as_deref() == Some("sort") {
            if !token.negated
                && let Some(sort) = token.values.first().and_then(|v| Sort::parse(v))
            {
                query.sort = Some(sort);
            }
            continue;
        }
        if let Some(q) = qualifier.as_deref().filter(|q| LIBRARY_ONLY.contains(q)) {
            return Err(Unsupported(q.to_string()));
        }
        if query.terms.len() >= MAX_TERMS {
            continue;
        }
        if let Some(term) = to_term(&token, qualifier.as_deref()) {
            query.terms.push(term);
        }
    }
    Ok(query)
}

fn to_term(token: &Token, qualifier: Option<&str>) -> Option<Term> {
    let text_term = |text: &str| {
        let text = clip(text);
        (!text.is_empty()).then(|| Term { field: Field::Text, values: vec![equal(text)], negated: token.negated })
    };
    let Some(qualifier) = qualifier else {
        let word = token.values.first().map_or("", String::as_str);
        let word = if token.quoted { word } else { word.strip_prefix('#').unwrap_or(word) };
        return text_term(word);
    };
    let Some(field) = Field::of(qualifier) else { return text_term(&token.raw) };
    let values: Vec<Value> = token.values.iter().filter_map(|v| to_value(field, v)).collect();
    (!values.is_empty()).then_some(Term { field, values, negated: token.negated })
}

fn equal(text: String) -> Value {
    Value { text, cmp: Cmp::Equal, upper: None }
}

fn to_value(field: Field, raw: &str) -> Option<Value> {
    let value = clip(raw);
    if value.is_empty() {
        return None;
    }
    if field == Field::Result {
        let canonical = match value.to_lowercase().as_str() {
            "draw" | "½" | "½-½" | "1/2" => "1/2-1/2".to_string(),
            "unknown" => "*".to_string(),
            _ => value,
        };
        return Some(equal(canonical));
    }
    if !field.is_comparable() {
        return Some(equal(value));
    }
    let (cmp, rest, upper) = if let Some(r) = value.strip_prefix(">=") {
        (Cmp::GreaterOrEqual, r.to_string(), None)
    } else if let Some(r) = value.strip_prefix("<=") {
        (Cmp::LessOrEqual, r.to_string(), None)
    } else if let Some(r) = value.strip_prefix('>') {
        (Cmp::Greater, r.to_string(), None)
    } else if let Some(r) = value.strip_prefix('<') {
        (Cmp::Less, r.to_string(), None)
    } else if let Some((low, high)) = value.split_once("..") {
        let bound = |b: &str| Some(b.trim()).filter(|b| !b.is_empty() && *b != "*").map(str::to_string);
        match (bound(low), bound(high)) {
            (None, None) => return None,
            (None, Some(h)) => (Cmp::LessOrEqual, h, None),
            (Some(l), None) => (Cmp::GreaterOrEqual, l, None),
            (Some(l), Some(h)) => (Cmp::Range, l, Some(h)),
        }
    } else {
        (Cmp::Equal, value, None)
    };
    let mut text = rest.trim().to_string();
    let mut upper = upper;
    if text.is_empty() {
        return None;
    }
    if field == Field::Eco {
        text = text.to_uppercase();
        upper = upper.map(|u| u.to_uppercase());
    }
    Some(Value { text, cmp, upper })
}

/// Trimmed, and at most [`MAX_VALUE_CHARS`] characters.
fn clip(value: &str) -> String {
    value.trim().chars().take(MAX_VALUE_CHARS).collect()
}

/// One whitespace-separated piece of the text.
#[derive(Debug)]
struct Token {
    negated: bool,
    /// ASCII letters before a colon, as typed.
    qualifier: Option<String>,
    /// The comma-separated alternatives of a qualifier (commas inside quotes
    /// do not separate), or the one text of a bare word; empty ones dropped.
    values: Vec<String>,
    quoted: bool,
    /// The token as typed, without a leading `-`.
    raw: String,
}

fn tokens(text: &str) -> Vec<Token> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        let negated = chars[i] == '-' && i + 1 < n && !chars[i + 1].is_whitespace();
        if negated {
            i += 1;
        }
        let start = i;
        let mut qualifier = None;
        let mut j = i;
        while j < n && chars[j].is_ascii_alphabetic() {
            j += 1;
        }
        if j > i && j < n && chars[j] == ':' {
            qualifier = Some(chars[i..j].iter().collect::<String>());
            i = j + 1;
        }
        let (mut values, mut current) = (Vec::new(), String::new());
        let (mut in_quotes, mut quoted) = (false, false);
        while i < n {
            let c = chars[i];
            i += 1;
            if in_quotes {
                if c == '"' {
                    in_quotes = false;
                } else {
                    current.push(c);
                }
            } else if c == '"' {
                in_quotes = true;
                quoted = true;
            } else if c.is_whitespace() {
                i -= 1;
                break;
            } else if c == ',' && qualifier.is_some() {
                values.push(std::mem::take(&mut current));
            } else {
                current.push(c);
            }
        }
        values.push(current);
        values.retain(|v: &String| !v.trim().is_empty());
        out.push(Token { negated, qualifier, values, quoted, raw: chars[start..i].iter().collect() });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(text: &str) -> Term {
        let q = parse(text).unwrap();
        assert_eq!(q.terms.len(), 1, "{text}: {q:?}");
        q.terms.into_iter().next().unwrap()
    }

    #[test]
    fn words_qualifiers_and_negation() {
        let t = one("-white:\"de la\",tal");
        assert_eq!((t.field, t.negated), (Field::White, true));
        assert_eq!(t.values.iter().map(|v| v.text.as_str()).collect::<Vec<_>>(), ["de la", "tal"]);
        let t = one("#endgame");
        assert_eq!((t.field, t.values[0].text.as_str()), (Field::Text, "endgame"));
        assert_eq!(one("\"#x\"").values[0].text, "#x");
        // An unknown qualifier is literal text, the qualifier included.
        assert_eq!(one("Lesson3:rooks").values[0].text, "Lesson3:rooks");
        assert_eq!(one("foo:bar,baz").values[0].text, "foo:bar,baz");
        // A minus before a space is a word of its own.
        let q = parse("- x").unwrap();
        assert_eq!(
            q.terms.iter().map(|t| (t.values[0].text.as_str(), t.negated)).collect::<Vec<_>>(),
            [("-", false), ("x", false)]
        );
        assert!(parse("white:,,").unwrap().terms.is_empty());
    }

    #[test]
    fn comparisons_ranges_and_aliases() {
        let v = |text: &str| one(text).values.into_iter().next().unwrap();
        assert_eq!(v("eco:b90..b99"), Value { text: "B90".into(), cmp: Cmp::Range, upper: Some("B99".into()) });
        assert_eq!(v("date:>=2024").cmp, Cmp::GreaterOrEqual);
        assert_eq!(v("moves:>40"), Value { text: "40".into(), cmp: Cmp::Greater, upper: None });
        assert_eq!(v("elo:..2500"), Value { text: "2500".into(), cmp: Cmp::LessOrEqual, upper: None });
        assert_eq!(v("elo:2500..*").cmp, Cmp::GreaterOrEqual);
        assert!(parse("elo:..").unwrap().terms.is_empty());
        // Operators are literal on fields that do not compare.
        assert_eq!(v("white:>x").text, ">x");
        assert_eq!(v("result:Draw").text, "1/2-1/2");
        assert_eq!(v("result:unknown").text, "*");
    }

    #[test]
    fn sort_tokens_and_limits() {
        let q = parse("sort:date sort:bogus -sort:white x").unwrap();
        assert_eq!(q.sort, Some(Sort { key: SortKey::Date, descending: true }));
        assert_eq!(parse("sort:WhiteElo-desc").unwrap().sort, Some(Sort { key: SortKey::WhiteElo, descending: true }));
        assert_eq!(parse("sort:name").unwrap().sort, None);
        let many: String = (0..40).map(|i| format!("w{i} ")).collect();
        assert_eq!(parse(&many).unwrap().terms.len(), MAX_TERMS);
        assert_eq!(one(&"x".repeat(300)).values[0].text.chars().count(), MAX_VALUE_CHARS);
    }

    #[test]
    fn library_only_qualifiers_are_refused() {
        for q in ["tag:x", "-IS:chapter", "no:tag", "created:2026", "updated:>1", "has:eco", "tag:"] {
            assert!(parse(q).is_err(), "{q}");
        }
        assert_eq!(parse("x TAG:y").unwrap_err(), Unsupported("tag".into()));
    }
}
