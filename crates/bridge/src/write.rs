//! Writes into PGN databases (`docs/api.md`, "Writing games"): a game
//! appended at the end of the file, or one replaced or removed. ChessBase's
//! own formats are never written.
//!
//! An append writes after the file's last byte and leaves every earlier byte
//! as it was; when it fails part-way, the file is cut back to its old length.
//! A replace or a removal writes the whole new file beside the old one, as
//! `<name>.pgn.oschess-tmp`, flushes it, and renames it over the old one,
//! which a file system does at once: a crash leaves the old file or the new
//! one. (Windows' `ReplaceFileW` is not used: some of its failures leave
//! neither file at the name.) A temporary file left by a crash is removed
//! when the bridge next lists the file.
//!
//! Writes into one file run one at a time. Each names the generation its
//! client read, and nothing is written once the file changed since then. On
//! Windows the file is held while it is read and written so that no other
//! program writes it meanwhile, and a file another program holds is
//! reported, never waited for.
//!
//! The text is written as the file is written: in UTF-8 when all of the file
//! is UTF-8, which pure ASCII is, else in the computer's code page, as the
//! reader decodes such a file; with the file's line ends. The header index of
//! the new file is made from the old one (`cbformat::pgnfile::edit`), so a
//! write of the bridge's own never puts the database into `opening`.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use cbformat::codepage::CodePage;
use cbformat::pgnfile::edit::{self, Edit};
use cbformat::pgnfile::lex::Lexer;
use cbformat::pgnfile::line::{LineEnd, main_line};
use cbformat::pgnfile::{self, Record};
use cbformat::view::Base;

use crate::catalog::{Entry, Format, State};
use crate::http::{Request, Response};
use crate::json::Obj;
use crate::reply::{bad_parameter, error, error_with, not_found, unavailable};

/// What a temporary file beside a PGN file adds to its name.
const TEMP_SUFFIX: &str = ".oschess-tmp";
/// Bytes of a file read or copied at a time.
const CHUNK: usize = 1 << 20;

/// `POST /v1/databases/{id}/games`: appends the game of the body.
pub fn append(entry: &Entry, req: &Request) -> Response {
    write(entry, req, Op::Append)
}

/// `PUT /v1/databases/{id}/games/{number}`: replaces game `number` with the
/// game of the body.
pub fn replace(entry: &Entry, number: &str, req: &Request) -> Response {
    match game_number(number) {
        Some(n) => write(entry, req, Op::Replace(n)),
        None => not_found(),
    }
}

/// `DELETE /v1/databases/{id}/games/{number}`: removes game `number`.
pub fn delete(entry: &Entry, number: &str, req: &Request) -> Response {
    match game_number(number) {
        Some(n) => write(entry, req, Op::Delete(n)),
        None => not_found(),
    }
}

fn game_number(number: &str) -> Option<u32> {
    number.parse::<u32>().ok().filter(|&n| n > 0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Append,
    Replace(u32),
    Delete(u32),
}

/// Why a write wrote nothing.
#[derive(Debug)]
enum Failure {
    /// The file changed since the generation the client read.
    Changed,
    /// Windows refused to open, write or replace the file: another program
    /// holds it.
    Busy,
    /// The file's code page has no byte for this character of the game.
    Unencodable(char),
    /// The file system refused the write otherwise, as a full disk does.
    Io(io::Error),
    /// The file was written, then was gone before its generation was read.
    Lost,
}

impl From<io::Error> for Failure {
    fn from(e: io::Error) -> Failure {
        if held_elsewhere(&e) { Failure::Busy } else { Failure::Io(e) }
    }
}

/// Whether Windows refused a file operation because another program holds
/// the file: a sharing or lock violation, access denied to a file in use, or
/// a file mapped into another program's memory.
#[cfg(windows)]
fn held_elsewhere(e: &io::Error) -> bool {
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION, ERROR_USER_MAPPED_FILE,
    };
    let held = [ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION, ERROR_USER_MAPPED_FILE];
    e.raw_os_error().is_some_and(|code| held.contains(&(code as u32)))
}

