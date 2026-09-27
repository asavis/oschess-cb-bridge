//! The UCI engines already installed with ChessBase or Fritz (#13, #53),
//! listed for the user to choose from. Nothing found here is run until the
//! user chooses it; [`crate::engine::probe`] then checks it is a UCI engine.
//!
//! ChessBase keeps an engine the user added by hand as a `.uci` file, INI text
//! naming the executable (`Filename=`), in `%APPDATA%\ChessBase\Engines.UCI`;
//! one moved to a subfolder there is deactivated. Engines installed with the
//! programs live in `Engines` or `Engines.x64` under `Common Files\ChessBase`
//! or `ChessBase` in Program Files. ChessBase's own `.eng` and `.engine`
//! engines are not UCI and are not listed.

use std::path::{Path, PathBuf};

/// An engine found on this computer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// The name ChessBase shows, or the engine's folder or file name.
    pub name: String,
    pub path: PathBuf,
    /// Which program it came with: `ChessBase` or `Fritz`.
    pub source: &'static str,
}

/// Where to look: the roaming application data folder and the Program Files
/// folders of this computer, and the bridge's own data folder, whose
/// `engines` holds the builds it installed.
#[derive(Clone, Debug, Default)]
pub struct Roots {
    pub app_data: Option<PathBuf>,
    pub program_files: Vec<PathBuf>,
    pub bridge_data: Option<PathBuf>,
}

impl Roots {
    /// This computer's folders, from the environment; none on other systems.
    pub fn system() -> Roots {
        if !cfg!(windows) {
            return Roots::default();
        }
        let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
        let mut program_files: Vec<PathBuf> =
            ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"].iter().filter_map(|n| var(n)).collect();
        program_files.dedup();
        Roots { app_data: var("APPDATA"), program_files, bridge_data: None }
    }
}

/// The source of an engine the bridge installed itself.
pub const BRIDGE: &str = "bridge";

/// The largest `.uci` file read; ChessBase writes a few hundred bytes.
const MAX_UCI_FILE: u64 = 64 << 10;
/// The most engines listed.
const MAX_FOUND: usize = 100;

/// The engines under `roots`, each executable once, in the order found:
/// those added by hand first, then those installed with the programs.
pub fn find(roots: &Roots) -> Vec<Found> {
    let mut found: Vec<Found> = Vec::new();
    let mut add = |f: Found| {
        let key = std::fs::canonicalize(&f.path).unwrap_or_else(|_| f.path.clone());
        let known = found.iter().any(|g| std::fs::canonicalize(&g.path).unwrap_or_else(|_| g.path.clone()) == key);
        if !known && found.len() < MAX_FOUND && f.path.is_file() {
            found.push(f);
        }
    };
    // The builds the bridge installed first, each in engines\stockfish-<version>.
    if let Some(data) = &roots.bridge_data {
        for folder in folders(&data.join("engines")) {
            let Some(version) = folder.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_prefix("stockfish-"))
            else {
                continue;
            };
            for path in files(&folder).into_iter().filter(|p| is_stockfish(p)) {
                add(Found { name: format!("Stockfish {version}"), path, source: BRIDGE });
            }
        }
    }
    let mut uci_folders: Vec<PathBuf> = Vec::new();
    if let Some(app_data) = &roots.app_data {
        uci_folders.push(app_data.join("ChessBase").join("Engines.UCI"));
    }
    for base in &roots.program_files {
        uci_folders.push(base.join("Common Files").join("ChessBase").join("Engines.Uci"));
    }
    for folder in &uci_folders {
        for file in files(folder).into_iter().filter(|f| has_extension(f, "uci")) {
            if let Some(f) = read_uci(&file) {
                add(f);
            }
        }
    }
    for base in &roots.program_files {
        for parent in [base.join("Common Files").join("ChessBase"), base.join("ChessBase")] {
            for engines in ["Engines.x64", "Engines"] {
                let folder = parent.join(engines);
                let mut candidates: Vec<PathBuf> = files(&folder);
                for sub in folders(&folder) {
                    candidates.extend(files(&sub));
                }
                for path in candidates.into_iter().filter(|p| is_stockfish(p)) {
                    add(Found { name: engine_name(&path), source: source_of(&path), path });
                }
            }
        }
    }
    found
}

