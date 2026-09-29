//! Where the bridge keeps its files: its data folder, which holds the
//! settings, the pairing token and the log, and the folders of the indexes it
//! builds. Every way of starting the bridge, and every tool and test that
//! gives it a data folder of its own, asks here (#175).

use std::path::{Path, PathBuf};

/// Where the bridge keeps its settings and token: `OSCHESS_BRIDGE_HOME` when
/// set, else `%APPDATA%\oschess-bridge` on Windows and
/// `$XDG_CONFIG_HOME/oschess-bridge` or `~/.config/oschess-bridge` elsewhere.
pub fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = var("OSCHESS_BRIDGE_HOME") {
        return Some(dir);
    }
    let base = if cfg!(windows) {
        var("APPDATA")
    } else {
        var("XDG_CONFIG_HOME").or_else(|| var("HOME").map(|h| h.join(".config")))
    };
    base.map(|b| b.join("oschess-bridge"))
}

/// Where the bridge whose data folder is `data` keeps its position indexes
/// and the heads and names files beside them (#147): on Windows, for the
/// data folder `OSCHESS_BRIDGE_HOME` does not set,
/// `%LOCALAPPDATA%\oschess bridge\index`, since `%APPDATA%` roams with the
/// user's profile and the indexes of a large database take gigabytes; else
/// the data folder's `index`, where they were kept before.
pub fn index_dir(data: &Path) -> PathBuf {
    let local = if cfg!(windows) && var("OSCHESS_BRIDGE_HOME").is_none() { var("LOCALAPPDATA") } else { None };
    index_dir_in(data, data_dir().as_deref(), local.as_deref())
}

/// Where the bridge whose data folder is `data` keeps the header indexes of
/// its PGN files: the data folder's `pgn`.
pub fn pgn_dir(data: &Path) -> PathBuf {
    data.join("pgn")
}

/// [`index_dir`] of the data folder `data`, where the data folder by default
/// is `default` and the local application data folder is `local`.
fn index_dir_in(data: &Path, default: Option<&Path>, local: Option<&Path>) -> PathBuf {
    match local {
        Some(local) if default == Some(data) => local.join("oschess bridge").join("index"),
        _ => data.join("index"),
    }
}

/// The environment variable `name`, unless it is unset or empty.
fn var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default data folder keeps its indexes in the local application
    /// data folder when there is one; any other data folder, as tests and
    /// tools give, in its own `index`.
    #[test]
    fn the_default_data_folder_keeps_its_indexes_apart() {
        let (data, local) = (Path::new("/roaming/oschess-bridge"), Path::new("/local"));
        assert_eq!(index_dir_in(data, Some(data), Some(local)), local.join("oschess bridge").join("index"));
        assert_eq!(index_dir_in(data, Some(data), None), data.join("index"));
        let other = Path::new("/tmp/profile");
        assert_eq!(index_dir_in(other, Some(data), Some(local)), other.join("index"));
        assert_eq!(index_dir_in(other, None, Some(local)), other.join("index"));
        if !cfg!(windows) {
            assert_eq!(index_dir(data), data.join("index"), "elsewhere the folder stays where it was");
        }
    }
}
