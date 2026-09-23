//! Where the list of databases comes from: ChessBase's database window
//! (`DBItems.cbini`), then `bridge.toml`, then the command line.

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
        if !dir.is_dir() {
            return Ok(Vec::new());
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
        Ok(list.entries.into_iter().map(|e| Listed { path: dbitems::local_path(dir, &e.path), name: e.name }).collect())
    }

    /// The `databases` of `bridge.toml`, as written; empty when there is no file.
    pub fn configured(&self) -> Result<Vec<PathBuf>, String> {
        let Some(path) = &self.config else { return Ok(Vec::new()) };
        match std::fs::read_to_string(path) {
            Ok(text) => config::parse(&text).map(|c| c.databases).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// A hash of the sizes and modification times of everything the list is
    /// read from, given the configured paths: it changes when the list may
    /// have. A configured folder counts by its own time, which changes when a
    /// file is added to it or removed.
    pub fn signature(&self, configured: &[PathBuf]) -> u64 {
        let mut hash = Hash::new();
        let chessbase = self.chessbase.as_ref().map(|d| d.join(dbitems::FILE_NAME));
        for path in chessbase.iter().chain(&self.config).chain(configured) {
            hash.write_file(path);
        }
        hash.finish()
    }
}

/// The databases a configured path names: the path itself, or for a folder
/// the ChessBase databases directly in it, by file name.
pub fn expand(path: &Path) -> Vec<Listed> {
    if !path.is_dir() {
        return vec![Listed::at(path.to_owned())];
    }
    let Ok(dir) = std::fs::read_dir(path) else { return Vec::new() };
    let mut found: Vec<PathBuf> = dir
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| matches!(Format::of(p), Format::TwoCbh | Format::Cbh))
        .collect();
    found.sort();
    found.into_iter().map(Listed::at).collect()
}
