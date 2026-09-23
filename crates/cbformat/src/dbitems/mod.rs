//! ChessBase's database window list: `DBItems.cbini` in the ChessBase
//! documents folder, holding the databases the window shows.
//!
//! The file is a flat list of tagged key-value items; the layout is in
//! `docs/format-notes.md`, "The database window list".

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::v2::Date;
use crate::{Error, Result};

mod local;

pub use local::local_path;

/// The list's file name in the ChessBase documents folder.
pub const FILE_NAME: &str = "DBItems.cbini";
const MAGIC: [u8; 4] = [0x0c, 0x0b, 0x0a, 0x0e];
/// Largest file accepted. The lists examined are under 2 KB. Decoding is
/// linear in the input: the decoded strings hold at most twice its bytes (the
/// Latin-1 fallback turns one byte into two), and each section name is stored
/// once however many entries it holds.
pub const MAX_FILE: u64 = 1 << 20;

const TAG_SECTION: u8 = 0xff;
const TAG_BYTE: u8 = 0x01;
const TAG_INT: u8 = 0x08;
/// String tags. `1e` carries non-ASCII (UTF-8) text; `19` and `1a` have held
/// ASCII only in the files examined.
const TAG_TEXTS: [u8; 3] = [0x19, 0x1a, 0x1e];

/// One item's value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// A section header: the items after it, up to the next header, belong to it.
    Section,
    Byte(u8),
    Int(i32),
    /// A string item; `tag` is one of the three string tags.
    Text {
        tag: u8,
        text: String,
    },
}

/// One item of the list, in file order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub key: String,
    pub value: Value,
}

/// Every item of a list file, in file order.
pub fn items(bytes: &[u8]) -> Result<Vec<Item>> {
    let bad = |what: String| Error::Format(format!("{FILE_NAME}: {what}"));
    if bytes.len() as u64 > MAX_FILE {
        return Err(bad(format!("{} bytes, more than {MAX_FILE}", bytes.len())));
    }
    if bytes.len() < 8 || bytes[..4] != MAGIC {
        return Err(bad("no list header".into()));
    }
    let payload = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    if payload != bytes.len() - 8 {
        return Err(bad(format!("header says {payload} bytes follow, the file has {}", bytes.len() - 8)));
    }
    let mut r = Reader { b: bytes, at: 8 };
    let mut out = Vec::new();
    while r.at < bytes.len() {
        let at = r.at;
        let tag = r.byte()?;
        let value = match tag {
            TAG_SECTION => Value::Section,
            TAG_BYTE => Value::Byte(r.byte()?),
            TAG_INT => Value::Int(i32::from_le_bytes(r.take(4)?.try_into().unwrap())),
            t if TAG_TEXTS.contains(&t) => Value::Text { tag: t, text: text(r.string()?) },
            // Items carry no length, so nothing after an unknown tag can be found.
            t => return Err(bad(format!("unknown item tag {t:#04x} at {at:#x}"))),
        };
        let key = text(r.string()?);
        out.push(Item { key, value });
    }
    Ok(out)
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.b.len());
        let end = end.ok_or_else(|| Error::Format(format!("{FILE_NAME}: item at {:#x} runs past the end", self.at)))?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    /// A string: its byte length as a little-endian `int`, then the bytes.
    fn string(&mut self) -> Result<&'a [u8]> {
        let n = i32::from_le_bytes(self.take(4)?.try_into().unwrap());
        let n = usize::try_from(n).map_err(|_| Error::Format(format!("{FILE_NAME}: string length {n}")))?;
        self.take(n)
    }
}

/// UTF-8 when the bytes are valid UTF-8, otherwise one character per byte
/// (Latin-1), so no byte is lost.
fn text(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_owned(),
        Err(_) => b.iter().map(|&c| c as char).collect(),
    }
}

/// A database's format, from its file extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// `.2cbh`, ChessBase 17 and later.
    Cbh2,
    /// `.cbh`, the classic format.
    Cbh,
    Pgn,
    Other,
}

impl Format {
    pub fn of(path: &str) -> Format {
        let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
        match ext.as_deref() {
            Some("2cbh") => Format::Cbh2,
            Some("cbh") => Format::Cbh,
            Some("pgn") => Format::Pgn,
            _ => Format::Other,
        }
    }
}

/// One database in the window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The path as stored: an absolute Windows path in the files examined.
    pub path: String,
    /// The title the window shows, or the file name without its extension
    /// when the stored title is empty.
    pub name: String,
    pub format: Format,
    /// The section holding the entry, an index into [`DbList::sections`]:
    /// `2cbg` for 2CBH databases and `Databases` for the others in the files
    /// examined. `None` before the first section header.
    pub section: Option<usize>,
    /// The six numbers stored after the title, in order.
    pub numbers: [i64; 6],
}

