//! The user's Documents folder, where ChessBase keeps its documents folder
//! and the database window's list, and the folder names a database's place is
//! told by.

use std::path::{Component, Path, PathBuf, Prefix, PrefixComponent};

/// Names the Documents folder instead of the system's answer, on every system:
/// for tests, and for running the bridge outside Windows.
pub const DOCUMENTS_VAR: &str = "OSCHESS_BRIDGE_DOCUMENTS";

/// Names the user's profile folder instead of the system's answer, as
/// [`DOCUMENTS_VAR`] does the Documents folder.
pub const PROFILE_VAR: &str = "OSCHESS_BRIDGE_PROFILE";

/// ChessBase's documents folder, `Documents\ChessBase`, where it keeps
/// `DBItems.cbini`; `None` when there is no Documents folder.
pub fn chessbase_folder() -> Option<PathBuf> {
    documents().map(|d| d.join("ChessBase"))
}

/// The Documents folder: [`DOCUMENTS_VAR`] when set, otherwise the Windows
/// known folder, which follows a redirection (to a cloud folder, for
/// example). `None` elsewhere.
pub fn documents() -> Option<PathBuf> {
    match std::env::var_os(DOCUMENTS_VAR) {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => known_documents(),
    }
}

/// The user's profile folder, `C:\Users\<name>`: [`PROFILE_VAR`] when set,
/// otherwise the Windows known folder. `None` elsewhere.
pub fn profile() -> Option<PathBuf> {
    match std::env::var_os(PROFILE_VAR) {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => known_profile(),
    }
}

/// The folder holding the database at `path`, as `GET /v1/databases` names
/// it (#298): its path segments relative to ChessBase's documents folder
/// `chessbase` when the database is inside it, else, after a `~` segment,
/// relative to the user's profile folder `profile` when inside that, else the
/// whole path from its drive (from the root elsewhere). So the user's name,
/// and a redirection of Documents to a cloud folder, never show. The paths
/// are first made absolute and their `.` and `..` resolved
/// ([`resolved`]), so every spelling of a folder names it alike.
pub fn folder_segments(path: &Path, chessbase: Option<&Path>, profile: Option<&Path>) -> Vec<String> {
    let path = resolved(path);
    let folder = path.parent().unwrap_or(&path);
    if let Some(rest) = chessbase.and_then(|root| below(folder, &resolved(root))) {
        return rest;
    }
    if let Some(rest) = profile.and_then(|root| below(folder, &resolved(root))) {
        return std::iter::once("~".to_owned()).chain(rest).collect();
    }
    segments(folder.components())
}

/// `path` made absolute against the working folder, with `.` and `..`
/// resolved by their spelling alone: no file is looked at, so a missing
/// database or one kept in the cloud is named as any other, and nothing is
/// downloaded. A `..` at the root stays there.
fn resolved(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// The segments of `path` past `root`, when `root` is `path` or holds it.
fn below(path: &Path, root: &Path) -> Option<Vec<String>> {
    let mut rest = path.components();
    for part in root.components() {
        if !same(rest.next()?, part) {
            return None;
        }
    }
    Some(segments(rest))
}

/// Whether two path components name the same folder: a drive or share
/// however its prefix is written ([`prefix_name`]), and on Windows, whose
/// file systems ignore case, whatever the case they are written in.
fn same(a: Component<'_>, b: Component<'_>) -> bool {
    let name = |part: Component<'_>| match part {
        Component::Prefix(prefix) => prefix_name(prefix),
        other => other.as_os_str().to_string_lossy().into_owned(),
    };
    if cfg!(windows) { name(a).to_lowercase() == name(b).to_lowercase() } else { a == b }
}

/// A path's drive or share in its plain spelling: `\\?\C:` and `c:` are
/// `C:`, `\\?\UNC\nas\share` is `\\nas\share`. Any other prefix, such as a
/// device's, keeps its spelling.
fn prefix_name(prefix: PrefixComponent<'_>) -> String {
    match prefix.kind() {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => format!("{}:", char::from(letter).to_ascii_uppercase()),
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
            format!(r"\\{}\{}", server.to_string_lossy(), share.to_string_lossy())
        }
        _ => prefix.as_os_str().to_string_lossy().into_owned(),
    }
}

