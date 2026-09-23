//! Comparisons of the comparable fields: numbers, and fixed-width text (ECO
//! codes, dates) in byte order.

use super::query::{Cmp, Value};

/// A comparison of fixed-width text (ECO codes, dates) in byte order. A bound
/// followed by `~` sorts after every text that starts with it, which turns a
/// prefix into the upper end of a range.
pub enum TextCmp {
    Prefix(Vec<u8>),
    AtLeast(Vec<u8>),
    AtMost(Vec<u8>),
    Above(Vec<u8>),
    Below(Vec<u8>),
}

pub enum IntCmp {
    Range(i64, i64),
    Never,
}

pub fn int_cmp(v: &Value) -> IntCmp {
    let parse = |s: &str| s.trim().parse::<i64>().ok();
    let Some(low) = parse(&v.text) else { return IntCmp::Never };
    match v.cmp {
        Cmp::Equal => IntCmp::Range(low, low),
        Cmp::Greater => IntCmp::Range(low.saturating_add(1), i64::MAX),
        Cmp::GreaterOrEqual => IntCmp::Range(low, i64::MAX),
        Cmp::Less => IntCmp::Range(i64::MIN, low.saturating_sub(1)),
        Cmp::LessOrEqual => IntCmp::Range(i64::MIN, low),
        Cmp::Range => match v.upper.as_deref().and_then(parse) {
            Some(high) => IntCmp::Range(low, high),
            None => IntCmp::Never,
        },
    }
}

fn with_tilde(bound: &[u8]) -> Vec<u8> {
    let mut v = bound.to_vec();
    v.push(b'~');
    v
}

/// The comparisons `v` asks for, on `low` and, for a range, `v.upper`.
pub fn text_cmps(low: &[u8], v: &Value) -> Vec<TextCmp> {
    let high = v.upper.as_deref().map_or(low, str::as_bytes);
    match v.cmp {
        Cmp::Equal => vec![TextCmp::Prefix(low.to_vec())],
        Cmp::GreaterOrEqual => vec![TextCmp::AtLeast(low.to_vec())],
        Cmp::Greater => vec![TextCmp::Above(with_tilde(low))],
        Cmp::Less => vec![TextCmp::Below(low.to_vec())],
        Cmp::LessOrEqual => vec![TextCmp::AtMost(with_tilde(low))],
        Cmp::Range => vec![TextCmp::AtLeast(low.to_vec()), TextCmp::AtMost(with_tilde(high))],
    }
}

/// A typed date in the stored `YYYY.MM.DD` prefix form: `2024`, `2024-3` →
/// `2024.03`, `2024/03/15` → `2024.03.15`; `None` when it is not a date.
pub fn normalize_date(text: &str) -> Option<String> {
    let mut parts = text.trim().split(['-', '.', '/']);
    let year = parts.next().filter(|y| y.len() == 4 && y.bytes().all(|b| b.is_ascii_digit()))?;
    let mut out = year.to_string();
    for part in parts.by_ref().take(2) {
        if part.is_empty() || part.len() > 2 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        out.push('.');
        out.push_str(&format!("{part:0>2}"));
    }
    parts.next().is_none().then_some(out)
}

impl TextCmp {
    pub fn holds(&self, s: &[u8]) -> bool {
        match self {
            TextCmp::Prefix(p) => s.len() >= p.len() && s[..p.len()].eq_ignore_ascii_case(p),
            TextCmp::AtLeast(b) => s >= &b[..],
            TextCmp::AtMost(b) => s <= &b[..],
            TextCmp::Above(b) => s > &b[..],
            TextCmp::Below(b) => s < &b[..],
        }
    }
}

impl IntCmp {
    pub fn holds(&self, v: i64) -> bool {
        match *self {
            IntCmp::Range(low, high) => (low..=high).contains(&v),
            IntCmp::Never => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_normalise_as_typed() {
        assert_eq!(normalize_date("2024").as_deref(), Some("2024"));
        assert_eq!(normalize_date("2024-3").as_deref(), Some("2024.03"));
        assert_eq!(normalize_date("2024/03/5").as_deref(), Some("2024.03.05"));
        for bad in ["24", "2024-", "2024-123", "2024-01-02-03", "x", "2024-1a"] {
            assert_eq!(normalize_date(bad), None, "{bad}");
        }
    }

    #[test]
    fn text_comparisons_with_the_tilde_bound() {
        let v = |cmp, upper: Option<&str>| Value { text: "B9".into(), cmp, upper: upper.map(Into::into) };
        let all = |cmps: Vec<TextCmp>, s: &str| cmps.iter().all(|c| c.holds(s.as_bytes()));
        assert!(all(text_cmps(b"B9", &v(Cmp::Equal, None)), "B90"));
        assert!(!all(text_cmps(b"B9", &v(Cmp::Equal, None)), "B80"));
        assert!(all(text_cmps(b"B9", &v(Cmp::LessOrEqual, None)), "B99"));
        assert!(!all(text_cmps(b"B9", &v(Cmp::Greater, None)), "B99"));
        assert!(all(text_cmps(b"B9", &v(Cmp::Greater, None)), "C00"));
        assert!(all(text_cmps(b"B9", &v(Cmp::Range, Some("C1"))), "C19"));
        assert!(!all(text_cmps(b"B9", &v(Cmp::Range, Some("C1"))), "C20"));
    }
}
