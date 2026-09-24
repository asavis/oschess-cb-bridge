//! Where the list of databases comes from: ChessBase's database window
//! (`DBItems.cbini`), then `bridge.toml`, then the command line.

use std::collections::HashMap;
use std::fmt::Display;
use std::path::{Path, PathBuf};

use cbformat::dbitems;

use crate::catalog::{Format, Hash};
use crate::config;

/// The places the list is read from.
#[derive(Clone, Debug, Default)]
pub struct Sources {
    /// ChessBase's documents folder, holding `DBItems.cbini`.
    pub chessbase: Option<PathBuf>,
    /// `bridge.toml`; its `databases` are read again when it changes.
    pub config: Option<PathBuf>,
    /// Databases named on the command line.
    pub fixed: Vec<PathBuf>,
}

/// A database of the list: its path on this computer and the name shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub path: PathBuf,
    pub name: String,
}

impl Listed {
    /// A database listed by path alone, named after its file.
    pub fn at(path: PathBuf) -> Listed {
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Listed { path, name }
    }
}

impl Sources {
    /// The databases ChessBase's window lists, in the file's order, named as
    /// the window names them. `Ok(empty)` when there is no such list.
    pub fn window(&self) -> Result<Vec<Listed>, String> {
        let Some(dir) = &self.chessbase else { return Ok(Vec::new()) };
        match std::fs::metadata(dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Ok(Vec::new()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("{}: {e}", dir.display())),
        }
        let located = dbitems::locate(dir).map_err(|e| e.to_string())?;
        if !located.conflict_copies.is_empty() {
            eprintln!(
                "oschess-bridge: ignoring {} sync-conflict cop{} of {}",
                located.conflict_copies.len(),
                if located.conflict_copies.len() == 1 { "y" } else { "ies" },
                dbitems::FILE_NAME
            );
        }
        let Some(list) = dbitems::read(dir).map_err(|e| e.to_string())? else { return Ok(Vec::new()) };
        Ok(list
            .into_window_order()
            .into_iter()
            .map(|e| Listed { path: dbitems::local_path(dir, &e.path), name: e.name })
            .collect())
    }

    /// The `databases` of `bridge.toml`, as written; empty when there is no file.
    pub fn configured(&self) -> Result<Vec<PathBuf>, String> {
        let Some(path) = &self.config else { return Ok(Vec::new()) };
        match std::fs::metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            // A pipe would block the read.
            Ok(m) if !m.is_file() => return Err(format!("{}: not a regular file", path.display())),
            _ => {}
        }
        match std::fs::read_to_string(path) {
            Ok(text) => config::parse(&text).map(|c| c.databases).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }
}

/// The databases a configured path names: the path itself, or for a folder
/// the ChessBase databases directly in it, by file name. In a folder only
/// regular files (or links to them) count: a pipe or a folder named like a
/// database is none.
pub fn expand(path: &Path) -> Result<Vec<Listed>, String> {
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => {}
        // A database, or a path that names nothing and is then reported missing.
        Ok(_) => return Ok(vec![Listed::at(path.to_owned())]),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![Listed::at(path.to_owned())]),
        Err(e) => return Err(err(e)),
    }
    let mut found = Vec::new();
    for entry in std::fs::read_dir(path).map_err(err)? {
        let file = entry.map_err(err)?.path();
        if !matches!(Format::of(&file), Format::TwoCbh | Format::Cbh) {
            continue;
        }
        match std::fs::metadata(&file) {
            Ok(m) if m.is_file() => found.push(file),
            Ok(_) => {}
            // Removed while the folder was read, or a link to nothing.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(err(e)),
        }
    }
    found.sort();
    Ok(found.into_iter().map(Listed::at).collect())
}

/// What was last read from each source. A source is read again when its
/// signature (sizes and modification times) is not the one it was last read
/// at. The signature is taken before reading, so that a change made while it
/// is read shows next time. A read that fails keeps what was last read and
/// records no signature, so the source is read again on the next request: a
/// folder or file that cannot be read for a while loses none of its databases.
#[derive(Default)]
pub(crate) struct Read {
    window: Kept<Vec<Listed>>,
    config: Kept<Vec<PathBuf>>,
    /// The databases of each configured path.
    paths: HashMap<PathBuf, Kept<Vec<Listed>>>,
}

impl Read {
    /// Reads again the sources that need it; whether any was read.
    pub(crate) fn update(&mut self, sources: &Sources) -> bool {
        let window_file = sources.chessbase.as_ref().map(|d| d.join(dbitems::FILE_NAME));
        let mut changed =
            self.window.update(signature(window_file.as_deref()), &"the database window list", || sources.window());
        changed |= self.config.update(signature(sources.config.as_deref()), &"bridge.toml", || sources.configured());
        let configured = &self.config.value;
        let before = self.paths.len();
        self.paths.retain(|p, _| configured.contains(p));
        changed |= self.paths.len() != before;
        for path in configured {
            let kept = self.paths.entry(path.clone()).or_default();
            changed |= kept.update(folder_signature(path), &path.display(), || expand(path));
        }
        changed
    }

    /// The databases in order: the window's, the configured ones, the fixed ones.
    pub(crate) fn listed(&self, sources: &Sources) -> Vec<Listed> {
        let mut listed = self.window.value.clone();
        for path in &self.config.value {
            listed.extend(self.paths.get(path).into_iter().flat_map(|k| k.value.iter().cloned()));
        }
        listed.extend(sources.fixed.iter().cloned().map(Listed::at));
        listed
    }
}

/// One source's last contents read without error.
#[derive(Default)]
struct Kept<T> {
    /// The signature they were read at; `None` until a read succeeds, and
    /// after one fails.
    signature: Option<u64>,
    value: T,
    /// The last read failed; its error has been logged.
    failing: bool,
}

impl<T> Kept<T> {
    /// Reads the source with `read` unless it was last read at `signature`;
    /// whether it was read.
    fn update(&mut self, signature: u64, what: &dyn Display, read: impl FnOnce() -> Result<T, String>) -> bool {
        if self.signature == Some(signature) {
            return false;
        }
        match read() {
            Ok(value) => {
                (self.value, self.signature, self.failing) = (value, Some(signature), false);
                true
            }
            Err(e) => {
                if !self.failing {
                    eprintln!("oschess-bridge: {what} cannot be read, keeping what was read before: {e}");
                }
                (self.signature, self.failing) = (None, true);
                false
            }
        }
    }
}

/// The size and modification time of the file or folder at `path`.
fn signature(path: Option<&Path>) -> u64 {
    let mut hash = Hash::new();
    if let Some(path) = path {
        hash.write_file(path);
    }
    hash.finish()
}

/// [`signature`] of a configured path, and for a folder also the name, size
/// and modification time of each database file in it. A folder's own time
/// moves with the kernel's coarse clock, a few milliseconds a step, and its
/// size rarely changes, so a database added right after a listing could leave
/// both as they were and stay unseen until the folder changed again.
fn folder_signature(path: &Path) -> u64 {
    let mut hash = Hash::new();
    hash.write_file(path);
    if let Ok(entries) = std::fs::read_dir(path) {
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|f| matches!(Format::of(f), Format::TwoCbh | Format::Cbh))
            .collect();
        files.sort();
        for file in files {
            hash.write(file.as_os_str().as_encoded_bytes());
            hash.write_file(&file);
        }
    }
    hash.finish()
}