/// The files directly in `folder`, sorted; none when it cannot be read.
fn files(folder: &Path) -> Vec<PathBuf> {
    entries(folder, |t| t.is_file())
}

fn folders(folder: &Path) -> Vec<PathBuf> {
    entries(folder, |t| t.is_dir())
}

fn entries(folder: &Path, keep: impl Fn(std::fs::FileType) -> bool) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(folder) else { return Vec::new() };
    let mut paths: Vec<PathBuf> =
        read.flatten().filter(|e| e.file_type().is_ok_and(&keep)).map(|e| e.path()).take(1000).collect();
    paths.sort();
    paths
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case(extension))
}

fn is_stockfish(path: &Path) -> bool {
    has_extension(path, "exe")
        && path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.to_ascii_lowercase().starts_with("stockfish"))
}

/// A Stockfish found in a folder: named after its folder when that names
/// Stockfish (`Stockfish 17.1`), else after its file.
fn engine_name(path: &Path) -> String {
    let folder = path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()).unwrap_or_default();
    if folder.to_ascii_lowercase().starts_with("stockfish") {
        return folder.to_string();
    }
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

fn source_of(path: &Path) -> &'static str {
    if path.to_string_lossy().to_ascii_lowercase().contains("fritz") { "Fritz" } else { "ChessBase" }
}

/// The engine a `.uci` file names: `Name=` and `Filename=` of its `[ENGINE]`
/// section. `None` when the file cannot be read or names no executable.
fn read_uci(file: &Path) -> Option<Found> {
    if std::fs::metadata(file).ok()?.len() > MAX_UCI_FILE {
        return None;
    }
    let text = decode(&std::fs::read(file).ok()?);
    let (mut section, mut name, mut filename) = (String::new(), None, None);
    for line in text.lines().map(str::trim) {
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = header.trim().to_ascii_uppercase();
            continue;
        }
        if section != "ENGINE" {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        match key.trim().to_ascii_lowercase().as_str() {
            "name" => name = Some(value.trim().to_string()),
            "filename" => filename = Some(value.trim().trim_matches('"').to_string()),
            _ => {}
        }
    }
    let path = PathBuf::from(filename.filter(|f| !f.is_empty())?);
    let name = name.filter(|n| !n.is_empty()).unwrap_or_else(|| engine_name(&path));
    Some(Found { name: name.chars().take(100).collect(), source: source_of(&path).max(source_of(file)), path })
}

/// A `.uci` file's text: UTF-16 with its byte order mark, UTF-8 with or
/// without one, else Windows-1252, as older ChessBase versions write it.
fn decode(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        return String::from_utf16_lossy(&units);
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => bytes.iter().map(|&b| windows_1252(b)).collect(),
    }
}

