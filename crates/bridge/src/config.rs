//! `bridge.toml`: the port, extra origins, extra databases, the oschess
//! site the pairing link opens, and the engine the analysis board uses. The bridge reads them when it starts, and the
//! databases again whenever the file changes.
//!
//! The file is a small subset of TOML — `key = value` lines, `#` comments,
//! integers, strings in `"double"` (with escapes) or `'single'` (literal)
//! quotes, and arrays of strings that may span lines — read without a TOML
//! dependency. Anything else is an error naming the line.

use std::path::{Path, PathBuf};

use crate::pairing::DEFAULT_WEB;

/// The file's name in the data folder.
pub const FILE_NAME: &str = "bridge.toml";

pub const DEFAULT_PORT: u16 = 39581;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub port: u16,
    /// Allowed origins in addition to the oschess ones.
    pub origins: Vec<String>,
    /// Databases in addition to those ChessBase lists: database files, or
    /// folders of databases.
    pub databases: Vec<PathBuf>,
    /// The oschess site the pairing link opens.
    pub web: String,
    /// The UCI engine the analysis board uses, and its threads and hash table
    /// in megabytes when not the defaults (`engine.rs`).
    pub engine: Option<PathBuf>,
    pub engine_threads: Option<u32>,
    pub engine_hash: Option<u32>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: DEFAULT_PORT,
            origins: Vec::new(),
            databases: Vec::new(),
            web: DEFAULT_WEB.to_string(),
            engine: None,
            engine_threads: None,
            engine_hash: None,
        }
    }
}

const TEMPLATE: &str = "\
# oschess bridge settings. A new port, origin or web site takes effect when the
# bridge restarts; the databases and the engine take effect as soon as this
# file is saved. A file that cannot be read leaves the settings read before.

port = 39581

# Web origins allowed in addition to oschess.org, for local development,
# for example [\"http://localhost:5173\"].
origins = []

# Databases in addition to those ChessBase shows: paths to .2cbh, .cbh or .pgn
# files, or to folders whose databases are all served,
# for example ['C:\\Users\\me\\Documents\\ChessBase\\MyWork\\Games.2cbh'].
databases = []

# The oschess site the pairing link opens, one of the allowed origins, for
# example \"https://staging.oschess.org\".
# web = \"https://oschess.org\"

# The UCI engine the analysis board uses. oschess's engine panel sets its
# threads and hash table for each analysis; engine_threads and engine_hash (MB)
# are what it uses when oschess names none, by default all processors but two
# and at most 512 MB.
# For example engine = 'C:\\Program Files\\Stockfish\\stockfish.exe'
";

/// Reads `path`, writing the default file first when there is none.
pub fn load_or_create(path: &Path) -> Result<Config, String> {
    load(path).map_err(|e| e.to_string())
}

/// [`load_or_create`], its error kept whole for the log.
pub fn load(path: &Path) -> Result<Config, Error> {
    if !path.exists() {
        // Under the lock of every change: a default file written beside a
        // change would replace it (#62).
        let _one = changing();
        create(path)?;
    }
    read_file(path)?.ok_or_else(|| Error { path: path.to_owned(), why: Why::Missing })
}

/// Why `bridge.toml` cannot be read or made. Its message, which the console
/// and the windows show, names the file's path and quotes the text at fault;
/// [`Error::logged`] does neither (#117).
#[derive(Debug)]
pub struct Error {
    path: PathBuf,
    why: Why,
}

#[derive(Debug)]
enum Why {
    Io(std::io::Error),
    /// Making the folder the file goes in.
    Folder(std::io::Error),
    NotAFile,
    Missing,
    Syntax(Syntax),
}

impl Error {
    /// The error as a log line says it: the file by its name, and a line at
    /// fault by its number and what is wrong with it.
    pub fn logged(&self) -> String {
        match &self.why {
            Why::Io(e) => format!("bridge.toml: {e}"),
            Why::Folder(e) => format!("the folder of bridge.toml: {e}"),
            Why::NotAFile => "bridge.toml: not a regular file".into(),
            Why::Missing => "bridge.toml: no such file".into(),
            Why::Syntax(s) => format!("bridge.toml: line {}: {}", s.line, s.fault.what()),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let path = self.path.display();
        match &self.why {
            Why::Io(e) => write!(f, "{path}: {e}"),
            Why::Folder(e) => write!(f, "{}: {e}", self.path.parent().unwrap_or(&self.path).display()),
            Why::NotAFile => write!(f, "{path}: not a regular file"),
            Why::Missing => write!(f, "{path}: no such file"),
            Why::Syntax(s) => write!(f, "{path}: {s}"),
        }
    }
}

impl From<Error> for String {
    fn from(e: Error) -> String {
        e.to_string()
    }
}

/// The lock every write of `bridge.toml` in this process holds.
fn changing() -> std::sync::MutexGuard<'static, ()> {
    static CHANGING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    crate::sync::lock(&CHANGING)
}

