//! `bridge.toml`: the port, extra origins, extra databases and the oschess
//! site the pairing link opens.
//!
//! The file is a small subset of TOML — `key = value` lines, `#` comments,
//! integers, strings in `"double"` (with escapes) or `'single'` (literal)
//! quotes, and arrays of strings that may span lines — read without a TOML
//! dependency. Anything else is an error naming the line.

use std::path::{Path, PathBuf};

use crate::pairing::DEFAULT_WEB;

pub const DEFAULT_PORT: u16 = 39581;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub port: u16,
    /// Allowed origins in addition to the oschess ones.
    pub origins: Vec<String>,
    /// Databases in addition to those ChessBase lists.
    pub databases: Vec<PathBuf>,
    /// The oschess site the pairing link opens.
    pub web: String,
}

impl Default for Config {
    fn default() -> Self {
        Config { port: DEFAULT_PORT, origins: Vec::new(), databases: Vec::new(), web: DEFAULT_WEB.to_string() }
    }
}

const TEMPLATE: &str = "\
# oschess bridge settings. Restart the bridge after changing this file.

port = 39581

# Web origins allowed in addition to oschess.org, for local development,
# for example [\"http://localhost:5173\"].
origins = []

# Databases in addition to those ChessBase shows, as paths to .2cbh files,
# for example ['C:\\Users\\me\\Documents\\ChessBase\\MyWork\\Games.2cbh'].
databases = []

# The oschess site the pairing link opens, one of the allowed origins, for
# example \"https://staging.oschess.org\".
# web = \"https://oschess.org\"
";

/// Reads `path`, writing the default file first when there is none.
pub fn load_or_create(path: &Path) -> Result<Config, String> {
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(path, TEMPLATE).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

enum Value {
    Int(i64),
    Str(String),
    List(Vec<String>),
}

pub fn parse(text: &str) -> Result<Config, String> {
    let mut config = Config::default();
    let mut lines = text.lines().enumerate();
    while let Some((i, line)) = lines.next() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        let at = |msg: &str| format!("line {}: {msg}", i + 1);
        let (key, rest) = line.split_once('=').ok_or_else(|| at("expected key = value"))?;
        let mut value_text = rest.trim().to_string();
        // An array may continue on the following lines up to its closing bracket.
        while value_text.starts_with('[') && !closes(&value_text) {
            let (_, next) = lines.next().ok_or_else(|| at("unterminated array"))?;
            value_text.push(' ');
            value_text.push_str(strip_comment(next).trim());
        }
        let value = parse_value(&value_text).map_err(|e| at(&e))?;
        match (key.trim(), value) {
            ("port", Value::Int(p)) => {
                config.port = u16::try_from(p).ok().filter(|&p| p > 0).ok_or_else(|| at("port out of range"))?
            }
            ("origins", Value::List(v)) => config.origins = v,
            ("databases", Value::List(v)) => config.databases = v.into_iter().map(PathBuf::from).collect(),
            ("web", Value::Str(v)) => config.web = v,
            ("port" | "origins" | "databases" | "web", _) => return Err(at("wrong type")),
            (k, _) => return Err(at(&format!("unknown key {k}"))),
        }
    }
    Ok(config)
}

/// The line without a `#` comment, ignoring `#` inside strings.
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (Some('"'), '\\') if !escaped => {
                escaped = true;
                continue;
            }
            (Some(q), c) if c == q && !escaped => quote = None,
            (None, '"' | '\'') => quote = Some(c),
            (None, '#') => return &line[..i],
            _ => {}
        }
        escaped = false;
    }
    line
}

/// Whether an array's text has its closing bracket outside strings.
fn closes(text: &str) -> bool {
    let mut chars = text.chars();
    let mut quote = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some('"'), '\\') => {
                chars.next();
            }
            (Some(q), c) if c == q => quote = None,
            (None, '"' | '\'') => quote = Some(c),
            (None, ']') => return true,
            _ => {}
        }
    }
    false
}

fn parse_value(text: &str) -> Result<Value, String> {
    if let Some(inner) = text.strip_prefix('[') {
        let inner = inner.strip_suffix(']').ok_or("text after the array")?;
        let mut items = Vec::new();
        let mut rest = inner.trim();
        while !rest.is_empty() {
            let (item, after) = parse_string(rest)?;
            items.push(item);
            rest = after.trim_start();
            rest = match rest.strip_prefix(',') {
                Some(r) => r.trim_start(),
                None if rest.is_empty() => rest,
                None => return Err("expected , between array items".into()),
            };
        }
        return Ok(Value::List(items));
    }
    if text.starts_with(['"', '\'']) {
        let (value, after) = parse_string(text)?;
        return if after.trim().is_empty() { Ok(Value::Str(value)) } else { Err("text after the string".into()) };
    }
    text.parse::<i64>().map(Value::Int).map_err(|_| format!("not a value: {text}"))
}

/// One quoted string at the start of `text`, and what follows it.
fn parse_string(text: &str) -> Result<(String, &str), String> {
    if let Some(body) = text.strip_prefix('\'') {
        let end = body.find('\'').ok_or("unterminated string")?;
        return Ok((body[..end].to_string(), &body[end + 1..]));
    }
    let body = text.strip_prefix('"').ok_or("expected a string")?;
    let mut out = String::new();
    let mut chars = body.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Ok((out, &body[i + 1..])),
            '\\' => match chars.next().map(|(_, e)| e) {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('u') => {
                    let hex: String = (0..4).filter_map(|_| chars.next().map(|(_, h)| h)).collect();
                    let code = u32::from_str_radix(&hex, 16).map_err(|_| "bad \\u escape")?;
                    out.push(char::from_u32(code).ok_or("bad \\u escape")?);
                }
                _ => return Err("unknown escape".into()),
            },
            c => out.push(c),
        }
    }
    Err("unterminated string".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_to_the_defaults() {
        assert_eq!(parse(TEMPLATE).unwrap(), Config::default());
    }

    #[test]
    fn values_and_multi_line_arrays() {
        let c = parse(
            "port = 40000 # comment\norigins = [\"http://localhost:5173\", # dev\n  'http://127.0.0.1:4173',\n]\n\
             databases = ['C:\\Bases\\A # b.2cbh', \"D:\\\\x\\\\\\u0415.2cbh\"]\n",
        )
        .unwrap();
        assert_eq!(c.port, 40000);
        assert_eq!(c.origins, ["http://localhost:5173", "http://127.0.0.1:4173"]);
        assert_eq!(c.databases, [PathBuf::from("C:\\Bases\\A # b.2cbh"), PathBuf::from("D:\\x\\\u{415}.2cbh")]);
        assert_eq!(c.web, DEFAULT_WEB);
        assert_eq!(parse("web = \"https://staging.oschess.org\"").unwrap().web, "https://staging.oschess.org");
    }

    #[test]
    fn errors_name_the_line() {
        for (text, line) in [
            ("port = 0", 1),
            ("port = 70000", 1),
            ("\nport = \"x\"", 2),
            ("origins = [\"a\" \"b\"]", 1),
            ("origins = [\"a\"", 1),
            ("colour = 1", 1),
            ("port", 1),
            ("databases = ['x'] y", 1),
            ("web = ['x']", 1),
            ("origins = 'x'", 1),
        ] {
            let e = parse(text).unwrap_err();
            assert!(e.starts_with(&format!("line {line}:")), "{text:?}: {e}");
        }
    }
}