/// The segments of a path: its drive (`D:`, or `\\server\share`) or root
/// first, then each folder's name.
fn segments<'a>(components: impl Iterator<Item = Component<'a>>) -> Vec<String> {
    let mut drive = false;
    let mut out = Vec::new();
    for part in components {
        match part {
            Component::Prefix(prefix) => {
                drive = true;
                out.push(prefix_name(prefix));
            }
            Component::RootDir if drive => {}
            Component::RootDir => out.push(std::path::MAIN_SEPARATOR_STR.to_owned()),
            // Resolved away by `resolved`.
            Component::CurDir | Component::ParentDir => {}
            Component::Normal(name) => out.push(name.to_string_lossy().into_owned()),
        }
    }
    out
}

#[cfg(windows)]
fn known_documents() -> Option<PathBuf> {
    known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Documents)
}

#[cfg(windows)]
fn known_profile() -> Option<PathBuf> {
    known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Profile)
}

/// The Windows known folder `id` of the current user.
#[cfg(windows)]
fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{KF_FLAG_DEFAULT, SHGetKnownFolderPath};

    let mut path: *mut u16 = std::ptr::null_mut();
    // SAFETY: the folder id is a valid GUID that outlives the call, a null
    // token means the current user, and `path` is a valid place for the
    // returned pointer. The shell allocates the string with the COM task
    // allocator even when the call fails, so it is freed with CoTaskMemFree
    // in both cases, once, after its UTF-16 units up to the terminating zero
    // have been copied.
    let text = unsafe {
        let hr = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT as u32, std::ptr::null_mut(), &mut path);
        let text = (hr >= 0 && !path.is_null()).then(|| {
            let len = (0..).take_while(|&i| *path.add(i) != 0).count();
            OsString::from_wide(std::slice::from_raw_parts(path, len))
        });
        CoTaskMemFree(path as *const core::ffi::c_void);
        text
    };
    text.filter(|t| !t.is_empty()).map(PathBuf::from)
}

#[cfg(not(windows))]
fn known_documents() -> Option<PathBuf> {
    None
}

