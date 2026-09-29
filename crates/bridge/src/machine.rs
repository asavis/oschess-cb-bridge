//! What the bridge asks of the computer for the work it does in the
//! background (#149): the priority of a thread, whether the computer runs on
//! battery, and the free space of a disk. Only Windows answers. Elsewhere a
//! thread keeps its priority, the computer is taken as running on mains
//! power, and the free space as unknown, which no build waits for.

use std::cell::Cell;
use std::path::Path;

/// The priority a thread runs at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Priority {
    #[default]
    Normal = 0,
    /// Work a request waits for, below the browser the answer goes to, as
    /// the engine's process runs (`engine.rs`).
    BelowNormal = 1,
    /// Work nothing waits for: Windows's background mode, which lowers the
    /// thread's processor, disk and memory priority.
    Background = 2,
}

impl Priority {
    /// The priority of `code`, as [`Priority`]`as u8` gives it; `Normal` for
    /// any other.
    pub fn from_code(code: u8) -> Priority {
        match code {
            1 => Priority::BelowNormal,
            2 => Priority::Background,
            _ => Priority::Normal,
        }
    }
}

thread_local! {
    /// The priority the calling thread runs at, as [`follow`] set it.
    static CURRENT: Cell<Priority> = const { Cell::new(Priority::Normal) };
}

/// The priority the calling thread runs at: what [`follow`] set last, which
/// the workers a pass starts from it take ([`crate::search::workers::run`]).
pub fn current() -> Priority {
    CURRENT.get()
}

/// Runs the calling thread at `priority` from now on. The operating system is
/// asked only when the priority changes.
pub fn follow(priority: Priority) {
    let was = CURRENT.get();
    if was != priority {
        os::set(was, priority);
        CURRENT.set(priority);
    }
}

/// Runs the calling thread at `priority` until the guard is dropped, then at
/// the priority it had before.
pub fn at(priority: Priority) -> At {
    let was = CURRENT.get();
    follow(priority);
    At(was)
}

/// The priority to go back to.
pub struct At(Priority);

impl Drop for At {
    fn drop(&mut self) {
        follow(self.0);
    }
}

/// How the bridge sees the computer. [`System`] is the real one; tests stand
/// in their own.
pub trait Machine: Send + Sync {
    /// Whether the computer runs on battery now.
    fn on_battery(&self) -> bool;

    /// The bytes free to this user on the disk of `dir`, or of the nearest
    /// folder above it that exists; `None` when unknown.
    fn free_bytes(&self, dir: &Path) -> Option<u64>;
}

/// The computer the bridge runs on.
pub struct System;

impl Machine for System {
    fn on_battery(&self) -> bool {
        os::on_battery()
    }

    fn free_bytes(&self, dir: &Path) -> Option<u64> {
        os::free_bytes(dir.ancestors().find(|d| d.is_dir())?)
    }
}

