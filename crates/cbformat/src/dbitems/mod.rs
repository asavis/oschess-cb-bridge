//! ChessBase's database window list: `DBItems.cbini` in the ChessBase
//! documents folder, holding the databases the window shows.
//!
//! The file is a flat list of tagged key-value items; the layout is in
//! `docs/format-notes.md`, "The database window list".

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::bytes::{Cursor, Fields};
use crate::codepage::CodePage;
use crate::game::Date;
use crate::view::Format;
use crate::{Error, Result};

mod local;
mod order;

pub use order::SORT_BY_ICON;

pub use local::local_path;

/// The list's file name in the ChessBase documents folder.
pub const FILE_NAME: &str = "DBItems.cbini";
const MAGIC: [u8; 4] = [0x0c, 0x0b, 0x0a, 0x0e];
/// Largest file accepted. The lists examined are under 2 KB. Decoding is
/// linear in the input: the decoded strings hold at most three times its
/// bytes (a code page character takes up to three bytes of UTF-8, and a
/// name shown for a cut title is its file name, at most three bytes for each
/// byte of the title), and each section name is stored once however many
/// entries it holds.
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

/// Every item of a list file, in file order. Strings that are not UTF-8 are
/// read in `page`, the computer's ANSI code page, as ChessBase reads them.
pub fn items(bytes: &[u8], page: CodePage) -> Result<Vec<Item>> {
    let bad = |what: String| Error::Format(format!("{FILE_NAME}: {what}"));
    if bytes.len() as u64 > MAX_FILE {
        return Err(bad(format!("{} bytes, more than {MAX_FILE}", bytes.len())));
    }
    let mut r = Cursor::new(bytes);
    let header = r.array::<8>().filter(|h| h.field::<0, 4>() == &MAGIC).ok_or_else(|| bad("no list header".into()))?;
    let payload = header.be_u32::<4>() as usize;
    if payload != r.left() {
        return Err(bad(format!("header says {payload} bytes follow, the file has {}", r.left())));
    }
    let mut out = Vec::new();
    while r.left() > 0 {
        let at = r.at();
        let tag = r.u8().ok_or_else(|| past_end(&r))?;
        let value = match tag {
            TAG_SECTION => Value::Section,
            TAG_BYTE => Value::Byte(r.u8().ok_or_else(|| past_end(&r))?),
            TAG_INT => Value::Int(r.le_i32().ok_or_else(|| past_end(&r))?),
            t if TAG_TEXTS.contains(&t) => Value::Text { tag: t, text: page.utf8_or(string(&mut r)?) },
            // Items carry no length, so nothing after an unknown tag can be found.
            t => return Err(bad(format!("unknown item tag {t:#04x} at {at:#x}"))),
        };
        let key = page.utf8_or(string(&mut r)?);
        out.push(Item { key, value });
    }
    Ok(out)
}

/// An item that runs past the end of the list, where `r` stopped reading it.
fn past_end(r: &Cursor<'_>) -> Error {
    Error::Format(format!("{FILE_NAME}: item at {:#x} runs past the end", r.at()))
}

/// A string: its byte length as a little-endian `int`, then the bytes.
fn string<'a>(r: &mut Cursor<'a>) -> Result<&'a [u8]> {
    let n = r.le_i32().ok_or_else(|| past_end(r))?;
    let n = usize::try_from(n).map_err(|_| Error::Format(format!("{FILE_NAME}: string length {n}")))?;
    r.take(n).ok_or_else(|| past_end(r))
}

/// One database in the window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The path as stored: an absolute Windows path in the files examined.
    pub path: String,
    /// The title the window shows, or the file name without its extension
    /// when the stored title is empty. A title ChessBase stored garbled, cut
    /// to one byte per UTF-16 unit or escaped, is shown as the text it meant
    /// (`docs/format-notes.md`, "Older entries").
    pub name: String,
    /// The format the path's extension names; `None` for any other file.
    pub format: Option<Format>,
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
    /// The window's sort setting (`Sort` in `Status`): [`SORT_BY_ICON`] is
    /// decoded, and other values are kept raw.
    pub sort: Option<i32>,
    /// `SortDir0` to `SortDir7` in `Status`, raw: their values are not yet
    /// interpreted. [`DbList::window_order`] gives the order the window shows.
    pub sort_dir: [Option<u8>; 8],
}