impl Entry {
    /// A format code: 28 for 2CBH, 1 for CBH and 3 for PGN in the files examined.
    pub fn type_code(&self) -> i64 {
        self.numbers[1]
    }
    /// The number of games when ChessBase last looked.
    pub fn games(&self) -> i64 {
        self.numbers[2]
    }
    /// When the database was last used, as a ChessBase date.
    pub fn last_used(&self) -> Date {
        Date(self.numbers[4] as i32)
    }
    /// When the database was added to the window, as a ChessBase date.
    pub fn added(&self) -> Date {
        Date(self.numbers[5] as i32)
    }
}

/// The decoded list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DbList {
    /// The databases, in file order.
    pub entries: Vec<Entry>,
    /// The section names, in file order, each stored once.
    pub sections: Vec<String>,
    /// The reference database's stored path (`RefDB` in section `2cbh`).
    pub reference: Option<String>,
    /// The stored path of the database selected in the window (`Selected` in `Status`).
    pub selected: Option<String>,
    /// The window's sort setting (`Sort` in `Status`), raw: its values are
    /// not yet interpreted.
    pub sort: Option<i32>,
    /// `SortDir0` to `SortDir7` in `Status`, raw: their values are not yet
    /// interpreted. With `sort` they should give the order the window shows.
    pub sort_dir: [Option<u8>; 8],
}

impl DbList {
    /// The name of the section holding `entry`.
    pub fn section_of(&self, entry: &Entry) -> Option<&str> {
        entry.section.and_then(|i| self.sections.get(i)).map(String::as_str)
    }
}

/// Decodes a list file. A string item whose value is a title followed by six
/// comma-separated integers is a database entry, keyed by its path.
pub fn parse(bytes: &[u8]) -> Result<DbList> {
    let mut list = DbList::default();
    let mut section: Option<usize> = None;
    for item in items(bytes)? {
        let in_section = |name: &str| section.is_some_and(|i| list.sections[i] == name);
        match item.value {
            Value::Section => {
                section = Some(list.sections.len());
                list.sections.push(item.key);
            }
            Value::Text { text, .. } => {
                if let Some((title, numbers)) = title_and_numbers(&text) {
                    let name = if title.is_empty() { stem(&item.key).to_owned() } else { title.to_owned() };
                    let format = Format::of(&item.key);
                    list.entries.push(Entry { path: item.key, name, format, section, numbers });
                } else if in_section("2cbh") && item.key == "RefDB" {
                    list.reference = Some(text);
                } else if in_section("Status") && item.key == "Selected" {
                    list.selected = Some(text);
                }
            }
            Value::Int(v) if in_section("Status") && item.key == "Sort" => list.sort = Some(v),
            Value::Byte(v) if in_section("Status") => {
                if let Some(slot) = sort_dir_slot(&item.key) {
                    list.sort_dir[slot] = Some(v);
                }
            }
            _ => {}
        }
    }
    Ok(list)
}

/// `SortDir0` to `SortDir7` as 0 to 7.
fn sort_dir_slot(key: &str) -> Option<usize> {
    let digit = key.strip_prefix("SortDir")?;
    let slot: usize = digit.parse().ok().filter(|_| digit.len() == 1)?;
    (slot < 8).then_some(slot)
}

/// Splits `title,n1,…,n6`. The title may itself contain commas, so the six
/// numbers are taken from the right.
fn title_and_numbers(value: &str) -> Option<(&str, [i64; 6])> {
    let mut parts = value.rsplitn(7, ',');
    let mut numbers = [0i64; 6];
    for slot in numbers.iter_mut().rev() {
        *slot = parts.next()?.trim().parse().ok()?;
    }
    Some((parts.next()?, numbers))
}

/// The file name of a stored Windows or Unix path, without its extension.
fn stem(path: &str) -> &str {
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// Where the list lies in a ChessBase documents folder.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Located {
    /// `DBItems.cbini`, the file ChessBase reads, when present.
    pub file: Option<PathBuf>,
    /// `DBItems-<name>.cbini` files beside it. They are OneDrive sync-conflict
    /// copies named after the computer that lost the conflict, not files
    /// ChessBase reads, and are never used.
    pub conflict_copies: Vec<PathBuf>,
}

pub fn locate(dir: &Path) -> Result<Located> {
    let io = |e| Error::Io(dir.to_owned(), e);
    let mut located = Located::default();
    let main = dir.join(FILE_NAME);
    if main.is_file() {
        located.file = Some(main);
    }
    for entry in std::fs::read_dir(dir).map_err(io)? {
        let name = entry.map_err(io)?.file_name();
        let name = name.to_string_lossy();
        let lower = name.to_ascii_lowercase();
        if lower.starts_with("dbitems-") && lower.ends_with(".cbini") {
            located.conflict_copies.push(dir.join(&*name));
        }
    }
    located.conflict_copies.sort();
    Ok(located)
}

