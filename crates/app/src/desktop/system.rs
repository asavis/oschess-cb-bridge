//! What the app asks Windows directly: the display language and the
//! taskbar's theme.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};

use crate::status::Theme;

/// The user's display language as a Windows language identifier.
pub fn display_language() -> u16 {
    // SAFETY: the function takes no arguments and only returns a value.
    unsafe { GetUserDefaultUILanguage() }
}

/// The taskbar's theme: `SystemUsesLightTheme` in the user's personalisation
/// settings. Without the value, as on older Windows 10, the taskbar is dark.
pub fn taskbar_theme() -> Theme {
    let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let value = wide("SystemUsesLightTheme");
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: both names are NUL-terminated UTF-16 strings that outlive the
    // call, the value type is restricted to a DWORD, and `data` has the four
    // bytes `size` promises.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast::<c_void>(),
            &mut size,
        )
    };
    if status == ERROR_SUCCESS && data == 1 { Theme::Light } else { Theme::Dark }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