/// Elsewhere no program holds a file against another.
#[cfg(not(windows))]
fn held_elsewhere(_: &io::Error) -> bool {
    false
}

/// What a write did.
struct Written {
    generation: u64,
    /// Games in the file after it, when its header index was made.
    games: Option<u32>,
}

fn write(entry: &Entry, req: &Request, op: Op) -> Response {
    if matches!(entry.format, Format::TwoCbh | Format::Cbh) {
        return read_only();
    }
    let Some(expected) = req.header("if-match") else {
        return error(428, "precondition_required", "A write names the generation it read in If-Match");
    };
    let text = match op {
        Op::Append | Op::Replace(_) => match game_text(&req.body) {
            Ok(text) => Some(text),
            Err(why) => return bad_parameter("body", why),
        },
        Op::Delete(_) => None,
    };
    let mut writing = entry.writing();
    let open = match entry.open() {
        Ok(open) => open,
        Err(state) => return unavailable(state),
    };
    if !names_generation(expected, open.generation) {
        return generation_changed();
    }
    let Base::Pgn(db) = &*open.db else { return read_only() };
    if !entry.writable_file() {
        return read_only();
    }
    let count = db.record_count();
    if let Op::Replace(n) | Op::Delete(n) = op
        && n > count
    {
        return not_found();
    }
    let Some(index) = entry.pgn().index_path(&entry.id) else {
        return error(500, "internal", "The bridge has no data folder for the header index");
    };
    let done = entry.pgn().while_idle(&entry.id, || {
        let cached = (*writing).filter(|(g, _)| *g == open.generation).map(|(_, layout)| layout);
        let edited = edit_file(entry, db, open.generation, cached, op, text.as_deref(), &index);
        edited.map(|(written, layout)| {
            *writing = Some((written.generation, layout));
            written
        })
    });
    let written = match done {
        None => return unavailable(State::Opening),
        Some(Err(failure)) => return refused(entry, failure),
        Some(Ok(written)) => written,
    };
    drop(writing);
    let (generation, page, path) = (written.generation, db.code_page(), entry.path.clone());
    if written.games.is_some() {
        entry.install(generation, || pgnfile::Database::open(&path, &index, generation, page).ok().map(Base::Pgn));
    }
    let tag = format!("{generation:016x}");
    let body = Obj::new().str("generation", &tag);
    let (status, body) = match op {
        Op::Append => (201, body.num("number", written.games.unwrap_or(count + 1))),
        Op::Replace(n) => (200, body.num("number", n)),
        Op::Delete(_) => (200, body),
    };
    Response::json(status, body.done()).header("ETag", format!("\"{tag}\""))
}

/// Whether an `If-Match` header names `generation`, as an entity tag or bare.
fn names_generation(header: &str, generation: u64) -> bool {
    let tag = header.trim();
    let tag = tag.strip_prefix('"').and_then(|t| t.strip_suffix('"')).unwrap_or(tag);
    tag == format!("{generation:016x}")
}

fn read_only() -> Response {
    error(409, "read_only", "The database is not a PGN file that takes writes")
}

fn generation_changed() -> Response {
    error(409, "generation_changed", "The file changed since the generation the write names; read it again")
}

fn refused(entry: &Entry, failure: Failure) -> Response {
    match failure {
        Failure::Changed => generation_changed(),
        Failure::Busy => error(409, "file_busy", "Another program holds the file; close it there and write again"),
        Failure::Unencodable(c) => {
            error_with(422, "unencodable", "The file's code page cannot hold a character of the game", |o| {
                o.str("character", &c.to_string())
            })
        }
        Failure::Io(e) => {
            crate::log!("writing the PGN file of database {} failed: {e}", entry.id);
            error(500, "write_failed", "The file system refused the write; the file is as it was")
        }
        Failure::Lost => {
            crate::log!("the PGN file of database {} was gone right after a write", entry.id);
            error(500, "internal", "The file was written, then was gone")
        }
    }
}

