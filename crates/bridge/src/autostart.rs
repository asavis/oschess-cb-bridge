//! Starting with Windows: a value under the user's `Run` key that starts this
//! executable at sign-in. It is per user and needs no administrator rights.

use std::path::Path;

/// The key under `HKEY_CURRENT_USER` whose values Windows runs at sign-in.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// The bridge's value under [`RUN_KEY`].
pub const VALUE_NAME: &str = "oschess bridge";

/// The command that starts `exe`: its path in quotes, so that a space in it
/// cannot split it into a program and arguments.
pub fn command(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// Whether the stored command starts `exe`: the same path, quoted or not,
/// compared as Windows compares paths, ignoring case and separator style.
pub fn starts(value: &str, exe: &Path) -> bool {
    let value = value.trim();
    let program = match value.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(rest),
        None => value,
    };
    let fold = |p: &str| p.replace('/', "\\").to_lowercase();
    fold(program) == fold(&exe.display().to_string())
}

#[cfg(windows)]
pub use windows::{enabled, set};

#[cfg(windows)]
mod windows {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
    };

    use crate::wide;

    use super::{RUN_KEY, VALUE_NAME, command, starts};

    /// Whether the `Run` value exists and starts this executable. A value that
    /// starts a copy elsewhere, left by a moved download, does not count.
    pub fn enabled() -> bool {
        let Ok(exe) = std::env::current_exe() else { return false };
        let (key, name) = (wide(RUN_KEY), wide(VALUE_NAME));
        let mut bytes: u32 = 0;
        // SAFETY: the key and value names are NUL-terminated and outlive the
        // call; a null data pointer asks only for the size, written to `bytes`.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut bytes,
            )
        };
        if status != ERROR_SUCCESS || bytes == 0 || bytes > 64 * 1024 {
            return false;
        }
        let mut data = vec![0u16; (bytes as usize).div_ceil(2)];
        let mut size = (data.len() * 2) as u32;
        // SAFETY: `data` holds `size` bytes, and the call writes at most `size`
        // bytes to it, NUL-terminated because RRF_RT_REG_SZ asks for that.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                data.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if status != ERROR_SUCCESS {
            return false;
        }
        let len = data.iter().position(|&c| c == 0).unwrap_or(data.len());
        starts(&String::from_utf16_lossy(&data[..len]), &exe)
    }

    /// Writes or removes the `Run` value; the error is the Win32 error code.
    pub fn set(on: bool) -> Result<(), u32> {
        let (key, name) = (wide(RUN_KEY), wide(VALUE_NAME));
        let status = if on {
            let exe = std::env::current_exe().map_err(|e| e.raw_os_error().unwrap_or(0) as u32)?;
            let data = wide(&command(&exe));
            // SAFETY: the names are NUL-terminated; `data` is a NUL-terminated
            // UTF-16 string of exactly the byte length passed.
            unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    name.as_ptr(),
                    REG_SZ,
                    data.as_ptr().cast(),
                    (data.len() * 2) as u32,
                )
            }
        } else {
            // SAFETY: the key and value names are NUL-terminated and outlive the call.
            unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) }
        };
        match status {
            ERROR_SUCCESS => Ok(()),
            ERROR_FILE_NOT_FOUND if !on => Ok(()),
            e => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn the_command_quotes_a_path_with_spaces() {
        let exe = PathBuf::from(r"C:\Users\Anna Maria\Downloads\oschess-bridge.exe");
        assert_eq!(command(&exe), r#""C:\Users\Anna Maria\Downloads\oschess-bridge.exe""#);
        assert!(starts(&command(&exe), &exe));
    }

    #[test]
    fn a_stored_command_is_matched_as_windows_matches_paths() {
        let exe = PathBuf::from(r"C:\Users\Anna Maria\Downloads\oschess-bridge.exe");
        assert!(starts(r#""c:\users\anna maria\downloads\OSCHESS-BRIDGE.EXE""#, &exe));
        assert!(starts(r#"  "C:/Users/Anna Maria/Downloads/oschess-bridge.exe" --flag "#, &exe));
        assert!(starts(r"C:\Users\Anna Maria\Downloads\oschess-bridge.exe", &exe));
        assert!(!starts(r#""C:\Users\Anna Maria\Desktop\oschess-bridge.exe""#, &exe));
        assert!(!starts(r#""C:\Users\Anna Maria\Downloads\oschess-bridge.exe.old""#, &exe));
        assert!(!starts("", &exe));
    }
}
