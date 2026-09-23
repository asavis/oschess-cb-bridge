//! Opening a page in the user's default browser, on Windows.

use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::wide;

/// Opens `url` in the default browser; false when Windows could not.
pub fn open(url: &str) -> bool {
    let (verb, url) = (wide("open"), wide(url));
    // SAFETY: NUL-terminated strings that outlive the call; no owner window,
    // parameters or working folder.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            url.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW reports success as a value above 32.
    result as isize > 32
}
