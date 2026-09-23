//! The user's Documents folder, where ChessBase keeps its documents folder
//! and the database window's list.

use std::path::PathBuf;

/// Names the Documents folder instead of the system's answer, on every system:
/// for tests, and for running the bridge outside Windows.
pub const DOCUMENTS_VAR: &str = "OSCHESS_BRIDGE_DOCUMENTS";

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

#[cfg(windows)]
fn known_documents() -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_Documents, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

    let mut path: *mut u16 = std::ptr::null_mut();
    // SAFETY: the folder id is a valid GUID that outlives the call, a null
    // token means the current user, and `path` is a valid place for the
    // returned pointer. The shell allocates the string with the COM task
    // allocator even when the call fails, so it is freed with CoTaskMemFree
    // in both cases, once, after its UTF-16 units up to the terminating zero
    // have been copied.
    let text = unsafe {
        let hr = SHGetKnownFolderPath(&FOLDERID_Documents, KF_FLAG_DEFAULT as u32, std::ptr::null_mut(), &mut path);
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
