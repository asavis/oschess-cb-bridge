//! What the settings window shows and changes in `bridge.toml`: the databases
//! and folders added to ChessBase's own list, and the port.

use std::path::{Path, PathBuf};

use bridge::config::Config;
use serde::Serialize;

/// A database file or a folder of databases added in the settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Extra {
    pub path: String,
    pub folder: bool,
    /// The folder's databases, directly in it; `None` for a file.
    pub databases: Option<usize>,
    /// Whether the folder or file is there.
    pub present: bool,
}

/// The extra databases of `config`, as the settings window lists them.
pub fn extras(config: &Config) -> Vec<Extra> {
    config.databases.iter().map(|path| extra(path)).collect()
}

fn extra(path: &Path) -> Extra {
    let meta = std::fs::metadata(path).ok();
    let folder = meta.as_ref().is_some_and(|m| m.is_dir());
    Extra {
        path: path.to_string_lossy().into_owned(),
        folder,
        databases: folder.then(|| databases_in(path)),
        present: meta.is_some(),
    }
}

/// The regular `.2cbh`, `.cbh` and `.pgn` files directly in `folder`, as the
/// bridge serves a folder.
pub fn databases_in(folder: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(folder) else { return 0 };
    entries
        .flatten()
        .filter(|e| {
            let path = e.path();
            let ext = path.extension().and_then(|x| x.to_str()).map(str::to_ascii_lowercase);
            matches!(ext.as_deref(), Some("2cbh" | "cbh" | "pgn"))
                && std::fs::metadata(&path).is_ok_and(|m| m.is_file())
        })
        .count()
}

/// `config` with `path` added at the end; unchanged when it is there already.
pub fn with_database(config: &Config, path: PathBuf) -> Config {
    let mut next = config.clone();
    if !next.databases.iter().any(|p| same_path(p, &path)) {
        next.databases.push(path);
    }
    next
}

/// `config` without `path`.
pub fn without_database(config: &Config, path: &Path) -> Config {
    let mut next = config.clone();
    next.databases.retain(|p| !same_path(p, path));
    next
}

/// Paths compare as Windows does: case and the slash style do not matter.
fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    norm(a) == norm(b)
}

/// A port typed in the settings: a number from 1024 to 65535, which needs no
/// administrator rights.
pub fn parse_port(text: &str) -> Option<u16> {
    let text = text.trim();
    if text.is_empty() || text.len() > 5 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u16>().ok().filter(|p| *p >= 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports() {
        assert_eq!(parse_port("39581"), Some(39581));
        assert_eq!(parse_port(" 40000 "), Some(40000));
        assert_eq!(parse_port("1024"), Some(1024));
        assert_eq!(parse_port("65535"), Some(65535));
        for bad in ["", "80", "1023", "65536", "99999", "123456", "-1", "+4000", "4e4", "40 00", "0x9c9d", "３９５８１"]
        {
            assert_eq!(parse_port(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn adding_and_removing_databases() {
        let config = Config::default();
        let one = with_database(&config, PathBuf::from(r"C:\Bases\Archive"));
        assert_eq!(one.databases, [PathBuf::from(r"C:\Bases\Archive")]);
        assert_eq!(with_database(&one, PathBuf::from(r"c:/bases/archive/")), one, "the same folder once");
        let two = with_database(&one, PathBuf::from(r"D:\Tournaments"));
        assert_eq!(
            without_database(&two, Path::new(r"c:\BASES\archive")).databases,
            [PathBuf::from(r"D:\Tournaments")]
        );
        assert_eq!(without_database(&two, Path::new(r"E:\Other")), two);
    }

    #[test]
    fn folders_count_their_databases() {
        let dir = std::env::temp_dir().join(format!("bridge-app-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub.2cbh")).unwrap();
        for f in ["A.2cbh", "A.2cbg", "B.CBH", "C.pgn", "notes.txt"] {
            std::fs::write(dir.join(f), b"").unwrap();
        }
        let config = Config { databases: vec![dir.clone(), dir.join("A.2cbh"), dir.join("gone")], ..Config::default() };
        let list = extras(&config);
        assert_eq!(
            list.iter().map(|e| (e.folder, e.databases, e.present)).collect::<Vec<_>>(),
            [(true, Some(3), true), (false, None, true), (false, None, false)]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