/// The text of the one game `body` holds, as the reader finds it in a file:
/// from its first tag (or move) to its end. Refused when the body is not
/// UTF-8, holds no game or more than one, or when the game's main line does
/// not play: a move that names no legal move, or a `FEN` tag that names no
/// position. A null move ends the main line, as it does in the reader.
fn game_text(body: &[u8]) -> Result<String, &'static str> {
    let text = std::str::from_utf8(body).map_err(|_| "The body is not UTF-8")?;
    let game = match &edit::games(text.as_bytes())[..] {
        [] => return Err("The body holds no game"),
        [game] => game.clone(),
        _ => return Err("The body holds more than one game"),
    };
    let span = text.get(game.start as usize..game.end as usize).ok_or("The game does not end at a character")?;
    match main_line(span.as_bytes(), &mut Lexer::new(), &mut |_, _| true) {
        LineEnd::End | LineEnd::NullMove => Ok(span.to_string()),
        LineEnd::BadStart => Err("The game's FEN names no position"),
        LineEnd::Stopped | LineEnd::Unplayable(_) => Err("The game's main line does not play"),
    }
}

/// Does `op` to the PGN file of `entry`, which `db` read at `generation`, and
/// makes the header index of the new file at `index`, holding the database's
/// build: what it wrote, and the layout of the file after it. `layout` is the
/// file's at `generation`, when a write found it. `text` is the game for an
/// append or a replace.
fn edit_file(
    entry: &Entry,
    db: &pgnfile::Database,
    generation: u64,
    layout: Option<Layout>,
    op: Op,
    text: Option<&str>,
    index: &Path,
) -> Result<(Written, Layout), Failure> {
    let path = &entry.path;
    let (edit, layout) = {
        let mut file = open_held(path, op == Op::Append)?;
        // The file as the client read it; held, no other program changes it.
        if entry.generation() != Some(generation) {
            return Err(Failure::Changed);
        }
        let layout = match layout {
            Some(layout) => layout,
            None => Layout::of(&mut file)?,
        };
        let game = match text {
            Some(text) => layout.encode(text, db.code_page()).map_err(Failure::Unencodable)?,
            None => Vec::new(),
        };
        let len = file.metadata()?.len();
        match op {
            Op::Append => {
                let bytes = layout.appended(&game);
                append_at_end(&file, len, &bytes, &mut &file)?;
                let edit = Edit { first: db.record_count() + 1, removed: 0, delta: bytes.len() as i64 };
                (edit, layout.after_append())
            }
            Op::Replace(n) => {
                let record = db.record(n).map_err(io_error)?;
                let cut = record.offset()..record.offset().saturating_add(u64::from(record.len()));
                let layout = rewrite(entry, generation, file, len, cut.clone(), &game)?;
                (Edit { first: n, removed: 1, delta: game.len() as i64 - span(&cut) }, layout)
            }
            Op::Delete(n) => {
                let record = db.record(n).map_err(io_error)?;
                // The game and the blank lines after it, to the next game.
                let end = if n < db.record_count() {
                    db.record(n + 1).map(|r: Record| r.offset()).map_err(io_error)?
                } else {
                    len
                };
                let cut = record.offset()..end.max(record.offset());
                let layout = rewrite(entry, generation, file, len, cut.clone(), &[])?;
                (Edit { first: n, removed: 1, delta: -span(&cut) }, layout)
            }
        }
    };
    let generation = entry.generation().ok_or(Failure::Lost)?;
    let games = match edit::update(db, path, index, generation, &edit) {
        Ok(games) => Some(games),
        Err(e) => {
            // The file is written; the next request reads it whole instead.
            crate::log!("the header index of database {} after a write: {}", entry.id, crate::log::error(&e));
            None
        }
    };
    Ok((Written { generation, games }, layout))
}

fn span(range: &std::ops::Range<u64>) -> i64 {
    (range.end - range.start) as i64
}

fn io_error(e: cbformat::Error) -> Failure {
    match e {
        cbformat::Error::Io(_, e) => Failure::from(e),
        e => Failure::Io(io::Error::other(crate::log::error(&e))),
    }
}

