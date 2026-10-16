//! What the app asks Windows directly: the display language, the taskbar's
//! theme and Windows' own version.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_ROUTINE_FLAGS, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
};

use crate::status::Theme;

/// The user's display language as a Windows language identifier.
pub fn display_language() -> u16 {
    // SAFETY: the function takes no arguments and only returns a value.
    unsafe { GetUserDefaultUILanguage() }
}

/// The taskbar's theme: `SystemUsesLightTheme` in the user's personalisation
/// settings. Without the value, as on older Windows 10, the taskbar is dark.
pub fn taskbar_theme() -> Theme {
    let theme = dword(
        HKEY_CURRENT_USER,
        r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        "SystemUsesLightTheme",
    );
    if theme == Some(1) { Theme::Light } else { Theme::Dark }
}

/// Windows' version, build and release, such as `10.0.26100.4652 24H2`, as
/// `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion` names them (#333).
/// A part Windows does not name is left out.
pub fn windows_version() -> String {
    const KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let major = dword(HKEY_LOCAL_MACHINE, KEY, "CurrentMajorVersionNumber");
    let minor = dword(HKEY_LOCAL_MACHINE, KEY, "CurrentMinorVersionNumber");
    let build = string(HKEY_LOCAL_MACHINE, KEY, "CurrentBuildNumber");
    let mut version = match (major, minor, build) {
        (Some(major), Some(minor), Some(build)) => format!("{major}.{minor}.{build}"),
        (_, _, Some(build)) => build,
        _ => "unknown".to_string(),
    };
    if let Some(revision) = dword(HKEY_LOCAL_MACHINE, KEY, "UBR") {
        version.push_str(&format!(".{revision}"));
    }
    if let Some(release) = string(HKEY_LOCAL_MACHINE, KEY, "DisplayVersion") {
        version.push(' ');
        version.push_str(&release);
    }
    version
}

/// The DWORD `value` of `key` under `root`.
fn dword(root: HKEY, key: &str, value: &str) -> Option<u32> {
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // `data` has the four bytes `size` promises.
    let read = get(root, key, value, RRF_RT_REG_DWORD, (&mut data as *mut u32).cast::<c_void>(), &mut size);
    read.then_some(data)
}

/// The string `value` of `key` under `root`, of at most 255 characters.
fn string(root: HKEY, key: &str, value: &str) -> Option<String> {
    let mut data = [0u16; 256];
    let mut size = std::mem::size_of_val(&data) as u32;
    // `data` has the bytes `size` promises; Windows ends the string in a NUL.
    let read = get(root, key, value, RRF_RT_REG_SZ, data.as_mut_ptr().cast::<c_void>(), &mut size);
    let len = data.iter().position(|&c| c == 0).unwrap_or(data.len());
    read.then(|| String::from_utf16_lossy(&data[..len])).filter(|s| !s.is_empty())
}

/// Reads `value` of `key` under `root`, of the type `flags` allows, into the
/// `size` bytes at `data`; whether it was read.
fn get(root: HKEY, key: &str, value: &str, flags: REG_ROUTINE_FLAGS, data: *mut c_void, size: &mut u32) -> bool {
    let (key, value) = (wide(key), wide(value));
    // SAFETY: both names are NUL-terminated UTF-16 strings that outlive the
    // call, the value type is restricted by `flags`, and the callers give a
    // buffer of the `size` bytes they promise.
    let status = unsafe { RegGetValueW(root, key.as_ptr(), value.as_ptr(), flags, std::ptr::null_mut(), data, size) };
    status == ERROR_SUCCESS
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