#[cfg(windows)]
mod os {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN, THREAD_MODE_BACKGROUND_END,
        THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_NORMAL,
    };

    use super::Priority;

    /// Moves the calling thread from `from` to `to`. Background mode is left
    /// before another priority is set, and entered from the normal one, which
    /// it goes back to when left.
    pub fn set(from: Priority, to: Priority) {
        // SAFETY: the pseudo-handle of the calling thread needs no closing,
        // and the calls take nothing else.
        unsafe {
            let thread = GetCurrentThread();
            if from == Priority::Background {
                SetThreadPriority(thread, THREAD_MODE_BACKGROUND_END);
            }
            match to {
                Priority::Normal => SetThreadPriority(thread, THREAD_PRIORITY_NORMAL),
                Priority::BelowNormal => SetThreadPriority(thread, THREAD_PRIORITY_BELOW_NORMAL),
                Priority::Background => {
                    SetThreadPriority(thread, THREAD_PRIORITY_NORMAL);
                    SetThreadPriority(thread, THREAD_MODE_BACKGROUND_BEGIN)
                }
            };
        }
    }

    /// Windows says the computer is off mains power; not when it cannot say.
    pub fn on_battery() -> bool {
        let mut status = SYSTEM_POWER_STATUS::default();
        // SAFETY: `status` is a valid, writable SYSTEM_POWER_STATUS.
        let ok = unsafe { GetSystemPowerStatus(&mut status) } != 0;
        ok && status.ACLineStatus == 0
    }

    pub fn free_bytes(dir: &Path) -> Option<u64> {
        // With a separator at its end, as the root of a network share needs.
        let mut name: Vec<u16> = dir.as_os_str().encode_wide().collect();
        if name.last().is_some_and(|&c| c != u16::from(b'\\') && c != u16::from(b'/')) {
            name.push(u16::from(b'\\'));
        }
        name.push(0);
        let mut free = 0u64;
        // SAFETY: `name` ends with a NUL, and `free` is writable; the totals
        // not asked for may be null.
        let ok = unsafe { GetDiskFreeSpaceExW(name.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
        (ok != 0).then_some(free)
    }
}

#[cfg(not(windows))]
mod os {
    use std::path::Path;

    use super::Priority;

    pub fn set(_: Priority, _: Priority) {}

    pub fn on_battery() -> bool {
        false
    }

    pub fn free_bytes(_: &Path) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A thread's priority follows what it was last set to, a guard gives
    /// back the one before it, and a new thread starts at the normal one.
    #[test]
    fn a_thread_keeps_the_priority_it_was_set_to() {
        assert_eq!(current(), Priority::Normal);
        {
            let _below = at(Priority::BelowNormal);
            assert_eq!(current(), Priority::BelowNormal);
            {
                let _background = at(Priority::Background);
                assert_eq!(current(), Priority::Background);
                std::thread::spawn(|| assert_eq!(current(), Priority::Normal)).join().unwrap();
            }
            assert_eq!(current(), Priority::BelowNormal);
            follow(Priority::Background);
            assert_eq!(current(), Priority::Background);
        }
        assert_eq!(current(), Priority::Normal);
        for p in [Priority::Normal, Priority::BelowNormal, Priority::Background] {
            assert_eq!(Priority::from_code(p as u8), p);
        }
        assert_eq!(Priority::from_code(9), Priority::Normal);
    }

    /// The free space is asked of the nearest folder that exists.
    #[test]
    fn free_space_is_asked_of_a_folder_that_exists() {
        let missing = std::env::temp_dir().join(format!("bridge-machine-{}", std::process::id())).join("a").join("b");
        assert!(!missing.exists());
        let known = os::free_bytes(&std::env::temp_dir()).is_some();
        assert_eq!(System.free_bytes(&missing).is_some(), known);
        assert_eq!(known, cfg!(windows), "only Windows answers");
    }

    /// The calls Windows answers (#149): background mode entered and left, a
    /// priority below normal set and reset, the power status and a disk's
    /// free space read.
    #[cfg(windows)]
    #[test]
    fn windows_answers_each_call() {
        use windows_sys::Win32::Foundation::{
            ERROR_THREAD_MODE_ALREADY_BACKGROUND, ERROR_THREAD_MODE_NOT_BACKGROUND, GetLastError,
        };
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, GetThreadPriority, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
            THREAD_MODE_BACKGROUND_END, THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_NORMAL,
        };

        std::thread::spawn(|| {
            // The error of setting `mode` again, 0 when it was set. SAFETY:
            // calls on the calling thread's pseudo-handle.
            let again =
                |mode| unsafe { if SetThreadPriority(GetCurrentThread(), mode) == 0 { GetLastError() } else { 0 } };
            // SAFETY: as above.
            let priority = || unsafe { GetThreadPriority(GetCurrentThread()) };
            {
                let _background = at(Priority::Background);
                assert_eq!(again(THREAD_MODE_BACKGROUND_BEGIN), ERROR_THREAD_MODE_ALREADY_BACKGROUND);
            }
            assert_eq!(again(THREAD_MODE_BACKGROUND_END), ERROR_THREAD_MODE_NOT_BACKGROUND);
            assert_eq!(priority(), THREAD_PRIORITY_NORMAL);
            {
                let _below = at(Priority::BelowNormal);
                assert_eq!(priority(), THREAD_PRIORITY_BELOW_NORMAL);
                follow(Priority::Background);
                assert_eq!(again(THREAD_MODE_BACKGROUND_BEGIN), ERROR_THREAD_MODE_ALREADY_BACKGROUND);
                follow(Priority::BelowNormal);
                assert_eq!(again(THREAD_MODE_BACKGROUND_END), ERROR_THREAD_MODE_NOT_BACKGROUND);
                assert_eq!(priority(), THREAD_PRIORITY_BELOW_NORMAL);
            }
            assert_eq!(priority(), THREAD_PRIORITY_NORMAL);
        })
        .join()
        .unwrap();
        // Either answer is right; the call must not fail.
        let _ = System.on_battery();
        for dir in [std::env::temp_dir(), std::env::temp_dir().join("bridge-machine-none").join("a")] {
            let free = System.free_bytes(&dir);
            assert!(free.is_some_and(|b| b > 0), "{free:?}");
        }
    }
}