/// The PGN file at `path`, opened to read it, and to write it when `write`.
/// On Windows, no other program may write it while it is open, and a file
/// another program holds against that is refused at once.
fn open_held(path: &Path, write: bool) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(write);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ};
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE);
    }
    options.open(path)
}

/// Writes `bytes` through `out` at the end of `file`, `len` bytes long, and
/// flushes it to the disk. When that fails part-way, the file is cut back to
/// `len`.
fn append_at_end(file: &File, len: u64, bytes: &[u8], out: &mut impl Write) -> io::Result<()> {
    let mut at = file;
    let written = at
        .seek(SeekFrom::Start(len))
        .and_then(|_| out.write_all(bytes))
        .and_then(|()| out.flush())
        .and_then(|()| file.sync_data());
    if let Err(e) = written {
        let _ = file.set_len(len).and_then(|()| file.sync_data());
        return Err(e);
    }
    Ok(())
}

/// The temporary file beside the PGN file `path` that a replace or a removal
/// writes the new file to.
fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(TEMP_SUFFIX);
    PathBuf::from(name)
}

/// Removes the temporary file a replace or a removal left beside the PGN file
/// `path` when the bridge stopped before it was renamed.
pub fn remove_leftover(path: &Path) {
    let temp = temp_path(path);
    if std::fs::symlink_metadata(&temp).is_ok_and(|m| m.is_file()) {
        match std::fs::remove_file(&temp) {
            Ok(()) => crate::log!("removed {} left by a write that did not end", temp.display()),
            Err(e) => crate::log!("{} left by a write that did not end cannot be removed: {e}", temp.display()),
        }
    }
}

/// Writes the PGN file at `path`, which `file` holds, `len` bytes long,
/// with `insert` in the place of the bytes `cut`, beside it, then renames
/// the new file over it: the layout of the new file. The file is let go
/// before the rename, and its generation looked at again.
fn rewrite(
    entry: &Entry,
    generation: u64,
    file: File,
    len: u64,
    cut: std::ops::Range<u64>,
    insert: &[u8],
) -> Result<Layout, Failure> {
    let temp = temp_path(&entry.path);
    let written = write_temp(&file, len, &cut, insert, &temp);
    drop(file);
    let swapped = written.map_err(Failure::from).and_then(|layout| {
        if entry.generation() != Some(generation) {
            return Err(Failure::Changed);
        }
        std::fs::rename(&temp, &entry.path)?;
        sync_folder(&entry.path);
        Ok(layout)
    });
    if swapped.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    swapped
}

/// Writes to `temp` the bytes of `file`, `len` long, with `insert` in the
/// place of `cut`, and flushes them to the disk: their layout.
fn write_temp(file: &File, len: u64, cut: &std::ops::Range<u64>, insert: &[u8], temp: &Path) -> io::Result<Layout> {
    let mut out = BufWriter::with_capacity(CHUNK, File::create(temp)?);
    let mut scan = Scan::default();
    let mut source = file;
    let mut buf = vec![0u8; CHUNK];
    let mut copy = |from: u64, to: u64, out: &mut BufWriter<File>, scan: &mut Scan| -> io::Result<()> {
        source.seek(SeekFrom::Start(from))?;
        let mut left = to.saturating_sub(from);
        while left > 0 {
            let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
            let n = source.read(&mut buf[..want])?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            out.write_all(&buf[..n])?;
            scan.feed(&buf[..n]);
            left -= n as u64;
        }
        Ok(())
    };
    copy(0, cut.start, &mut out, &mut scan)?;
    out.write_all(insert)?;
    scan.feed(insert);
    copy(cut.end, len, &mut out, &mut scan)?;
    out.into_inner().map_err(io::IntoInnerError::into_error)?.sync_all()?;
    Ok(scan.finish())
}

/// Flushes the folder of `path` after a rename in it, where the system can,
/// so that the rename outlasts a crash. Windows flushes a rename itself.
fn sync_folder(path: &Path) {
    #[cfg(unix)]
    if let Some(folder) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let _ = File::open(folder).and_then(|f| f.sync_all());
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// A line end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eol {
    CrLf,
    Lf,
    Cr,
}

