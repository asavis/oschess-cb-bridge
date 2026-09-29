//! What the bridge asks of the computer for the work it does in the
//! background (#149): the priority of a thread, whether the computer runs on
//! battery, and the free space of a disk. Only Windows answers. Elsewhere a
//! thread keeps its priority, the computer is taken as running on mains
//! power, and the free space as unknown, which no build waits for.

use std::cell::Cell;
use std::path::Path;
use std::sync::OnceLock;

/// The priority a thread runs at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Priority {
    #[default]
    Normal = 0,
    /// Work a request waits for, below the browser the answer goes to, as
    /// the engine's process runs (`engine.rs`).
    BelowNormal = 1,
    /// Work nothing waits for, in Windows's background mode, which lowers the
    /// thread's processor, disk and memory priority.
    Background = 2,
    /// Work nothing waits for, at the lowest processor priority of the
    /// process's class, below work a request waits for and the engine, with
    /// the disk and memory priority of any other thread.
    Lowest = 3,
}

impl Priority {
    /// The priority of `code`, as [`Priority`]`as u8` gives it; `Normal` for
    /// any other.
    pub fn from_code(code: u8) -> Priority {
        match code {
            1 => Priority::BelowNormal,
            2 => Priority::Background,
            3 => Priority::Lowest,
            _ => Priority::Normal,
        }
    }
}

/// The variable that chooses the priority of the work nothing waits for,
/// the index builds the keeper queues (#149): one of the names of
/// [`MODES`]. It is read once, when a bridge first starts its keeper; any
/// other value, or none, takes the first.
pub const BACKGROUND_MODE: &str = "OSCHESS_BRIDGE_BACKGROUND_MODE";

/// The priorities work nothing waits for may run at, by the name
/// [`BACKGROUND_MODE`] gives them, the default first:
/// - `lowcpu`: the lowest processor priority of the process's class, below
///   requested builds and the engine, which run below normal, with the normal
///   disk and memory priority. Not the idle priority, which runs behind every
///   other program's work: on a busy computer a build would wait for idle
///   moments, and a search waiting for a lock that a build's thread holds,
///   such as the search budget's or a database file's handles, with it.
/// - `background`: Windows's background mode, which lowers the disk and
///   memory priority too, so that the build's gigabytes of reads and writes
///   give way to other programs' and take longer.
pub const MODES: [(&str, Priority); 2] = [("lowcpu", Priority::Lowest), ("background", Priority::Background)];

/// The mode of work nothing waits for, as [`BACKGROUND_MODE`] chose it when
/// first asked: its name and its priority.
pub fn background_mode() -> (&'static str, Priority) {
    static MODE: OnceLock<(&str, Priority)> = OnceLock::new();
    *MODE.get_or_init(|| mode_of(std::env::var(BACKGROUND_MODE).ok().as_deref()))
}

/// The priority of work nothing waits for ([`background_mode`]).
pub fn background() -> Priority {
    background_mode().1
}

/// The mode of work nothing waits for that `value` of [`BACKGROUND_MODE`]
/// names.
fn mode_of(value: Option<&str>) -> (&'static str, Priority) {
    let value = value.map(str::trim).unwrap_or_default();
    MODES.into_iter().find(|(name, _)| name.eq_ignore_ascii_case(value)).unwrap_or(MODES[0])
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
        for call in calls(was, priority) {
            make(call);
        }
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

/// What a thread asks Windows of its own priority: `SetThreadPriority` with
/// the value each names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    /// `THREAD_MODE_BACKGROUND_END`
    LeaveBackground,
    /// `THREAD_PRIORITY_NORMAL`
    Normal,
    /// `THREAD_PRIORITY_BELOW_NORMAL`
    BelowNormal,
    /// `THREAD_PRIORITY_LOWEST`
    Lowest,
    /// `THREAD_MODE_BACKGROUND_BEGIN`
    EnterBackground,
}

/// The calls that move a thread from `from` to `to`, in order. Background
/// mode is left before another priority is set, and entered from the normal
/// one, which it goes back to when left.
fn calls(from: Priority, to: Priority) -> impl Iterator<Item = Call> {
    let leave = (from == Priority::Background).then_some(Call::LeaveBackground);
    let set = match to {
        Priority::Normal | Priority::Background => Call::Normal,
        Priority::BelowNormal => Call::BelowNormal,
        Priority::Lowest => Call::Lowest,
    };
    let enter = (to == Priority::Background).then_some(Call::EnterBackground);
    leave.into_iter().chain([set]).chain(enter)
}

