//! The package identity Windows gives the app when the Microsoft Store
//! installed it (#112); none for the NSIS installation.

use std::path::PathBuf;

use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};
use windows_sys::Win32::Storage::Packaging::Appx::{GetCurrentPackageFamilyName, GetCurrentPackagePath};

use crate::channel::{Channel, Package};

/// The channel this process runs in.
pub fn channel() -> Channel {
    // SAFETY: each call gets a valid length pointer and a buffer of that length.
    let family = wide(|len, buf| unsafe { GetCurrentPackageFamilyName(len, buf) });
    // SAFETY: as above.
    let install = wide(|len, buf| unsafe { GetCurrentPackagePath(len, buf) });
    match (family, install) {
        (Some(family), Some(install)) => Channel::Store(Package { install: PathBuf::from(install), family }),
        _ => Channel::Direct,
    }
}

/// A string from a Windows call that first answers the length it needs; none
/// when the process has no package identity.
fn wide(read: impl Fn(*mut u32, *mut u16) -> u32) -> Option<String> {
    let mut len = 0u32;
    if read(&mut len, std::ptr::null_mut()) != ERROR_INSUFFICIENT_BUFFER {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    if read(&mut len, buf.as_mut_ptr()) != ERROR_SUCCESS {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16(&buf[..end]).ok().filter(|s| !s.is_empty())
}