impl Eol {
    fn text(self) -> &'static str {
        match self {
            Eol::CrLf => "\r\n",
            Eol::Lf => "\n",
            Eol::Cr => "\r",
        }
    }
}

/// How a PGN file ends, after its byte-order mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// Nothing: an empty file, or a byte-order mark alone.
    Empty,
    /// A line without its line end.
    Open,
    /// A line and its line end.
    Line,
    /// An empty line: two line ends, or a line end alone.
    Blank,
}

/// How a PGN file is written, as far as a write follows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// Whether all of it is UTF-8, which pure ASCII is: text is then written
    /// in UTF-8, else in the computer's code page.
    pub utf8: bool,
    /// Its first line end, else CRLF, as ChessBase writes on Windows.
    pub eol: Eol,
    pub end: End,
}

impl Layout {
    /// The layout of `file`, read whole.
    fn of(file: &mut File) -> io::Result<Layout> {
        file.seek(SeekFrom::Start(0))?;
        let mut scan = Scan::default();
        let mut buf = vec![0u8; CHUNK];
        loop {
            match file.read(&mut buf) {
                Ok(0) => return Ok(scan.finish()),
                Ok(n) => scan.feed(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// `text` as the file writes it: its line ends the file's, in UTF-8 or
    /// in `page`. The first character `page` cannot hold is the error.
    fn encode(&self, text: &str, page: CodePage) -> Result<Vec<u8>, char> {
        let mut lines = String::with_capacity(text.len() + text.len() / 32);
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\r' | '\n' => {
                    if c == '\r' && chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    lines.push_str(self.eol.text());
                }
                c => lines.push(c),
            }
        }
        if self.utf8 { Ok(lines.into_bytes()) } else { page.encode(&lines) }
    }

    /// What an append of `game` writes after the file's last byte: an empty
    /// line between the file's last game and the new one, the line before it
    /// ended first, then the game and an empty line, as PGN's export format
    /// ends each game.
    fn appended(&self, game: &[u8]) -> Vec<u8> {
        let eol = self.eol.text().as_bytes();
        let lead = match self.end {
            End::Empty | End::Blank => 0,
            End::Line => 1,
            End::Open => 2,
        };
        let mut bytes = Vec::with_capacity(game.len() + eol.len() * (lead + 2));
        for _ in 0..lead {
            bytes.extend_from_slice(eol);
        }
        bytes.extend_from_slice(game);
        bytes.extend_from_slice(eol);
        bytes.extend_from_slice(eol);
        bytes
    }

    /// The layout after an append: the text was written in the file's
    /// encoding and line ends, and ends with an empty line.
    fn after_append(self) -> Layout {
        Layout { end: End::Blank, ..self }
    }
}

const BOM: &[u8] = b"\xef\xbb\xbf";

/// A file's layout, found as its bytes are fed in order.
#[derive(Default)]
struct Scan {
    len: u64,
    /// Its first three bytes, which may be a byte-order mark.
    head: Vec<u8>,
    /// A UTF-8 sequence the bytes so far end inside.
    carry: Vec<u8>,
    not_utf8: bool,
    eol: Option<Eol>,
    /// The bytes so far end with the file's first line end, a CR, which the
    /// next byte may make a CRLF.
    cr_last: bool,
    /// Its last four bytes, the oldest first.
    tail: [u8; 4],
}

impl Scan {
    fn feed(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.len += bytes.len() as u64;
        let room = 3usize.saturating_sub(self.head.len());
        self.head.extend_from_slice(&bytes[..room.min(bytes.len())]);
        self.check_utf8(bytes);
        if self.cr_last {
            self.eol = Some(if bytes[0] == b'\n' { Eol::CrLf } else { Eol::Cr });
            self.cr_last = false;
        }
        if self.eol.is_none()
            && let Some(at) = bytes.iter().position(|&b| b == b'\r' || b == b'\n')
        {
            self.eol = match (bytes[at], bytes.get(at + 1)) {
                (b'\n', _) => Some(Eol::Lf),
                (_, Some(b'\n')) => Some(Eol::CrLf),
                (_, Some(_)) => Some(Eol::Cr),
                (_, None) => {
                    self.cr_last = true;
                    None
                }
            };
        }
        for &b in &bytes[bytes.len().saturating_sub(4)..] {
            self.tail.rotate_left(1);
            self.tail[3] = b;
        }
    }