/// Writes the default file at `path` unless there is a file; the caller
/// holds [`changing`].
fn create(path: &Path) -> Result<(), Error> {
    if path.exists() {
        return Ok(());
    }
    let error = |why| Error { path: path.to_owned(), why };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| error(Why::Folder(e)))?;
    }
    crate::files::write_atomic(path, TEMPLATE.as_bytes()).map_err(|e| error(Why::Io(e)))
}

/// The settings of the `bridge.toml` at `path`; `None` when there is no file.
/// The one place the file is read (#70). Only a regular file is read: a pipe
/// would block the reader.
pub fn read(path: &Path) -> Result<Option<Config>, String> {
    read_file(path).map_err(|e| e.to_string())
}

/// [`read`], its error kept whole for the log.
fn read_file(path: &Path) -> Result<Option<Config>, Error> {
    let error = |why| Error { path: path.to_owned(), why };
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Ok(m) if !m.is_file() => return Err(error(Why::NotAFile)),
        _ => {}
    }
    let text = std::fs::read_to_string(path).map_err(|e| error(Why::Io(e)))?;
    parse_text(&text).map(Some).map_err(|e| error(Why::Syntax(e)))
}

/// `bridge.toml` as the running bridge follows it (#70): one for the bridge,
/// which the database list and the engine share, each asking it for the
/// settings in force (#175).
///
/// The file is read again whenever its size or modification time changes,
/// by whichever reader looks first. There is one rule for what a read means:
/// - a missing file is the defaults;
/// - a file that cannot be read or parsed leaves the last good settings in
///   force, and is read again at the next look, as access may return. It
///   is logged once for each revision, however often it is read, so that a
///   broken file changed into another broken one is logged again.
#[derive(Debug)]
pub struct Watched {
    path: PathBuf,
    last: std::sync::Mutex<Last>,
}

#[derive(Debug, Default)]
struct Last {
    /// The file's signature when it was last read without error.
    signature: Option<u64>,
    config: Config,
    /// How many reads have changed the settings; the first that succeeds
    /// counts as one.
    changes: u64,
    /// The signature of the revision whose failed read was logged last;
    /// `None` once a read succeeds.
    failed: Option<u64>,
}

/// What one reader of a [`Watched`] file has seen of its changes: nothing
/// before its first look.
#[derive(Default)]
pub struct Seen(u64);

/// The settings in force, and whether they changed since the reader last
/// looked.
pub struct Look {
    pub config: Config,
    pub changed: bool,
}

