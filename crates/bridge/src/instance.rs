//! One bridge per signed-in user. The first start holds a lock. A later start
//! signals the running bridge and exits; what the running bridge does with the
//! signal is up to its caller (today it opens oschess in the browser).

use std::time::Duration;

/// The lock: a mutex in the session's `Local\` namespace, so each signed-in
/// user runs their own bridge.
pub const LOCK_NAME: &str = r"Local\oschess-bridge";
/// The auto-reset event a later start sets to signal the running bridge.
pub const SIGNAL_NAME: &str = r"Local\oschess-bridge-signal";

/// How often, and how far apart, a later start tries to signal: a bridge
/// started a moment earlier may hold the lock and not yet have its event.
pub const SIGNAL_ATTEMPTS: u32 = 25;
pub const SIGNAL_PAUSE: Duration = Duration::from_millis(200);

/// What the operating system offers to tell starts apart.
pub trait Instances {
    /// Takes the lock; false when another start holds it.
    fn claim(&mut self) -> bool;
    /// Signals the bridge that holds the lock; false when it cannot yet be
    /// signalled.
    fn signal(&mut self) -> bool;
    fn pause(&mut self);
}

#[derive(Debug, PartialEq, Eq)]
pub enum Start {
    /// This is the only bridge: run.
    First,
    /// The running bridge was signalled: exit.
    Signalled,
    /// Another start holds the lock but could not be signalled: exit and say so.
    Unanswered,
}

pub fn start(instances: &mut impl Instances) -> Start {
    if instances.claim() {
        return Start::First;
    }
    for attempt in 1..=SIGNAL_ATTEMPTS {
        if instances.signal() {
            return Start::Signalled;
        }
        if attempt < SIGNAL_ATTEMPTS {
            instances.pause();
        }
    }
    Start::Unanswered
}

#[cfg(windows)]
pub use windows::Session;

#[cfg(windows)]
mod windows {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};

    use crate::wide;

    use super::{Instances, LOCK_NAME, SIGNAL_NAME, SIGNAL_PAUSE};

    /// The Win32 side of [`Instances`]. It keeps the lock until dropped, which
    /// is when the process ends.
    #[derive(Default)]
    pub struct Session {
        lock: Option<HANDLE>,
        signal: Option<HANDLE>,
    }

    /// An event handle moved to the thread that waits on it.
    struct Event(HANDLE);

    // SAFETY: a kernel handle is a process-wide value that any thread may wait
    // on; the thread that receives it is its only user and never closes it.
    unsafe impl Send for Event {}

    impl Session {
        /// Calls `on_signal` on a thread of its own each time a later start
        /// signals this bridge. Only the start that took the lock can listen.
        pub fn listen(&mut self, mut on_signal: impl FnMut() + Send + 'static) -> Result<(), String> {
            let event = Event(self.signal.take().ok_or("this start holds no signal event")?);
            let thread = std::thread::Builder::new().name("bridge-signal".into()).spawn(move || {
                let event = event;
                // SAFETY: `event.0` is a valid event handle, kept open for the
                // life of the process, so the wait never outlives it.
                while unsafe { WaitForSingleObject(event.0, INFINITE) } == WAIT_OBJECT_0 {
                    on_signal();
                }
            });
            thread.map(drop).map_err(|e| format!("signal thread: {e}"))
        }
    }

    impl Instances for Session {
        fn claim(&mut self) -> bool {
            let name = wide(LOCK_NAME);
            // SAFETY: default security, not owned, and a NUL-terminated name
            // that outlives the call.
            let lock = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
            // SAFETY: reads the calling thread's last error, set by CreateMutexW.
            let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
            if existed {
                // SAFETY: `lock` is the handle just returned, closed once.
                unsafe { CloseHandle(lock) };
                return false;
            }
            // Without a lock (a null handle) the port, bound next, still keeps
            // a second bridge from serving.
            self.lock = (!lock.is_null()).then_some(lock);
            let name = wide(SIGNAL_NAME);
            // SAFETY: default security, auto-reset, not signalled, and a
            // NUL-terminated name that outlives the call.
            let signal = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
            self.signal = (!signal.is_null()).then_some(signal);
            true
        }

        fn signal(&mut self) -> bool {
            let name = wide(SIGNAL_NAME);
            // SAFETY: a NUL-terminated name that outlives the call.
            let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
            if event.is_null() {
                return false;
            }
            // SAFETY: `event` is the handle just opened, closed once. The user
            // started this process, so it may hand on the right to bring a
            // window to the front: the running bridge may open the browser.
            unsafe {
                AllowSetForegroundWindow(ASFW_ANY);
                let set = SetEvent(event) != 0;
                CloseHandle(event);
                set
            }
        }

        fn pause(&mut self) {
            std::thread::sleep(SIGNAL_PAUSE);
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            for handle in [self.lock.take(), self.signal.take()].into_iter().flatten() {
                // SAFETY: a handle this session opened and still owns, closed once.
                unsafe { CloseHandle(handle) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lock held or free, and a running bridge that can be signalled after
    /// `ready_after` failed attempts.
    struct Fake {
        held: bool,
        ready_after: Option<u32>,
        signals: u32,
        pauses: u32,
    }

    impl Instances for Fake {
        fn claim(&mut self) -> bool {
            !std::mem::replace(&mut self.held, true)
        }
        fn signal(&mut self) -> bool {
            self.signals += 1;
            self.ready_after.is_some_and(|n| self.signals > n)
        }
        fn pause(&mut self) {
            self.pauses += 1;
        }
    }

    fn fake(held: bool, ready_after: Option<u32>) -> Fake {
        Fake { held, ready_after, signals: 0, pauses: 0 }
    }

    #[test]
    fn the_first_start_runs_without_signalling() {
        let mut f = fake(false, Some(0));
        assert_eq!(start(&mut f), Start::First);
        assert_eq!((f.signals, f.pauses), (0, 0));
        assert_eq!(start(&mut f), Start::Signalled, "the lock stays taken");
    }

    #[test]
    fn a_later_start_signals_the_running_one() {
        let mut f = fake(true, Some(0));
        assert_eq!(start(&mut f), Start::Signalled);
        assert_eq!((f.signals, f.pauses), (1, 0));
    }

    #[test]
    fn a_later_start_waits_for_a_bridge_still_starting() {
        let mut f = fake(true, Some(3));
        assert_eq!(start(&mut f), Start::Signalled);
        assert_eq!((f.signals, f.pauses), (4, 3));
    }

    #[test]
    fn a_running_bridge_that_cannot_be_signalled_is_reported() {
        let mut f = fake(true, None);
        assert_eq!(start(&mut f), Start::Unanswered);
        assert_eq!((f.signals, f.pauses), (SIGNAL_ATTEMPTS, SIGNAL_ATTEMPTS - 1));
        assert!(SIGNAL_PAUSE * SIGNAL_ATTEMPTS <= Duration::from_secs(10));
    }
}
