//! Small databases for tests, written to a temporary directory.
//!
//! Built only with the `fixture` feature, which the tests of this workspace
//! enable; a release build never contains it.

use std::path::{Path, PathBuf};

use crate::movetable::{self, Captured, Color, MoveWord, Piece, Sq};
use crate::v2::checksum;

/// A square from its name: `sq("e4")`.
pub fn sq(name: &str) -> Sq {
    let b = name.as_bytes();
    assert!(b.len() == 2 && (b'a'..=b'h').contains(&b[0]) && (b'1'..=b'8').contains(&b[1]), "square {name}");
    (b[1] - b'1') * 8 + (b[0] - b'a')
}

/// The word of a move that neither captures nor promotes. Panics when no word
/// names the move.
pub fn quiet(color: Color, piece: Piece, from: &str, to: &str) -> u16 {
    let mv =
        MoveWord::Normal { color, piece, from: sq(from), to: sq(to), captured: Captured::Nothing, promotion: None };
    movetable::encode(mv).unwrap_or_else(|| panic!("no word for {mv:?}"))
}

/// Words as little-endian bytes, the layout of a move record's content.
pub fn bytes(words: &[u16]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// A complete move record: the frame around `content`, with variant `tag`.
pub fn framed(tag: u16, content: &[u8]) -> Vec<u8> {
    let mut r = vec![0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11];
    r.extend((content.len() as i32).to_le_bytes());
    r.extend(0i32.to_le_bytes());
    r.extend(checksum(content).to_be_bytes());
    r.extend(tag.to_le_bytes());
    r.extend(content);
    r.extend((content.len() as i64 + 34).to_le_bytes());
    r
}

/// A `.2lid` header with one entity type of `container` bytes per entity and
/// `count` entities.
pub fn lid_header(container: i32, count: i64) -> Vec<u8> {
    let mut d = Vec::new();
    d.extend(184i32.to_be_bytes());
    d.extend(1i32.to_be_bytes());
    d.extend(container.to_be_bytes());
    d.extend(count.to_be_bytes());
    d.extend((-1i64).to_be_bytes());
    d.resize(184, 0);
    d
}

/// A database in a temporary directory, removed on drop.
pub struct TempDb {
    dir: PathBuf,
}

impl TempDb {
    /// The directory holding `db.2cbh`, `db.2cbg` and `db.2lid`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The database path without its extension, as [`crate::v2::Database::open`] takes it.
    pub fn base(&self) -> PathBuf {
        self.dir.join("db")
    }

    /// Takes ownership of `dir`, which is removed on drop.
    pub fn at(dir: PathBuf) -> TempDb {
        TempDb { dir }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Builds a database: game records, move records in the order they are added,
/// a `.2lid` file that holds no entities unless replaced, and a `.2cba` file
/// once an annotation record is added.
pub struct Builder {
    records: Vec<[u8; 192]>,
    cbg: Vec<u8>,
    cba: Option<Vec<u8>>,
    lid: Vec<u8>,
}

impl Default for Builder {
    fn default() -> Self {
        Builder { records: Vec::new(), cbg: vec![0; 12], cba: None, lid: lid_header(1024, 0) }
    }
}

impl Builder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a move record of variant `tag` holding `words`, and returns its
    /// offset in `.2cbg`.
    pub fn moves(&mut self, tag: u16, words: &[u16]) -> i64 {
        let offset = self.cbg.len() as i64;
        self.cbg.extend(framed(tag, &bytes(words)));
        offset
    }

    /// Appends an annotation record holding `content` (see [`annotations`]),
    /// and returns its offset in `.2cba`.
    pub fn annotations(&mut self, content: &[u8]) -> i64 {
        let cba = self.cba.get_or_insert_with(|| vec![0; 12]);
        let offset = cba.len() as i64;
        cba.extend(framed(crate::v2::ANNOTATION_TAG, content));
        offset
    }

    /// Appends a game whose move record is at `moves` and annotation record
    /// at `annotations`.
    pub fn annotated_game(&mut self, moves: i64, annotations: i64) -> &mut [u8; 192] {
        let rec = self.game(moves);
        rec[0x10..0x18].copy_from_slice(&annotations.to_le_bytes());
        rec
    }

    /// Appends a game, won by white, whose move record is at `offset`, and
    /// returns its record for further changes.
    pub fn game(&mut self, offset: i64) -> &mut [u8; 192] {
        let mut rec = [0u8; 192];
        rec[0] = 1;
        rec[2] = 1;
        rec[3] = 1;
        rec[0x08..0x10].copy_from_slice(&offset.to_le_bytes());
        rec[0x58] = 2;
        self.records.push(rec);
        self.records.last_mut().unwrap()
    }

    /// Replaces the whole `.2lid` file.
    pub fn lid(&mut self, lid: Vec<u8>) -> &mut Self {
        self.lid = lid;
        self
    }

    /// Writes the database to a new temporary directory named after `name`,
    /// which must be unique among the tests that run at the same time.
    ///
    /// When there is a `.2cba`, a game without an annotation record gets an
    /// empty one, as in databases ChessBase writes.
    pub fn write(&self, name: &str) -> TempDb {
        let dir = std::env::temp_dir().join(format!("cbformat-fixture-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cbh = vec![0u8; 192];
        cbh[0x0a..0x0c].copy_from_slice(&192i16.to_le_bytes());
        cbh[0x0d] = 5;
        let mut cba = self.cba.clone();
        for rec in &self.records {
            let mut rec = *rec;
            if let Some(cba) = &mut cba
                && rec[0x10..0x18] == [0; 8]
            {
                rec[0x10..0x18].copy_from_slice(&(cba.len() as i64).to_le_bytes());
                cba.extend(framed(crate::v2::ANNOTATION_TAG, &annotations(&[])));
            }
            cbh.extend(rec);
        }
        if let Some(mut cba) = cba {
            let total = cba.len() as i64;
            cba[..8].copy_from_slice(&total.to_le_bytes());
            cba[8..10].copy_from_slice(&12i16.to_le_bytes());
            std::fs::write(dir.join("db.2cba"), cba).unwrap();
        }
        let mut cbg = self.cbg.clone();
        let total = cbg.len() as i64;
        cbg[..8].copy_from_slice(&total.to_le_bytes());
        cbg[8..10].copy_from_slice(&12i16.to_le_bytes());
        cbg[10..12].copy_from_slice(&[0, 5]);
        std::fs::write(dir.join("db.2cbh"), cbh).unwrap();
        std::fs::write(dir.join("db.2cbg"), cbg).unwrap();
        std::fs::write(dir.join("db.2lid"), &self.lid).unwrap();
        TempDb { dir }
    }
}

/// Builds a `DBItems.cbini` database window list, item by item, in the layout
/// `crate::dbitems` reads.
#[derive(Default)]
pub struct DbItems {
    body: Vec<u8>,
}

impl DbItems {
    pub fn new() -> Self {
        Self::default()
    }

    fn string(&mut self, s: &[u8]) {
        self.body.extend((s.len() as i32).to_le_bytes());
        self.body.extend(s);
    }

    /// A section header.
    pub fn section(&mut self, name: &str) -> &mut Self {
        self.body.push(0xff);
        self.string(name.as_bytes());
        self
    }

    /// A string item with the given tag (`0x19`, `0x1a` or `0x1e`).
    pub fn text(&mut self, tag: u8, key: &[u8], value: &[u8]) -> &mut Self {
        self.body.push(tag);
        self.string(value);
        self.string(key);
        self
    }

    pub fn int(&mut self, key: &str, value: i32) -> &mut Self {
        self.body.push(0x08);
        self.body.extend(value.to_le_bytes());
        self.string(key.as_bytes());
        self
    }

    pub fn byte(&mut self, key: &str, value: u8) -> &mut Self {
        self.body.push(0x01);
        self.body.push(value);
        self.string(key.as_bytes());
        self
    }

    /// A database entry: `title,n1,…,n6` keyed by `path`, tagged `0x1e` when
    /// the title is not ASCII and `0x19` otherwise, as ChessBase writes them.
    pub fn database(&mut self, path: &str, title: &str, numbers: [i64; 6]) -> &mut Self {
        let mut value = title.to_owned();
        for n in numbers {
            value.push_str(&format!(",{n}"));
        }
        let tag = if value.is_ascii() { 0x19 } else { 0x1e };
        self.text(tag, path.as_bytes(), value.as_bytes())
    }

    /// Appends raw bytes, for damaged files.
    pub fn raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.body.extend(bytes);
        self
    }

    /// The file: magic, the big-endian length of what follows, and the items.
    pub fn bytes(&self) -> Vec<u8> {
        let mut out = vec![0x0c, 0x0b, 0x0a, 0x0e];
        out.extend((self.body.len() as u32).to_be_bytes());
        out.extend(&self.body);
        out
    }
}

/// An annotation record's content: position blocks, each a position (−1 for
/// the game) and its annotations, then the end marker.
pub fn annotations(blocks: &[(i32, Vec<Vec<u8>>)]) -> Vec<u8> {
    let mut v = Vec::new();
    for (position, anns) in blocks {
        v.extend(position.to_le_bytes());
        v.extend((anns.len() as i32).to_le_bytes());
        for a in anns {
            v.extend(a);
        }
    }
    v.extend(0x7fff_ffffi32.to_le_bytes());
    v
}

/// A text annotation: after the move, or before it; `language` as in
/// [`crate::v2::language`].
pub fn text(before: bool, language: u16, text: &str) -> Vec<u8> {
    let mut v = (if before { 0x82u16 } else { 0x02 }).to_le_bytes().to_vec();
    v.extend([0, 0]);
    v.extend(language.to_le_bytes());
    v.extend((text.len() as i32).to_le_bytes());
    v.extend(text.as_bytes());
    v
}

/// A symbols annotation: NAGs on the move, on the position, and a prefix.
pub fn symbols(on_move: u8, on_position: u8, prefix: u8) -> Vec<u8> {
    vec![3, 0, on_move, on_position, prefix]
}

/// A square numbered from 1, file by file, as annotations store it.
fn cb_square(name: &str) -> u8 {
    let s = sq(name);
    (s % 8) * 8 + s / 8 + 1
}

/// A coloured-squares annotation from (colour, square) pairs.
pub fn squares(items: &[(u8, &str)]) -> Vec<u8> {
    let data: Vec<u8> = items.iter().flat_map(|&(c, s)| [c, cb_square(s)]).collect();
    let mut v = vec![4, 0];
    v.extend((data.len() as i32).to_le_bytes());
    v.extend(data);
    v
}

/// An arrows annotation from (colour, from, to) triples.
pub fn arrows(items: &[(u8, &str, &str)]) -> Vec<u8> {
    let data: Vec<u8> = items.iter().flat_map(|&(c, f, t)| [c, cb_square(f), cb_square(t)]).collect();
    let mut v = vec![5, 0];
    v.extend((data.len() as i32).to_le_bytes());
    v.extend(data);
    v
}