impl Watched {
    pub fn new(path: PathBuf) -> Watched {
        Watched { path, last: std::sync::Mutex::default() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The settings in force, after reading the file again if it changed.
    /// `seen` is the reader's own, which the look brings up to date: each
    /// reader learns of a change once, whichever of them read it. The first
    /// read that succeeds counts as a change.
    pub fn look(&self, seen: &mut Seen) -> Look {
        let mut last = crate::sync::lock(&self.last);
        let signature = crate::sources::signature(Some(&self.path));
        if last.signature != Some(signature) {
            match read_file(&self.path) {
                Ok(read) => {
                    let next = read.unwrap_or_default();
                    if last.changes == 0 || next != last.config {
                        (last.config, last.changes) = (next, last.changes + 1);
                    }
                    (last.signature, last.failed) = (Some(signature), None);
                }
                Err(e) => {
                    if last.failed != Some(signature) {
                        crate::log!("{}; keeping the settings read before", e.logged());
                    }
                    (last.signature, last.failed) = (None, Some(signature));
                }
            }
        }
        let changed = seen.0 != last.changes;
        seen.0 = last.changes;
        Look { config: last.config.clone(), changed }
    }
}

/// The file for `config`: the default file's text and comments with its values.
/// The Windows app writes its settings this way; comments of the user's own
/// are not kept.
pub fn render(config: &Config) -> String {
    let list = |items: Vec<String>| {
        if items.is_empty() {
            "[]".to_string()
        } else {
            let lines: String = items.iter().map(|i| format!("    {},\n", quote(i))).collect();
            format!("[\n{lines}]")
        }
    };
    let origins = list(config.origins.clone());
    let databases = list(config.databases.iter().map(|p| p.to_string_lossy().into_owned()).collect());
    let web = if config.web == DEFAULT_WEB {
        "# web = \"https://oschess.org\"".to_string()
    } else {
        format!("web = {}", quote(&config.web))
    };
    let mut text = TEMPLATE
        .replace("port = 39581", &format!("port = {}", config.port))
        .replace("origins = []", &format!("origins = {origins}"))
        .replace("databases = []", &format!("databases = {databases}"))
        .replace("# web = \"https://oschess.org\"", &web);
    if let Some(engine) = &config.engine {
        text.push_str(&format!("engine = {}\n", quote(&engine.to_string_lossy())));
    }
    if let Some(n) = config.engine_threads {
        text.push_str(&format!("engine_threads = {n}\n"));
    }
    if let Some(n) = config.engine_hash {
        text.push_str(&format!("engine_hash = {n}\n"));
    }
    text
}

/// Writes `config` to `path` through a temporary file, so a reader never sees
/// half a file.
pub fn save(path: &Path, config: &Config) -> Result<(), String> {
    crate::files::write_atomic(path, render(config).as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
}

/// Changes the `bridge.toml` at `path`: reads it, applies `change` and saves
/// the result, holding the one lock every change in this process takes, so
/// two changes at once, such as a folder added while an install chooses its
/// engine, both land (#62). The saved settings.
pub fn update(path: &Path, change: impl FnOnce(&Config) -> Config) -> Result<Config, String> {
    let _one = changing();
    create(path)?;
    let now = read_file(path)?.ok_or_else(|| Error { path: path.to_owned(), why: Why::Missing })?;
    let next = change(&now);
    save(path, &next)?;
    Ok(next)
}

/// A string as a value: literal in single quotes when it can be, which keeps
/// Windows paths readable, else in double quotes with escapes.
fn quote(text: &str) -> String {
    if !text.contains('\'') && !text.chars().any(char::is_control) {
        return format!("'{text}'");
    }
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

enum Value {
    Int(i64),
    Str(String),
    List(Vec<String>),
}

pub fn parse(text: &str) -> Result<Config, String> {
    parse_text(text).map_err(|e| e.to_string())
}

/// A line of the file that cannot be read.
#[derive(Debug)]
struct Syntax {
    line: usize,
    fault: Fault,
}

/// What is wrong with a line. The message quotes the text at fault; the log
/// says only what is wrong.
#[derive(Debug)]
enum Fault {
    Fixed(&'static str),
    NotAValue(String),
    UnknownKey(String),
}

impl Fault {
    fn what(&self) -> &'static str {
        match self {
            Fault::Fixed(what) => what,
            Fault::NotAValue(_) => "not a value",
            Fault::UnknownKey(_) => "unknown key",
        }
    }
}

impl std::fmt::Display for Syntax {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: ", self.line)?;
        match &self.fault {
            Fault::Fixed(what) => f.write_str(what),
            Fault::NotAValue(text) => write!(f, "not a value: {text}"),
            Fault::UnknownKey(key) => write!(f, "unknown key {key}"),
        }
    }
}

/// [`parse`], its error kept whole.
fn parse_text(text: &str) -> Result<Config, Syntax> {
    let mut config = Config::default();
    let mut lines = text.lines().enumerate();
    while let Some((i, line)) = lines.next() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        let with = |fault| Syntax { line: i + 1, fault };
        let at = |what| with(Fault::Fixed(what));
        let (key, rest) = line.split_once('=').ok_or_else(|| at("expected key = value"))?;
        let mut value_text = rest.trim().to_string();
        // An array may continue on the following lines up to its closing bracket.
        while value_text.starts_with('[') && !closes(&value_text) {
            let (_, next) = lines.next().ok_or_else(|| at("unterminated array"))?;
            value_text.push(' ');
            value_text.push_str(strip_comment(next).trim());
        }
        let value = parse_value(&value_text).map_err(with)?;
        match (key.trim(), value) {
            ("port", Value::Int(p)) => {
                config.port = u16::try_from(p).ok().filter(|&p| p > 0).ok_or_else(|| at("port out of range"))?
            }
            ("origins", Value::List(v)) => config.origins = v,
            ("databases", Value::List(v)) => config.databases = v.into_iter().map(PathBuf::from).collect(),
            ("web", Value::Str(v)) => config.web = v,
            ("engine", Value::Str(v)) => config.engine = Some(PathBuf::from(v)),
            ("engine_threads", Value::Int(n)) => {
                config.engine_threads =
                    Some(u32::try_from(n).ok().filter(|&n| n > 0).ok_or_else(|| at("engine_threads out of range"))?)
            }
            ("engine_hash", Value::Int(n)) => {
                config.engine_hash =
                    Some(u32::try_from(n).ok().filter(|&n| n > 0).ok_or_else(|| at("engine_hash out of range"))?)
            }
            ("port" | "origins" | "databases" | "web" | "engine" | "engine_threads" | "engine_hash", _) => {
                return Err(at("wrong type"));
            }
            (k, _) => return Err(with(Fault::UnknownKey(k.to_string()))),
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

fn parse_value(text: &str) -> Result<Value, Fault> {
    if let Some(inner) = text.strip_prefix('[') {
        let inner = inner.strip_suffix(']').ok_or(Fault::Fixed("text after the array"))?;
        let mut items = Vec::new();
        let mut rest = inner.trim();
        while !rest.is_empty() {
            let (item, after) = parse_string(rest).map_err(Fault::Fixed)?;
            items.push(item);
            rest = after.trim_start();
            rest = match rest.strip_prefix(',') {
                Some(r) => r.trim_start(),
                None if rest.is_empty() => rest,
                None => return Err(Fault::Fixed("expected , between array items")),
            };
        }
        return Ok(Value::List(items));
    }
    if text.starts_with(['"', '\'']) {
        let (value, after) = parse_string(text).map_err(Fault::Fixed)?;
        return if after.trim().is_empty() {
            Ok(Value::Str(value))
        } else {
            Err(Fault::Fixed("text after the string"))
        };
    }
    text.parse::<i64>().map(Value::Int).map_err(|_| Fault::NotAValue(text.to_string()))
}

/// One quoted string at the start of `text`, and what follows it.
fn parse_string(text: &str) -> Result<(String, &str), &'static str> {
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
                _ => return Err("unknown escape"),
            },
            c => out.push(c),
        }
    }
    Err("unterminated string")
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
        let c = parse("engine = 'C:\\Engines\\sf.exe'\nengine_threads = 4\nengine_hash = 256\n").unwrap();
        assert_eq!(
            (c.engine, c.engine_threads, c.engine_hash),
            (Some(PathBuf::from("C:\\Engines\\sf.exe")), Some(4), Some(256))
        );
        for bad in ["engine = 5", "engine_threads = 0", "engine_hash = -1", "engine_threads = 'x'"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_default_renders_as_the_template() {
        assert_eq!(render(&Config::default()), TEMPLATE);
    }

    #[test]
    fn rendered_settings_read_back_the_same() {
        let configs = [
            Config {
                port: 40000,
                origins: vec!["http://localhost:5173".into(), "http://127.0.0.1:4173".into()],
                databases: vec![
                    PathBuf::from(r"C:\Users\me\Documents\ChessBase\MyWork"),
                    PathBuf::from(r"D:\Шахи\It's # here [1].2cbh"),
                    PathBuf::from("E:\\quote\" and\ttab"),
                ],
                web: "https://staging.oschess.org".into(),
                engine: Some(PathBuf::from(r"C:\Program Files\ChessBase\Engines.x64\Stockfish 17.1\sf.exe")),
                engine_threads: Some(6),
                engine_hash: Some(1024),
            },
            Config { databases: vec![PathBuf::from("/home/me/bases")], ..Config::default() },
            Config { engine: Some(PathBuf::from("/usr/games/stockfish")), ..Config::default() },
        ];
        for config in configs {
            let text = render(&config);
            assert_eq!(parse(&text).unwrap(), config, "{text}");
        }
    }

    #[test]
    fn saving_replaces_the_file_whole() {
        let dir = std::env::temp_dir().join(format!("bridge-config-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("bridge.toml");
        let mut config = load_or_create(&path).unwrap();
        config.databases.push(PathBuf::from(r"C:\Bases"));
        save(&path, &config).unwrap();
        assert_eq!(load_or_create(&path).unwrap(), config);
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["bridge.toml"], "no temporary file stays");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A reader that finds no file writes the default one under the lock of
    /// every change, so a change made at the same moment is never replaced by
    /// the defaults (#62).
    #[test]
    fn a_default_file_never_replaces_a_change() {
        let dir = std::env::temp_dir().join(format!("bridge-config-create-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("bridge.toml");
        for i in 0..300 {
            let _ = std::fs::remove_file(&path);
            let engine = PathBuf::from(format!("/engines/{i}"));
            let both = std::sync::Barrier::new(2);
            std::thread::scope(|s| {
                s.spawn(|| {
                    both.wait();
                    load_or_create(&path).unwrap();
                });
                s.spawn(|| {
                    both.wait();
                    update(&path, |c| Config { engine: Some(engine.clone()), ..c.clone() }).unwrap();
                });
            });
            assert_eq!(load_or_create(&path).unwrap().engine, Some(engine), "round {i}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Folders added while engines are chosen, all at once: every change lands
    /// (#62), as the settings window's folders and an install's engine do.
    #[test]
    fn changes_at_once_all_land() {
        let dir = std::env::temp_dir().join(format!("bridge-config-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("bridge.toml");
        const EACH: usize = 40;
        std::thread::scope(|s| {
            s.spawn(|| {
                for i in 0..EACH {
                    update(&path, |c| {
                        let mut next = c.clone();
                        next.databases.push(PathBuf::from(format!("/bases/{i}")));
                        next
                    })
                    .unwrap();
                }
            });
            s.spawn(|| {
                for i in 0..EACH {
                    update(&path, |c| Config { engine: Some(PathBuf::from(format!("/engines/{i}"))), ..c.clone() })
                        .unwrap();
                }
            });
        });
        let config = load_or_create(&path).unwrap();
        let expected: Vec<PathBuf> = (0..EACH).map(|i| PathBuf::from(format!("/bases/{i}"))).collect();
        assert_eq!(config.databases, expected);
        assert_eq!(config.engine, Some(PathBuf::from(format!("/engines/{}", EACH - 1))));
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["bridge.toml"], "no temporary file stays");
        let _ = std::fs::remove_dir_all(&dir);
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

    /// The log names the file alone and quotes none of it (#117); the message
    /// the console and the windows show is as before.
    #[test]
    fn the_log_names_the_file_alone() {
        let dir = std::env::temp_dir().join(format!("bridge-config-logged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bridge.toml");
        let error = |text: &str| {
            std::fs::write(&path, text).unwrap();
            let e = read_file(&path).unwrap_err();
            assert_eq!(read(&path).unwrap_err(), e.to_string());
            (e.to_string(), e.logged())
        };
        let shown = |message: &str| format!("{}: {message}", path.display());
        assert_eq!(
            error("port = 1\nengine = C:\\Users\\Jane\\sf.exe\n"),
            (shown("line 2: not a value: C:\\Users\\Jane\\sf.exe"), "bridge.toml: line 2: not a value".into())
        );
        assert_eq!(error("Jane = 1"), (shown("line 1: unknown key Jane"), "bridge.toml: line 1: unknown key".into()));
        assert_eq!(
            error("port = 0"),
            (shown("line 1: port out of range"), "bridge.toml: line 1: port out of range".into())
        );
        let folder = dir.join("folder.toml");
        std::fs::create_dir(&folder).unwrap();
        let e = load(&folder).unwrap_err();
        assert_eq!(
            (e.to_string(), e.logged()),
            (format!("{}: not a regular file", folder.display()), "bridge.toml: not a regular file".into())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The readers of one file, as the database list and the engine are,
    /// each learn of a change once, whichever of them read it (#175); a file
    /// that cannot be parsed keeps the settings read before, for all of them.
    #[test]
    fn every_reader_learns_of_a_change_once() {
        // The broken file is logged: not into the file of a test that counts
        // its lines.
        let _log = crate::log::testing::hold();
        let dir = std::env::temp_dir().join(format!("bridge-config-readers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, "port = 40000\n").unwrap();
        let file = Watched::new(path.clone());
        let (mut list, mut engine) = (Seen::default(), Seen::default());
        let look = |seen: &mut Seen| {
            let look = file.look(seen);
            (look.config.port, look.changed)
        };
        assert_eq!(look(&mut list), (40000, true));
        assert_eq!(look(&mut engine), (40000, true), "the first read is a change for each reader");
        assert_eq!(look(&mut list), (40000, false));
        // Each text of another length than the one before, so that its
        // signature changes whatever the clock.
        std::fs::write(&path, "port = 4000\n").unwrap();
        assert_eq!(look(&mut engine), (4000, true));
        assert_eq!(look(&mut engine), (4000, false));
        assert_eq!(look(&mut list), (4000, true), "a change the other reader read is one all the same");
        std::fs::write(&path, "port = \n").unwrap();
        assert_eq!(look(&mut list), (4000, false));
        assert_eq!(look(&mut engine), (4000, false));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