impl DbList {
    /// The name of the section holding `entry`.
    pub fn section_of(&self, entry: &Entry) -> Option<&str> {
        entry.section.and_then(|i| self.sections.get(i)).map(String::as_str)
    }
}

/// Decodes a list file, reading strings that are not UTF-8 in `page`. A
/// string item whose value is a title followed by six comma-separated
/// integers is a database entry, keyed by its path.
pub fn parse(bytes: &[u8], page: CodePage) -> Result<DbList> {
    let mut list = DbList::default();
    let mut section: Option<usize> = None;
    for item in items(bytes, page)? {
        let in_section = |name: &str| section.is_some_and(|i| list.sections[i] == name);
        match item.value {
            Value::Section => {
                section = Some(list.sections.len());
                list.sections.push(item.key);
            }
            Value::Text { text, .. } => {
                if let Some((title, numbers)) = title_and_numbers(&text) {
                    let stem = stem(&item.key);
                    let name = match title {
                        "" => stem.to_owned(),
                        _ => repaired_title(title, stem).unwrap_or_else(|| title.to_owned()),
                    };
                    let format = Format::of_extension(Path::new(&item.key));
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

/// The title ChessBase meant where it stored `title` garbled in one of the two
/// ways seen (`docs/format-notes.md`, "Older entries"); `None` for any other
/// title.
fn repaired_title(title: &str, stem: &str) -> Option<String> {
    cut_name(title, stem).map(str::to_owned).or_else(|| unescaped(title))
}

/// The part of the file name `stem` that `title` is as ChessBase stored it
/// cut: each UTF-16 unit reduced to its low byte, so that `Ладья` reads
/// `\u{1b}04LO`. The part is the whole name, or a start of it of two letters
/// or more that the rest begins with something other than a letter (the
/// title `завлечение` of `завлечение-1`). The whole title must be that image,
/// as every cut title seen was: a title that only starts with it is a title of
/// its own, such as `Queen endings` for `ё`, whose image is `Q`. Only an image
/// of ASCII bytes is matched, since those read the same in UTF-8 and in every
/// code page, and a name the cut leaves as it was, such as an ASCII one, is
/// never taken for cut. One pass over the name compares the title with its
/// image as it goes, so the work is linear in the name however long it is.
fn cut_name<'a>(title: &str, stem: &'a str) -> Option<&'a str> {
    let t = title.as_bytes();
    // Read so far: title bytes, letters, and whether the cut changed a character.
    let (mut at, mut letters, mut changed) = (0, 0, false);
    for (i, c) in stem.char_indices() {
        if at == t.len() {
            // The title ends before `c`: a start of the name.
            return (!c.is_alphabetic() && letters > 1 && changed).then(|| &stem[..i]);
        }
        for u in c.encode_utf16(&mut [0; 2]) {
            let low = u.to_le_bytes()[0];
            if !low.is_ascii() || t.get(at) != Some(&low) {
                return None;
            }
            at += 1;
        }
        letters += usize::from(c.is_alphabetic());
        changed |= !c.is_ascii();
    }
    (at == t.len() && changed).then_some(stem)
}

/// `title` read where ChessBase stored it as the UTF-8 bytes of the title
/// meant, each written `/` and two hex digits (`/D0/B5/D1/82/D1/8E/D0/B4/D0/B8`
/// for `етюди`): only a title made of such escapes alone that read as UTF-8
/// that is not ASCII.
fn unescaped(title: &str) -> Option<String> {
    let b = title.as_bytes();
    if b.is_empty() || !b.len().is_multiple_of(3) {
        return None;
    }
    let hex = |c: u8| char::from(c).to_digit(16).map(|d| d as u8);
    let bytes: Option<Vec<u8>> = b
        .as_chunks::<3>()
        .0
        .iter()
        .map(|c| match c {
            [b'/', h, l] => Some(hex(*h)? << 4 | hex(*l)?),
            _ => None,
        })
        .collect();
    String::from_utf8(bytes?).ok().filter(|t| !t.is_ascii())
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

/// Reads the list of a ChessBase documents folder, reading strings that are
/// not UTF-8 in `page`; `None` when it has none. The file is opened once and
/// at most [`MAX_FILE`] + 1 bytes are read, so a file that grows after it was
/// found is still read within the bound.
pub fn read(dir: &Path, page: CodePage) -> Result<Option<DbList>> {
    let Some(file) = locate(dir)?.file else { return Ok(None) };
    let opened = std::fs::File::open(&file).map_err(|e| Error::Io(file.clone(), e))?;
    parse(&read_capped(opened, &file)?, page).map(Some)
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
    fn stems() {
        assert_eq!(stem(r"C:\Bases\Mega Database 2026.2cbh"), "Mega Database 2026");
        assert_eq!(stem("/tmp/x/Old.cbh"), "Old");
        assert_eq!(stem("noext"), "noext");
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
    fn titles_cut_to_one_byte_are_told_apart() {
        let is_cut_name = |title: &str, stem: &str| cut_name(title, stem) == Some(stem);
        // `Ладья` and `Їжак`, each UTF-16 unit cut to its low byte.
        assert!(is_cut_name("\u{1b}04LO", "Ладья"));
        assert!(is_cut_name("\u{7}60:", "Їжак"));
        assert!(!is_cut_name("Ладья", "Ладья"), "a title stored whole");
        assert!(!is_cut_name("Rook", "Ладья"), "a title of its own");
        assert!(!is_cut_name("\u{1b}04L", "Ладья"), "shorter than the image");
        assert!(!is_cut_name("\u{1b}04LO (cbh)", "Ладья"), "longer than the image");
        // Titles of their own that start with the image of a short name:
        // `ё` cuts to `Q`, and `абв` to `012`.
        assert!(!is_cut_name("Queen endings", "ё"));
        assert!(!is_cut_name("012345 games", "абв"));
        assert!(!is_cut_name("Rook", "Rook"), "an ASCII name is not cut");
        // `é` cuts to a byte above ASCII, which a code page may have read as
        // another character: the image is not matched.
        assert!(!is_cut_name("\u{1b}\u{e9}", "Лé"));
        assert!(!is_cut_name("", ""));
    }

    /// A title cut from the start of its file name before a mark: `Ладья`
    /// of `Ладья-1` or `Ладья 2`, but no part of one letter (`ё` of `ё-1`,
    /// with a mark or a space or not) and no part that a letter follows
    /// (`Лад` of `Ладья`).
    #[test]
    fn titles_cut_from_a_start_of_the_file_name() {
        assert_eq!(cut_name("\u{1b}04LO", "Ладья-1"), Some("Ладья"));
        assert_eq!(cut_name("\u{1b}04LO", "Ладья 2"), Some("Ладья"));
        assert_eq!(cut_name("\u{1b}04LO-1", "Ладья-1"), Some("Ладья-1"));
        assert_eq!(cut_name("Q", "ё-1"), None);
        assert_eq!(cut_name("Q-", "ё-1"), None);
        assert_eq!(cut_name("Q ", "ё 2"), None);
        assert_eq!(cut_name("-Q", "-ё-1"), None);
        assert_eq!(cut_name("Q", "ё"), Some("ё"));
        assert_eq!(cut_name("\u{1b}04", "Ладья"), None);
        assert_eq!(cut_name("\u{1b}04", "Лад-ья"), Some("Лад"));
        assert_eq!(cut_name("Rook", "Rook-1"), None, "an ASCII start is not cut");
        // `Ъа` cuts to `*0` (U+042A, U+0430), and `𝄞`, two UTF-16 units, to
        // two bytes, `4` and 0x1e.
        assert_eq!(cut_name("*0", "Ъа-1"), Some("Ъа"));
        assert_eq!(cut_name("4\u{1e}", "𝄞"), Some("𝄞"));
        assert_eq!(cut_name("4", "𝄞"), None);
    }

    /// Titles stored as `/`-escaped UTF-8: `задачи`, and what is not one.
    #[test]
    fn titles_stored_as_escaped_utf8() {
        let title = "/D0/B7/D0/B0/D0/B4/D0/B0/D1/87/D0/B8";
        assert_eq!(unescaped(title).as_deref(), Some("задачи"));
        assert_eq!(unescaped(&title.to_lowercase()).as_deref(), Some("задачи"));
        assert_eq!(repaired_title(title, "x").as_deref(), Some("задачи"));
        assert_eq!(unescaped("/41/42"), None, "ASCII");
        assert_eq!(unescaped("/D0"), None, "not UTF-8");
        assert_eq!(unescaped("/D0/B7 x"), None, "more than escapes");
        assert_eq!(unescaped("/+1/B7"), None, "not hex");
        assert_eq!(unescaped("1/2"), None);
        assert_eq!(unescaped(""), None);
    }
}
