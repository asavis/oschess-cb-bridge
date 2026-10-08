//! Where the bridge keeps its files: its data folder, which holds the
//! settings, the pairing token and the log, and the folders of the indexes it
//! builds, and where the programs it starts find them. Every way of starting
//! the bridge, and every tool and test that gives it a data folder of its
//! own, asks here (#175).

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

/// The path at which another program finds `path`, a path of this process
/// (#289). The Microsoft Store package runs with its application data
/// virtualized. A folder the package creates in `%APPDATA%` or
/// `%LOCALAPPDATA%` is kept in the package's own folder,
/// `%LOCALAPPDATA%\Packages\<family>\LocalCache\Roaming` or `…\Local`, while
/// this process still sees it at its usual place. The programs the bridge
/// starts run outside the package: curl.exe, tar.exe, an engine, or the
/// program the shell opens a file with. Those find such a folder only in the
/// package's folder. Outside a package, and for a folder that existed before
/// the package created its own, this is `path` itself.
pub fn outside(path: &Path) -> PathBuf {
    let Some(cache) = package_cache() else { return path.to_path_buf() };
    let redirects: Vec<(PathBuf, PathBuf)> = [("APPDATA", "Roaming"), ("LOCALAPPDATA", "Local")]
        .into_iter()
        .filter_map(|(name, kept)| Some((var(name)?, cache.join(kept))))
        .collect();
    outside_in(path, &redirects)
}

/// [`outside`] for the folders `redirects`, each an application data folder
/// and the package's folder that keeps what the package creates in it. The
/// deepest part of `path` that exists decides. If that part is kept in the
/// package's folder, so is `path`; if this process sees it only at its usual
/// place, `path` is not redirected. A part that exists nowhere yet, such as
/// a file about to be written, follows the folder it will be created in.
fn outside_in(path: &Path, redirects: &[(PathBuf, PathBuf)]) -> PathBuf {
    for (root, kept) in redirects {
        let Ok(rel) = path.strip_prefix(root) else { continue };
        // The root itself is the user's folder, never redirected.
        for part in rel.ancestors().filter(|p| !p.as_os_str().is_empty()) {
            if kept.join(part).exists() {
                return kept.join(rel);
            }
            if root.join(part).exists() {
                return path.to_path_buf();
            }
        }
    }
    path.to_path_buf()
}

/// The folder in which the package this process runs in keeps what it
/// creates in the application data folders:
/// `%LOCALAPPDATA%\Packages\<family>\LocalCache`. `None` outside a package.
#[cfg(windows)]
fn package_cache() -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};
    use windows_sys::Win32::Storage::Packaging::Appx::GetCurrentPackageFamilyName;

    let mut len = 0u32;
    // SAFETY: a valid length pointer and no buffer, which asks the length
    // only; a process without a package answers APPMODEL_ERROR_NO_PACKAGE.
    if unsafe { GetCurrentPackageFamilyName(&mut len, std::ptr::null_mut()) } != ERROR_INSUFFICIENT_BUFFER {
        return None;
    }
    let mut name = vec![0u16; len as usize];
    // SAFETY: `name` has room for the `len` UTF-16 units the call asked for.
    if unsafe { GetCurrentPackageFamilyName(&mut len, name.as_mut_ptr()) } != ERROR_SUCCESS {
        return None;
    }
    // The length counts the terminating zero.
    let family = OsString::from_wide(&name[..name.iter().position(|&u| u == 0)?]);
    Some(var("LOCALAPPDATA")?.join("Packages").join(family).join("LocalCache"))
}

#[cfg(not(windows))]
fn package_cache() -> Option<PathBuf> {
    None
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

    /// A folder the package created is found in the package's folder, with
    /// whatever is in it or about to be written into it; a folder that was
    /// there before stays where it is, and so does a path elsewhere.
    #[test]
    fn other_programs_find_what_the_package_created_in_its_own_folder() {
        let base = std::env::temp_dir().join(format!("bridge-folders-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (roaming, kept) = (base.join("Roaming"), base.join("LocalCache").join("Roaming"));
        let redirects = [(roaming.clone(), kept.clone())];
        std::fs::create_dir_all(kept.join("oschess-bridge").join("engines")).unwrap();
        std::fs::create_dir_all(roaming.join("Earlier").join("engines")).unwrap();

        // Created by the package: the folder, a file not written yet, and a
        // path whose deeper folders do not exist yet either.
        let engines = roaming.join("oschess-bridge").join("engines");
        assert_eq!(outside_in(&engines, &redirects), kept.join("oschess-bridge").join("engines"));
        let zip = engines.join(".download-stockfish.zip");
        assert_eq!(
            outside_in(&zip, &redirects),
            kept.join("oschess-bridge").join("engines").join(".download-stockfish.zip")
        );
        let exe = engines.join("stockfish-19").join("stockfish.exe");
        assert_eq!(
            outside_in(&exe, &redirects),
            kept.join("oschess-bridge").join("engines").join("stockfish-19").join("stockfish.exe")
        );

        // There before the package: not redirected, nor a folder that exists
        // nowhere, nor the user's folder itself, nor a path elsewhere.
        let earlier = roaming.join("Earlier").join("engines").join("x.zip");
        assert_eq!(outside_in(&earlier, &redirects), earlier);
        let nowhere = roaming.join("Nowhere").join("x.zip");
        assert_eq!(outside_in(&nowhere, &redirects), nowhere);
        assert_eq!(outside_in(&roaming, &redirects), roaming);
        let elsewhere = base.join("Program Files").join("engine.exe");
        assert_eq!(outside_in(&elsewhere, &redirects), elsewhere);
        assert_eq!(outside_in(&zip, &[]), zip, "outside a package nothing is redirected");
        if !cfg!(windows) {
            assert_eq!(outside(&zip), zip);
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
