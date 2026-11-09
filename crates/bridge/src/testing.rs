//! What the library's tests share (#381). An integration test cannot use
//! this and has `tests/common` instead. A stand-in with a behaviour of its
//! own, one that panics or counts looks, stays in the test that needs it.

use std::fs::Metadata;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::fetch::Cloud;
use crate::machine::Machine;

/// How often [`until`] asks, and a held download looks whether it may go on.
const POLL: Duration = Duration::from_millis(5);

/// Waits until `done` holds, for `limit` at most; then the test fails with
/// `what`.
#[track_caller]
pub(crate) fn until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < deadline, "{what} (waited {limit:?})");
        std::thread::sleep(POLL);
    }
}

/// A computer whose power the test sets: on mains power unless set, and of
/// unknown free disk space.
#[derive(Default)]
pub(crate) struct Power(pub(crate) AtomicBool);

impl Machine for Power {
    fn on_battery(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    fn free_bytes(&self, _: &Path) -> Option<u64> {
        None
    }
}

/// A cloud provider that keeps every file in the cloud and holds a download
/// until [`HeldDownload::release`]; the file is then on this computer.
pub(crate) struct HeldDownload {
    cloud: AtomicBool,
    released: AtomicBool,
}

impl HeldDownload {
    pub(crate) fn new() -> HeldDownload {
        HeldDownload { cloud: AtomicBool::new(true), released: AtomicBool::new(false) }
    }

    pub(crate) fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
    }
}

impl Cloud for HeldDownload {
    fn is_cloud_only(&self, _: &Path, _: &Metadata) -> bool {
        self.cloud.load(Ordering::SeqCst)
    }

    fn fetch(&self, _: &Path, _: &mut dyn FnMut(u64)) -> std::io::Result<()> {
        while !self.released.load(Ordering::SeqCst) {
            std::thread::sleep(POLL);
        }
        self.cloud.store(false, Ordering::SeqCst);
        Ok(())
    }
}
