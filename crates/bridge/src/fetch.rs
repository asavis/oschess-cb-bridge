//! Cloud-only databases: files a cloud storage provider keeps only as
//! placeholders until they are read (`docs/api.md`, "Cloud-only databases").
//! Listing looks at file attributes and never reads such a file; opening the
//! database for its games reads its files in the background, one database at
//! a time, which makes the provider bring them to this computer.

use std::fs::Metadata;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// How the bridge sees and fetches cloud-only files. [`System`] is the real
/// one; tests stand in their own, since only Windows has placeholders.
pub trait Cloud: Send + Sync {
    /// Whether `path`, whose metadata this is, is a cloud-only placeholder.
    /// Must not read the file.
    fn is_cloud_only(&self, path: &Path, meta: &Metadata) -> bool;

    /// Reads the whole of `path`, passing each count of bytes read to `read`.
    fn fetch(&self, path: &Path, read: &mut dyn FnMut(u64)) -> std::io::Result<()>;
}

/// Windows placeholders, recognised by their file attributes. Other systems
/// have none: there every file is taken as local.
pub struct System;

impl Cloud for System {
    #[cfg(windows)]
    fn is_cloud_only(&self, _: &Path, meta: &Metadata) -> bool {
        use std::os::windows::fs::MetadataExt;
        cbformat::dbitems::is_cloud_only(meta.file_attributes())
    }

    #[cfg(not(windows))]
    fn is_cloud_only(&self, _: &Path, _: &Metadata) -> bool {
        false
    }

    fn fetch(&self, path: &Path, read: &mut dyn FnMut(u64)) -> std::io::Result<()> {
        let mut file = std::fs::File::open(path)?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            match file.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => read(n as u64),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

/// A download's progress: bytes of the database's files on this computer, of
/// `total`.
pub struct Progress {
    present: AtomicU64,
    pub total: u64,
}

impl Progress {
    pub fn new(present: u64, total: u64) -> Progress {
        Progress { present: AtomicU64::new(present), total }
    }

    pub fn present(&self) -> u64 {
        self.present.load(Ordering::Relaxed).min(self.total)
    }

    pub fn add(&self, bytes: u64) {
        self.present.fetch_add(bytes, Ordering::Relaxed);
    }
}
