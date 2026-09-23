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
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Builds a database: game records, move records in the order they are added,
/// and a `.2lid` file that holds no entities unless replaced.
pub struct Builder {
    records: Vec<[u8; 192]>,
    cbg: Vec<u8>,
    lid: Vec<u8>,
}

impl Default for Builder {
    fn default() -> Self {
        Builder { records: Vec::new(), cbg: vec![0; 12], lid: lid_header(1024, 0) }
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
    pub fn write(&self, name: &str) -> TempDb {
        let dir = std::env::temp_dir().join(format!("cbformat-fixture-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cbh = vec![0u8; 192];
        cbh[0x0a..0x0c].copy_from_slice(&192i16.to_le_bytes());
        cbh[0x0d] = 5;
        for rec in &self.records {
            cbh.extend(rec);
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