#[cfg(not(windows))]
fn known_profile() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(path: &str, chessbase: Option<&str>, profile: Option<&str>) -> Vec<String> {
        folder_segments(Path::new(path), chessbase.map(Path::new), profile.map(Path::new))
    }

    #[cfg(not(windows))]
    #[test]
    fn a_folder_is_named_from_chessbase_then_the_profile_then_the_root() {
        let (cb, home) = (Some("/home/u/Documents/ChessBase"), Some("/home/u"));
        assert_eq!(folder("/home/u/Documents/ChessBase/Bases/Mega2026/Mega.2cbh", cb, home), ["Bases", "Mega2026"]);
        assert!(folder("/home/u/Documents/ChessBase/Top.2cbh", cb, home).is_empty());
        assert_eq!(folder("/home/u/Desktop/Chess/Games.pgn", cb, home), ["~", "Desktop", "Chess"]);
        assert_eq!(folder("/home/u/Games.pgn", cb, home), ["~"]);
        assert_eq!(folder("/srv/chess/TWIC/twic.pgn", cb, home), ["/", "srv", "chess", "TWIC"]);
        // A folder whose name begins like the root's is not inside it.
        assert_eq!(folder("/home/user2/Games.pgn", cb, home), ["/", "home", "user2"]);
        // ChessBase's folder wins over the profile that holds it; without
        // either, the whole path.
        assert_eq!(folder("/home/u/Documents/ChessBase/A/x.2cbh", cb, None), ["A"]);
        assert_eq!(
            folder("/home/u/Documents/ChessBase/A/x.2cbh", None, None),
            ["/", "home", "u", "Documents", "ChessBase", "A"]
        );
    }

    /// Every spelling of a folder names it alike: `..` and `.` resolved, a
    /// relative path taken from the working folder, so a path written through
    /// ChessBase's folder or the profile's parent still hides the user's name.
    #[cfg(not(windows))]
    #[test]
    fn a_folder_is_named_by_where_it_is_not_how_it_is_spelt() {
        let (cb, home) = (Some("/r/alice/Documents/ChessBase"), Some("/r/alice"));
        let through = "/r/alice/Documents/ChessBase/../../../alice/Desktop/Chess/db.2cbh";
        assert_eq!(folder(through, cb, home), ["~", "Desktop", "Chess"]);
        assert_eq!(folder("/r/alice/./Desktop/../Games/db.pgn", cb, home), ["~", "Games"]);
        assert_eq!(
            folder(
                "/r/alice/Documents/ChessBase/Bases/x.2cbh",
                Some("/r/alice/Documents/../Documents/ChessBase"),
                home
            ),
            ["Bases"]
        );
        assert_eq!(folder("/../../x.pgn", cb, home), ["/"]);
        // From the working folder: as the bridge, started there, reads it.
        let cwd = std::env::current_dir().unwrap();
        let profile = cwd.parent().map(|p| p.to_string_lossy().into_owned());
        let name = cwd.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(folder("Chess/db.2cbh", None, profile.as_deref()), ["~".to_owned(), name, "Chess".to_owned()]);
        assert_eq!(folder("db.2cbh", Some(&cwd.to_string_lossy()), None), Vec::<String>::new());
    }

    #[cfg(windows)]
    #[test]
    fn a_folder_is_named_from_chessbase_then_the_profile_then_the_drive() {
        let (cb, profile) = (Some(r"C:\Users\Олена\OneDrive\Documents\ChessBase"), Some(r"C:\Users\Олена"));
        assert_eq!(folder(r"c:\users\олена\onedrive\documents\chessbase\Bases\Mega.2cbh", cb, profile), ["Bases"]);
        assert_eq!(folder(r"C:\Users\Олена\Desktop\Games.pgn", cb, profile), ["~", "Desktop"]);
        assert_eq!(folder(r"D:\Chess\TWIC\twic.pgn", cb, profile), ["D:", "Chess", "TWIC"]);
        assert_eq!(folder(r"\\nas\share\Bases\x.2cbh", cb, profile), [r"\\nas\share", "Bases"]);
        // A verbatim spelling of the database or of a root names the same folder.
        assert_eq!(folder(r"\\?\C:\Users\Олена\OneDrive\Documents\ChessBase\Bases\x.2cbh", cb, profile), ["Bases"]);
        assert_eq!(folder(r"\\?\c:\Users\Олена\Desktop\x.pgn", cb, profile), ["~", "Desktop"]);
        let verbatim = (Some(r"\\?\C:\Users\Олена\OneDrive\Documents\ChessBase"), Some(r"\\?\C:\Users\Олена"));
        assert_eq!(
            folder(r"C:\Users\Олена\OneDrive\Documents\ChessBase\Bases\x.2cbh", verbatim.0, verbatim.1),
            ["Bases"]
        );
        assert_eq!(folder(r"C:\Users\Олена\Desktop\x.pgn", verbatim.0, verbatim.1), ["~", "Desktop"]);
        assert_eq!(folder(r"\\?\D:\Chess\x.pgn", cb, profile), ["D:", "Chess"]);
        assert_eq!(folder(r"\\?\UNC\nas\share\Bases\x.2cbh", Some(r"\\nas\share"), None), ["Bases"]);
        assert_eq!(folder(r"\\?\UNC\nas\share\Bases\x.2cbh", None, None), [r"\\nas\share", "Bases"]);
        assert_eq!(folder(r"\\nas\share\Bases\x.2cbh", Some(r"\\?\UNC\NAS\Share"), None), ["Bases"]);
    }
}
