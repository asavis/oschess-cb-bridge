//! A file of the bridge's own, mapped read-only and whole (#145): with `mmap`
//! on Unix and `MapViewOfFile` on Windows. Only files the bridge writes are
//! mapped, never a database's: ChessBase truncates its own files, which a
//! mapped view would make fail on Windows and turn into a fault on Unix. The
//! bridge never writes a file it maps: it writes each under another name and
//! renames it into place.

use std::fs::File;

/// A read-only view of a whole file, unmapped when dropped. Its pages are the
/// operating system's file cache, which may drop them when memory is short
/// and reads them again when they are touched.
pub struct Map {
    ptr: *const u8,
    len: usize,
}

// SAFETY: the view is read-only memory that nothing in the process writes, so
// it may be read from any thread, and unmapped from any thread once no
// reference into it remains, which `Drop` guarantees.
unsafe impl Send for Map {}
unsafe impl Sync for Map {}

impl Map {
    /// The first `len` bytes of `file`, all of which it must hold. The view
    /// outlives `file`.
    pub fn new(file: &File, len: usize) -> std::io::Result<Map> {
        if len == 0 {
            return Ok(Map { ptr: std::ptr::NonNull::dangling().as_ptr(), len });
        }
        Ok(Map { ptr: map(file, len)?, len })
    }

    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `ptr` is the start of `len` readable bytes, mapped until
        // `self` is dropped (or dangling and aligned for `len` 0), and no one
        // writes them: see the module's comment.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        if self.len > 0 {
            unmap(self.ptr, self.len);
        }
    }
}

#[cfg(unix)]
fn map(file: &File, len: usize) -> std::io::Result<*const u8> {
    use std::os::fd::AsRawFd;
    // SAFETY: a new read-only shared mapping of an open descriptor at an
    // address the kernel picks; the result is checked before it is used, and
    // the mapping keeps the file open once the descriptor closes.
    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, file.as_raw_fd(), 0) };
    if ptr == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error());
    }
    Ok(ptr.cast_const().cast())
}

#[cfg(unix)]
fn unmap(ptr: *const u8, len: usize) {
    // SAFETY: `ptr` and `len` are a mapping `map` made, unmapped once.
    unsafe { libc::munmap(ptr.cast_mut().cast(), len) };
}

#[cfg(windows)]
fn map(file: &File, len: usize) -> std::io::Result<*const u8> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_READ, MapViewOfFile, PAGE_READONLY};
    // SAFETY: `file` holds its handle open for the call; the mapping is
    // read-only, of the file's whole length, unnamed and with the default
    // security; the result is checked before it is used.
    let mapping =
        unsafe { CreateFileMappingW(file.as_raw_handle(), std::ptr::null(), PAGE_READONLY, 0, 0, std::ptr::null()) };
    if mapping.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `mapping` was just created; the view is of `len` bytes from the
    // file's start, which the file holds, and it keeps the mapping open once
    // the mapping's handle is closed.
    let view = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, len) };
    let error = std::io::Error::last_os_error();
    // SAFETY: the handle was opened above and is closed once.
    unsafe { CloseHandle(mapping) };
    if view.Value.is_null() {
        return Err(error);
    }
    Ok(view.Value.cast_const().cast())
}

#[cfg(windows)]
fn unmap(ptr: *const u8, _len: usize) {
    use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
    // SAFETY: `ptr` is the start of a view `map` made, unmapped once.
    unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: ptr.cast_mut().cast() }) };
}