/// Makes `call` for the calling thread; tests see each call made.
fn make(call: Call) {
    #[cfg(test)]
    tests::MADE.with_borrow_mut(|made| made.push(call));
    os::call(call);
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
        THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_LOWEST, THREAD_PRIORITY_NORMAL,
    };

    use super::Call;

    pub fn call(call: Call) {
        let value = match call {
            Call::LeaveBackground => THREAD_MODE_BACKGROUND_END,
            Call::Normal => THREAD_PRIORITY_NORMAL,
            Call::BelowNormal => THREAD_PRIORITY_BELOW_NORMAL,
            Call::Lowest => THREAD_PRIORITY_LOWEST,
            Call::EnterBackground => THREAD_MODE_BACKGROUND_BEGIN,
        };
        // SAFETY: the pseudo-handle of the calling thread needs no closing,
        // and the call takes nothing else.
        unsafe { SetThreadPriority(GetCurrentThread(), value) };
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

    use super::Call;

    pub fn call(_: Call) {}

    pub fn on_battery() -> bool {
        false
    }

    pub fn free_bytes(_: &Path) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    const ALL: [Priority; 4] = [Priority::Normal, Priority::BelowNormal, Priority::Background, Priority::Lowest];

    thread_local! {
        /// The calls the thread made, as [`make`] made them.
        pub(super) static MADE: RefCell<Vec<Call>> = const { RefCell::new(Vec::new()) };
    }

    /// The calls the thread made since it last asked.
    fn made() -> Vec<Call> {
        MADE.take()
    }

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
            follow(Priority::Lowest);
            assert_eq!(current(), Priority::Lowest);
        }
        assert_eq!(current(), Priority::Normal);
        for p in ALL {
            assert_eq!(Priority::from_code(p as u8), p);
        }
        assert_eq!(Priority::from_code(9), Priority::Normal);
    }

    /// The variable names either mode, the low processor priority by
    /// default and for any other value.
    #[test]
    fn the_variable_chooses_the_background_mode() {
        let (lowcpu, background) = (("lowcpu", Priority::Lowest), ("background", Priority::Background));
        assert_eq!(mode_of(None), lowcpu);
        assert_eq!(mode_of(Some("lowcpu")), lowcpu);
        assert_eq!(mode_of(Some(" Background ")), background);
        assert_eq!(mode_of(Some("idle")), lowcpu);
        assert_eq!(mode_of(Some("")), lowcpu);
        assert!(MODES.contains(&background_mode()));
    }

    /// A thread's state as Windows keeps it: its priority, and whether it
    /// is in background mode.
    type State = (Call, bool);

    /// The state `calls` leave a thread in from `state`; `None` when one is
    /// refused (background mode entered or left twice) or sets a priority
    /// in background mode, which leaving it would undo.
    fn after(mut state: State, calls: impl IntoIterator<Item = Call>) -> Option<State> {
        for call in calls {
            state = match (call, state.1) {
                (Call::EnterBackground, false) => (state.0, true),
                (Call::LeaveBackground, true) => (state.0, false),
                (Call::EnterBackground | Call::LeaveBackground, _) | (_, true) => return None,
                (level, false) => (level, false),
            };
        }
        Some(state)
    }

    /// The calls that move a thread from any priority to any other leave it
    /// in the state of the new one, as Windows answers each (#149): the low
    /// processor priority in neither background mode nor at the normal
    /// priority, background mode entered from the normal priority, and left
    /// before any other is set.
    #[test]
    fn each_priority_is_reached_from_any_other() {
        let normal = (Call::Normal, false);
        let state = |p: Priority| match p {
            Priority::Normal => normal,
            Priority::BelowNormal => (Call::BelowNormal, false),
            Priority::Lowest => (Call::Lowest, false),
            Priority::Background => (Call::Normal, true),
        };
        for from in ALL {
            for to in ALL.into_iter().filter(|&to| to != from) {
                let reached = after(normal, calls(Priority::Normal, from)).and_then(|s| after(s, calls(from, to)));
                assert_eq!(reached, Some(state(to)), "{from:?} to {to:?}");
            }
        }
    }

    /// The calls a build makes in each background mode (#149): it runs at
    /// the mode's priority, a request raises it below normal, and its end
    /// gives back the normal one; a thread asked again for its priority
    /// makes no call.
    #[test]
    fn a_build_makes_the_calls_of_its_mode() {
        std::thread::spawn(|| {
            let expected = [
                (Priority::Lowest, vec![Call::Lowest], vec![Call::BelowNormal]),
                (
                    Priority::Background,
                    vec![Call::Normal, Call::EnterBackground],
                    vec![Call::LeaveBackground, Call::BelowNormal],
                ),
            ];
            for (mode, enter, raise) in expected {
                made();
                {
                    let _build = at(mode);
                    assert_eq!(made(), enter, "{mode:?}");
                    follow(mode);
                    assert_eq!(made(), [], "{mode:?}");
                    follow(Priority::BelowNormal);
                    assert_eq!(made(), raise, "{mode:?}");
                }
                assert_eq!(made(), [Call::Normal], "{mode:?}");
            }
        })
        .join()
        .unwrap();
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

    /// The calls Windows answers (#149): background mode entered and left,
    /// with the thread's memory and disk priority lowered in it alone; the
    /// lowest priority and one below normal set and reset; the power status
    /// and a disk's free space read.
    #[cfg(windows)]
    #[test]
    fn windows_answers_each_call() {
        use windows_sys::Win32::Foundation::{
            ERROR_THREAD_MODE_ALREADY_BACKGROUND, ERROR_THREAD_MODE_NOT_BACKGROUND, GetLastError, HANDLE,
        };
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, GetThreadInformation, GetThreadPriority, MEMORY_PRIORITY_INFORMATION,
            MEMORY_PRIORITY_NORMAL, MEMORY_PRIORITY_VERY_LOW, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
            THREAD_MODE_BACKGROUND_END, THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_LOWEST, THREAD_PRIORITY_NORMAL,
            ThreadMemoryPriority,
        };

        #[link(name = "ntdll")]
        unsafe extern "system" {
            fn NtQueryInformationThread(
                thread: HANDLE,
                class: u32,
                information: *mut std::ffi::c_void,
                length: u32,
                returned: *mut u32,
            ) -> i32;
        }
        /// `ThreadIoPriority`, whose answer is an `IO_PRIORITY_HINT`.
        const THREAD_IO_PRIORITY: u32 = 22;
        const IO_PRIORITY_VERY_LOW: u32 = 0;
        const IO_PRIORITY_NORMAL: u32 = 2;

        std::thread::spawn(|| {
            // The error of setting `mode` again, 0 when it was set. SAFETY:
            // calls on the calling thread's pseudo-handle.
            let again =
                |mode| unsafe { if SetThreadPriority(GetCurrentThread(), mode) == 0 { GetLastError() } else { 0 } };
            // SAFETY: as above.
            let priority = || unsafe { GetThreadPriority(GetCurrentThread()) };
            // The thread's memory and disk priority. SAFETY: as above, each
            // answer written into room of its size.
            let memory = || unsafe {
                let mut info = MEMORY_PRIORITY_INFORMATION::default();
                let size = size_of::<MEMORY_PRIORITY_INFORMATION>() as u32;
                let ok = GetThreadInformation(GetCurrentThread(), ThreadMemoryPriority, (&raw mut info).cast(), size);
                assert_ne!(ok, 0, "memory priority");
                info.MemoryPriority
            };
            let disk = || unsafe {
                let mut hint = u32::MAX;
                let status = NtQueryInformationThread(
                    GetCurrentThread(),
                    THREAD_IO_PRIORITY,
                    (&raw mut hint).cast(),
                    4,
                    std::ptr::null_mut(),
                );
                assert_eq!(status, 0, "disk priority");
                hint
            };
            assert_eq!((memory(), disk()), (MEMORY_PRIORITY_NORMAL, IO_PRIORITY_NORMAL));
            {
                let _background = at(Priority::Background);
                assert_eq!(again(THREAD_MODE_BACKGROUND_BEGIN), ERROR_THREAD_MODE_ALREADY_BACKGROUND);
                assert_eq!((memory(), disk()), (MEMORY_PRIORITY_VERY_LOW, IO_PRIORITY_VERY_LOW));
            }
            assert_eq!(again(THREAD_MODE_BACKGROUND_END), ERROR_THREAD_MODE_NOT_BACKGROUND);
            assert_eq!(priority(), THREAD_PRIORITY_NORMAL);
            assert_eq!((memory(), disk()), (MEMORY_PRIORITY_NORMAL, IO_PRIORITY_NORMAL));
            {
                let _lowcpu = at(Priority::Lowest);
                assert_eq!(priority(), THREAD_PRIORITY_LOWEST);
                assert_eq!(again(THREAD_MODE_BACKGROUND_END), ERROR_THREAD_MODE_NOT_BACKGROUND);
                assert_eq!((memory(), disk()), (MEMORY_PRIORITY_NORMAL, IO_PRIORITY_NORMAL));
                follow(Priority::Background);
                assert_eq!(again(THREAD_MODE_BACKGROUND_BEGIN), ERROR_THREAD_MODE_ALREADY_BACKGROUND);
                follow(Priority::Lowest);
                assert_eq!(again(THREAD_MODE_BACKGROUND_END), ERROR_THREAD_MODE_NOT_BACKGROUND);
                assert_eq!(priority(), THREAD_PRIORITY_LOWEST);
                assert_eq!((memory(), disk()), (MEMORY_PRIORITY_NORMAL, IO_PRIORITY_NORMAL));
                follow(Priority::BelowNormal);
                assert_eq!(priority(), THREAD_PRIORITY_BELOW_NORMAL);
            }
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
            assert_eq!((memory(), disk()), (MEMORY_PRIORITY_NORMAL, IO_PRIORITY_NORMAL));
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
