//! The databases the bridge serves: identity, state, and the open handle,
//! reopened whenever the files change (`docs/api.md`, "Database identity and
//! generations").

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cbformat::v2::{Database, EXTENSIONS};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    TwoCbh,
    Cbh,
    Pgn,
    Other,
}

impl Format {
    pub fn of(path: &Path) -> Format {
        match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
            Some("2cbh") => Format::TwoCbh,
            Some("cbh") => Format::Cbh,
            Some("pgn") => Format::Pgn,
            _ => Format::Other,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Format::TwoCbh => "2cbh",
            Format::Cbh => "cbh",
            Format::Pgn => "pgn",
            Format::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Ready,
    Missing,
    Unsupported,
    Unreadable,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Ready => "ready",
            State::Missing => "missing",
            State::Unsupported => "unsupported",
            State::Unreadable => "unreadable",
        }
    }
}

/// A database that is open, with the generation it was opened at.
#[derive(Clone)]
pub struct Opened {
    pub db: Arc<Database>,
    pub generation: u64,
}

pub struct Entry {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub format: Format,
    open: Mutex<Option<Opened>>,
}

impl Entry {
    pub fn new(path: PathBuf) -> Entry {
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Entry { id: id_of(&path), name, format: Format::of(&path), path, open: Mutex::new(None) }
    }

    /// The database's state, opening it if it is ready.
    pub fn state(&self) -> State {
        match self.open() {
            Ok(_) => State::Ready,
            Err(state) => state,
        }
    }

    /// The open database at its current generation: the handle already held
    /// when the files have not changed, a freshly opened one when they have.
    pub fn open(&self) -> Result<Opened, State> {
        if self.format != Format::TwoCbh {
            return Err(if self.path.exists() { State::Unsupported } else { State::Missing });
        }
        let generation = self.generation().ok_or(State::Missing)?;
        let mut slot = self.open.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(open) = slot.as_ref().filter(|o| o.generation == generation) {
            return Ok(open.clone());
        }
        let db = Database::open(&self.path).map_err(|_| State::Unreadable)?;
        let open = Opened { db: Arc::new(db), generation };
        *slot = Some(open.clone());
        Ok(open)
    }

    /// A hash of the sizes and modification times of the database's files;
    /// `None` when the header file is missing.
    pub fn generation(&self) -> Option<u64> {
        let stem = self.path.with_extension("");
        let mut hash = Fnv::new();
        for ext in EXTENSIONS {
            let mut path = stem.clone().into_os_string();
            path.push(ext);
            match std::fs::metadata(&path) {
                Ok(m) => {
                    hash.write(&m.len().to_le_bytes());
                    let modified = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
                    hash.write(&modified.map_or(0, |d| d.as_nanos()).to_le_bytes());
                }
                Err(_) if ext == ".2cbh" => return None,
                Err(_) => hash.write(&[0xff]),
            }
        }
        Some(hash.finish())
    }
}

pub struct Catalog {
    entries: Vec<Entry>,
}

impl Catalog {
    /// The databases at `paths`, in order, each once.
    pub fn new(paths: impl IntoIterator<Item = PathBuf>) -> Catalog {
        let mut entries: Vec<Entry> = Vec::new();
        for path in paths {
            let entry = Entry::new(path);
            if !entries.iter().any(|e| e.id == entry.id) {
                entries.push(entry);
            }
        }
        Catalog { entries }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }
}

/// 16 hexadecimal characters from the normalised path: separators unified and,
/// on Windows, case folded, since its paths are case-insensitive.
pub fn id_of(path: &Path) -> String {
    let mut text = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        text = text.to_lowercase();
    }
    let mut hash = Fnv::new();
    hash.write(text.as_bytes());
    format!("{:016x}", hash.finish())
}

/// 64-bit FNV-1a: stable across runs and platforms, unlike the standard
/// library's randomly keyed hasher.
struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_distinct() {
        let a = id_of(Path::new("C:\\Bases\\Mega.2cbh"));
        assert_eq!(a.len(), 16);
        assert_eq!(a, id_of(Path::new("C:/Bases/Mega.2cbh")));
        assert_ne!(a, id_of(Path::new("C:/Bases/Mega2.2cbh")));
        // FNV-1a test vector.
        let mut h = Fnv::new();
        h.write(b"a");
        assert_eq!(h.finish(), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn formats_and_states_without_files() {
        assert_eq!(Format::of(Path::new("x.2CBH")), Format::TwoCbh);
        assert_eq!(Format::of(Path::new("x.cbh")), Format::Cbh);
        assert_eq!(Format::of(Path::new("x.pgn")), Format::Pgn);
        assert_eq!(Entry::new(PathBuf::from("/no/such/base.2cbh")).state(), State::Missing);
        assert_eq!(Entry::new(PathBuf::from("/no/such/base.pgn")).state(), State::Missing);
        let catalog = Catalog::new([PathBuf::from("/a/x.2cbh"), PathBuf::from("/a/x.2cbh"), PathBuf::from("/a/y.pgn")]);
        assert_eq!(catalog.entries().len(), 2);
        assert_eq!(catalog.entries()[0].name, "x");
    }
}
