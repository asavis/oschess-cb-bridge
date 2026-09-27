//! The package identity Windows gives the app when the Microsoft Store
//! installed it (#112); none for the NSIS installation.

use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
use windows_sys::Win32::Storage::Packaging::Appx::GetCurrentPackageFamilyName;

use crate::channel::Channel;

/// The channel this process runs in. A process without a package identity
/// answers `APPMODEL_ERROR_NO_PACKAGE`; a packaged one asks for room for its
/// family name.
pub fn channel() -> Channel {
    let mut len = 0u32;
    // SAFETY: a valid length pointer and no buffer, which only asks the length.
    let answer = unsafe { GetCurrentPackageFamilyName(&mut len, std::ptr::null_mut()) };
    if answer == ERROR_INSUFFICIENT_BUFFER { Channel::Store } else { Channel::Direct }
}