/// Reads the list of a ChessBase documents folder; `None` when it has none.
/// The file is opened once and at most [`MAX_FILE`] + 1 bytes are read, so a
/// file that grows after it was found is still read within the bound.
pub fn read(dir: &Path) -> Result<Option<DbList>> {
    let Some(file) = locate(dir)?.file else { return Ok(None) };
    let opened = std::fs::File::open(&file).map_err(|e| Error::Io(file.clone(), e))?;
    parse(&read_capped(opened, &file)?).map(Some)
}

/// Reads at most [`MAX_FILE`] + 1 bytes of `source`, the file `path`; more
/// than [`MAX_FILE`] is an error.
fn read_capped(source: impl Read, path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    source.take(MAX_FILE + 1).read_to_end(&mut bytes).map_err(|e| Error::Io(path.to_owned(), e))?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(Error::Format(format!("{FILE_NAME}: more than {MAX_FILE} bytes")));
    }
    Ok(bytes)
}

/// Windows file attributes that mark a cloud-only placeholder: the data is not
/// on disk and reading it would start a download.
pub const CLOUD_ONLY_ATTRIBUTES: u32 = 0x0040_0000 // FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
    | 0x0004_0000 // FILE_ATTRIBUTE_RECALL_ON_OPEN
    | 0x0000_1000; // FILE_ATTRIBUTE_OFFLINE

/// Whether a file with these Windows attributes is a cloud-only placeholder.
pub fn is_cloud_only(attributes: u32) -> bool {
    attributes & CLOUD_ONLY_ATTRIBUTES != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_keep_their_commas() {
        let (t, n) = title_and_numbers("Smith, J. games,0,28,5,1,1037620,1037559").unwrap();
        assert_eq!(t, "Smith, J. games");
        assert_eq!(n, [0, 28, 5, 1, 1037620, 1037559]);
        assert_eq!(title_and_numbers("Selected"), None);
        assert_eq!(title_and_numbers("a,1,2,3,4,5"), None);
        assert_eq!(title_and_numbers("a,1,2,x,4,5,6"), None);
        assert_eq!(title_and_numbers(",1,2,3,4,5,6"), Some(("", [1, 2, 3, 4, 5, 6])));
    }

    #[test]
    fn stems_and_formats() {
        assert_eq!(stem(r"C:\Bases\Mega Database 2026.2cbh"), "Mega Database 2026");
        assert_eq!(stem("/tmp/x/Old.cbh"), "Old");
        assert_eq!(stem("noext"), "noext");
        assert_eq!(Format::of(r"C:\a.2CBH"), Format::Cbh2);
        assert_eq!(Format::of("a.cbh"), Format::Cbh);
        assert_eq!(Format::of("a.PGN"), Format::Pgn);
        assert_eq!(Format::of("a.cbv"), Format::Other);
    }

    #[test]
    fn cloud_only_attributes() {
        assert!(!is_cloud_only(0x0008_0420)); // archive, reparse point, pinned: a local OneDrive file
        assert!(is_cloud_only(0x0040_0000 | 0x0010_0000 | 0x0400)); // recall on data access, unpinned
        assert!(is_cloud_only(0x1000));
        assert!(!is_cloud_only(0x20));
    }

    #[test]
    fn reads_are_capped_whatever_the_source_holds() {
        // An endless source stands for a file that grows while it is read.
        let path = Path::new(FILE_NAME);
        let err = read_capped(std::io::repeat(0), path).unwrap_err().to_string();
        assert!(err.contains("more than 1048576 bytes"), "{err}");
        let exact = read_capped(std::io::repeat(7).take(MAX_FILE), path).unwrap();
        assert_eq!(exact.len() as u64, MAX_FILE);
    }

    #[test]
    fn sort_dir_keys() {
        assert_eq!(sort_dir_slot("SortDir0"), Some(0));
        assert_eq!(sort_dir_slot("SortDir7"), Some(7));
        assert_eq!(sort_dir_slot("SortDir8"), None);
        assert_eq!(sort_dir_slot("SortDir07"), None);
        assert_eq!(sort_dir_slot("SortDir+1"), None);
        assert_eq!(sort_dir_slot("Sort"), None);
    }

    #[test]
    fn latin1_fallback() {
        assert_eq!(text(b"M\xfcller"), "Müller");
        assert_eq!(text("Чорні".as_bytes()), "Чорні");
    }
}