fn windows_1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}', '\u{90}', '‘',
        '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
    ];
    match b {
        0x80..=0x9F => HIGH[usize::from(b - 0x80)],
        _ => char::from(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree(PathBuf);

    impl Tree {
        fn new(name: &str) -> Tree {
            let dir = std::env::temp_dir().join(format!("bridge-engines-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Tree(dir)
        }

        fn file(&self, relative: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, bytes).unwrap();
            path
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn lists_engines_added_by_hand_and_installed_with_the_programs() {
        let t = Tree::new("list");
        let pf = t.0.join("Program Files");
        let sf17 =
            t.file("Program Files/ChessBase/Engines.x64/Stockfish 17.1/stockfish-windows-x86-64-avx2.exe", b"MZ");
        let sf16 = t.file("Program Files/Common Files/ChessBase/Engines/stockfish_16_x64.exe", b"MZ");
        let lc0 = t.file("Engines/lc0/lc0.exe", b"MZ");
        t.file("Program Files/ChessBase/Engines.x64/Fritz 19/Fritz19.engine", b"x");
        t.file("Program Files/ChessBase/Engines.x64/Komodo/komodo.exe", b"MZ");
        let uci = |name: &str, file: &Path| {
            format!("[ENGINE]\r\nName={name}\r\nAuthor=x\r\nFilename={}\r\n[OPTIONS]\r\nHash=64\r\n", file.display())
        };
        t.file("AppData/ChessBase/Engines.UCI/Lc0.uci", uci("Lc0 0.31", &lc0).as_bytes());
        // The same Stockfish added by hand is listed once, under its hand-given name.
        t.file("AppData/ChessBase/Engines.UCI/SF.uci", uci("Stockfish 17.1 (mine)", &sf17).as_bytes());
        // Deactivated, and naming no file.
        t.file("AppData/ChessBase/Engines.UCI/Inactive/Old.uci", uci("Old", &sf16).as_bytes());
        t.file("AppData/ChessBase/Engines.UCI/Empty.uci", b"[ENGINE]\r\nName=Empty\r\n");
        t.file("AppData/ChessBase/Engines.UCI/Gone.uci", uci("Gone", &t.0.join("missing.exe")).as_bytes());
        let roots = Roots { app_data: Some(t.0.join("AppData")), program_files: vec![pf], bridge_data: None };
        let found = find(&roots);
        let names: Vec<(&str, &str)> = found.iter().map(|f| (f.name.as_str(), f.source)).collect();
        assert_eq!(
            names,
            [("Lc0 0.31", "ChessBase"), ("Stockfish 17.1 (mine)", "ChessBase"), ("stockfish_16_x64", "ChessBase")]
        );
        assert_eq!(found[2].path, sf16);
    }

    #[test]
    fn lists_the_builds_the_bridge_installed_first() {
        let t = Tree::new("bridge");
        let installed = t.file("data/engines/stockfish-19/stockfish-windows-x86-64-universal.exe", b"MZ");
        t.file("data/engines/stockfish-19/Copying.txt", b"GPL");
        t.file("data/engines/.unpack-stockfish-20/stockfish/stockfish.exe", b"MZ");
        t.file("PF/ChessBase/Engines/Stockfish 16/stockfish.exe", b"MZ");
        let roots = Roots { app_data: None, program_files: vec![t.0.join("PF")], bridge_data: Some(t.0.join("data")) };
        let found = find(&roots);
        let names: Vec<(&str, &str)> = found.iter().map(|f| (f.name.as_str(), f.source)).collect();
        assert_eq!(names, [("Stockfish 19", BRIDGE), ("Stockfish 16", "ChessBase")]);
        assert_eq!(found[0].path, installed);
    }

    #[test]
    fn names_a_folder_found_stockfish_after_its_folder_and_fritz_by_its_path() {
        let t = Tree::new("names");
        t.file("PF/ChessBase/Engines/Stockfish 16/sf.exe", b"MZ");
        t.file("PF/ChessBase/Engines/Fritz 20 Engines/stockfish-fritz.exe", b"MZ");
        let found = find(&Roots { app_data: None, program_files: vec![t.0.join("PF")], bridge_data: None });
        let names: Vec<(&str, &str)> = found.iter().map(|f| (f.name.as_str(), f.source)).collect();
        assert_eq!(names, [("stockfish-fritz", "Fritz")]);
        let found = find(&Roots { app_data: None, program_files: vec![t.0.join("PF")], bridge_data: None });
        assert!(
            found.iter().all(|f| f.path.file_name().unwrap() != "sf.exe"),
            "only files named stockfish are guessed"
        );
        t.file("PF/ChessBase/Engines/Stockfish 16/stockfish.exe", b"MZ");
        let found = find(&Roots { app_data: None, program_files: vec![t.0.join("PF")], bridge_data: None });
        assert!(found.iter().any(|f| f.name == "Stockfish 16"));
    }

    #[test]
    fn reads_uci_files_in_the_encodings_chessbase_writes() {
        let t = Tree::new("encodings");
        let exe = t.file("e.exe", b"MZ");
        let text = format!("[Engine]\nname = Stöckfish\nFILENAME=\"{}\"\n", exe.display());
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        let mut latin = Vec::new();
        for c in text.chars() {
            latin.push(if c == 'ö' { 0xF6 } else { c as u8 });
        }
        for (label, bytes) in [("utf8", text.clone().into_bytes()), ("utf16", utf16), ("1252", latin)] {
            let file = t.file(&format!("{label}.uci"), &bytes);
            let found = read_uci(&file).unwrap_or_else(|| panic!("{label}"));
            assert_eq!((found.name.as_str(), found.path.as_path()), ("Stöckfish", exe.as_path()), "{label}");
        }
        assert_eq!(windows_1252(0x80), '€');
    }
}