    fn check_utf8(&mut self, bytes: &[u8]) {
        if self.not_utf8 {
            return;
        }
        let joined;
        let text = if self.carry.is_empty() {
            bytes
        } else {
            joined = [std::mem::take(&mut self.carry).as_slice(), bytes].concat();
            &joined[..]
        };
        if let Err(e) = std::str::from_utf8(text) {
            match e.error_len() {
                None => self.carry = text[e.valid_up_to()..].to_vec(),
                Some(_) => self.not_utf8 = true,
            }
        }
    }

    fn finish(self) -> Layout {
        let utf8 = !self.not_utf8 && self.carry.is_empty();
        let eol = match (self.eol, self.cr_last) {
            (Some(eol), _) => eol,
            (None, true) => Eol::Cr,
            (None, false) => Eol::CrLf,
        };
        let bom = if self.head == BOM { BOM.len() as u64 } else { 0 };
        let content = self.len - bom;
        let tail = &self.tail[4 - content.min(4) as usize..];
        let is_eol = |b: &u8| *b == b'\r' || *b == b'\n';
        let end = match tail.last() {
            None => End::Empty,
            Some(b) if !is_eol(b) => End::Open,
            Some(_) => {
                let ended = if tail.ends_with(b"\r\n") { 2 } else { 1 };
                // A line end alone is an empty line.
                match tail[..tail.len() - ended].last() {
                    Some(b) if !is_eol(b) => End::Line,
                    _ => End::Blank,
                }
            }
        };
        Layout { utf8, eol, end }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(bytes: &[u8]) -> Layout {
        // Fed a byte at a time, as chunk boundaries may fall anywhere, and
        // whole.
        let mut one = Scan::default();
        bytes.iter().for_each(|b| one.feed(std::slice::from_ref(b)));
        let mut whole = Scan::default();
        whole.feed(bytes);
        let (one, whole) = (one.finish(), whole.finish());
        assert_eq!(one, whole, "{bytes:?}");
        one
    }

    #[test]
    fn the_layout_of_a_file() {
        let l = |utf8, eol, end| Layout { utf8, eol, end };
        assert_eq!(layout(b""), l(true, Eol::CrLf, End::Empty));
        assert_eq!(layout(BOM), l(true, Eol::CrLf, End::Empty));
        assert_eq!(layout(b"[Event \"a\"]"), l(true, Eol::CrLf, End::Open));
        assert_eq!(layout(b"a\nb\r\n"), l(true, Eol::Lf, End::Line));
        assert_eq!(layout(b"a\r\nb\n\n"), l(true, Eol::CrLf, End::Blank));
        assert_eq!(layout(b"a\rb\r\r"), l(true, Eol::Cr, End::Blank));
        assert_eq!(layout(b"a\r"), l(true, Eol::Cr, End::Line));
        assert_eq!(layout(b"\n"), l(true, Eol::Lf, End::Blank));
        assert_eq!(layout(b"\r\n"), l(true, Eol::CrLf, End::Blank));
        assert_eq!(layout(b"ab\r\n"), l(true, Eol::CrLf, End::Line));
        assert_eq!(layout("\u{feff}Ж\r\n\r\n".as_bytes()), l(true, Eol::CrLf, End::Blank));
        assert_eq!(layout(b"\xc5ngstr\xf6m\n"), l(false, Eol::Lf, End::Line));
        // A sequence cut off at the end is not UTF-8.
        assert!(!layout(b"ab\xd0").utf8);
    }

    #[test]
    fn text_is_written_as_the_file_is() {
        let utf8 = Layout { utf8: true, eol: Eol::CrLf, end: End::Line };
        assert_eq!(utf8.encode("[White \"Таль\"]\n\n1. e4\r\n*", CodePage::WESTERN).unwrap(), {
            "[White \"Таль\"]\r\n\r\n1. e4\r\n*".as_bytes().to_vec()
        });
        let ansi = Layout { utf8: false, eol: Eol::Lf, end: End::Line };
        assert_eq!(ansi.encode("[White \"Ståhlberg\"]\r\r1. e4", CodePage::WESTERN).unwrap(), {
            b"[White \"St\xe5hlberg\"]\n\n1. e4".to_vec()
        });
        assert_eq!(ansi.encode("[White \"Таль\"]", CodePage::WESTERN), Err('Т'));
        let appended = |end| Layout { utf8: true, eol: Eol::Lf, end }.appended(b"G");
        assert_eq!(appended(End::Empty), b"G\n\n");
        assert_eq!(appended(End::Blank), b"G\n\n");
        assert_eq!(appended(End::Line), b"\nG\n\n");
        assert_eq!(appended(End::Open), b"\n\nG\n\n");
    }

    #[test]
    fn the_game_of_a_body() {
        assert_eq!(
            game_text(b"\n[Event \"a\"]\n\n1. e4 e5 1-0 {end}\n\n").as_deref(),
            Ok("[Event \"a\"]\n\n1. e4 e5 1-0 {end}")
        );
        assert_eq!(game_text("\u{feff}1. d4 *".as_bytes()).as_deref(), Ok("1. d4 *"));
        assert_eq!(game_text(b"[Event \"a\"] 1. e4 -- 2. Qxf7 *").as_deref(), Ok("[Event \"a\"] 1. e4 -- 2. Qxf7 *"));
        assert_eq!(game_text(b"").unwrap_err(), "The body holds no game");
        assert_eq!(game_text(b"  {a comment}\n").unwrap_err(), "The body holds no game");
        assert_eq!(game_text(b"1. e4 1-0\n1. d4 0-1").unwrap_err(), "The body holds more than one game");
        assert_eq!(game_text(b"[Event \"a\"]\n[Event \"b\"]").unwrap_err(), "The body holds more than one game");
        assert_eq!(game_text(b"1. e4 e5 2. Ke3 *").unwrap_err(), "The game's main line does not play");
        assert_eq!(game_text(b"[FEN \"8/8/8 w - - 0 1\"]\n1. e4 *").unwrap_err(), "The game's FEN names no position");
        assert_eq!(game_text(b"[Event \"\xff\"] *").unwrap_err(), "The body is not UTF-8");
    }

    /// An append that fails part-way leaves the file at its old length.
    #[test]
    fn a_failed_append_is_cut_back() {
        struct Failing<'a> {
            file: &'a File,
            left: usize,
        }
        impl Write for Failing<'_> {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if self.left == 0 {
                    return Err(io::Error::other("the disk is full"));
                }
                let n = buf.len().min(self.left);
                self.left -= n;
                let mut file = self.file;
                file.write(&buf[..n])
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let path = std::env::temp_dir().join(format!("bridge-write-cut-{}.pgn", std::process::id()));
        let before = b"[Event \"a\"]\n1. e4 *\n";
        let len = before.len() as u64;
        std::fs::write(&path, before).unwrap();
        let file = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let failed = append_at_end(&file, len, b"\n[Event \"b\"]\n1. d4 *\n\n", &mut Failing { file: &file, left: 7 });
        assert!(failed.is_err());
        drop(file);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let file = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
        append_at_end(&file, len, b"\n1. d4 *\n\n", &mut &file).unwrap();
        drop(file);
        assert_eq!(std::fs::read(&path).unwrap(), b"[Event \"a\"]\n1. e4 *\n\n1. d4 *\n\n");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn if_match_names_a_generation() {
        let g = 0x0123_4567_89ab_cdef;
        assert!(names_generation("\"0123456789abcdef\"", g));
        assert!(names_generation(" 0123456789abcdef ", g));
        assert!(!names_generation("W/\"0123456789abcdef\"", g));
        assert!(!names_generation("*", g));
        assert!(!names_generation("\"0123456789abcdee\"", g));
    }
}
