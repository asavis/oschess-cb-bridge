//! The end that Windows asks for (#340): at sign-out and shutdown, and when
//! Restart Manager closes the bridge for a Store update, every top-level
//! window gets `WM_QUERYENDSESSION` and then `WM_ENDSESSION`. tao 0.35 answers
//! `WM_ENDSESSION` by destroying its event loop in place and keeps
//! dispatching: the next event panics with "cannot move state from Destroyed"
//! (BRIDGE-5), and one that arrives while the event handler runs panics with
//! "either event handler is re-entrant" (BRIDGE-6). Either way the bridge
//! ended with a panic report, and after an update with no clean close for
//! Restart Manager to restart it from.
//!
//! A hook on the event loop's thread sees every message sent to its windows
//! before any window procedure does, tao's included. On `WM_ENDSESSION` for
//! an end that goes ahead, it logs why, removes the tray icon, and exits with
//! 0, as tao 0.37 itself exits after the message.

use std::sync::OnceLock;

use tauri::AppHandle;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CWPSTRUCT, CallNextHookEx, ENDSESSION_CLOSEAPP, ENDSESSION_LOGOFF, HC_ACTION, SetWindowsHookExW, WH_CALLWNDPROC,
    WM_ENDSESSION,
};

/// Why the session ends, from `WM_ENDSESSION`'s `lParam`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// Restart Manager closes the bridge: a Store update or an installer.
    Close,
    SignOut,
    Shutdown,
}

impl Ending {
    fn what(self) -> &'static str {
        match self {
            Ending::Close => "Windows closes the bridge for an update",
            Ending::SignOut => "the user signs out",
            Ending::Shutdown => "Windows shuts down",
        }
    }
}

/// The end `message` announces: `WM_ENDSESSION` with a `wparam` other than 0,
/// an end that goes ahead. `None` for any other message, and for an end that
/// was called off.
pub fn ending(message: u32, wparam: WPARAM, lparam: LPARAM) -> Option<Ending> {
    if message != WM_ENDSESSION || wparam == 0 {
        return None;
    }
    let flags = lparam as u32;
    Some(if flags & ENDSESSION_CLOSEAPP != 0 {
        Ending::Close
    } else if flags & ENDSESSION_LOGOFF != 0 {
        Ending::SignOut
    } else {
        Ending::Shutdown
    })
}

/// What the hook does at an end: [`install`] sets it once.
static END: OnceLock<Box<dyn Fn(Ending) + Send + Sync>> = OnceLock::new();

/// Ends the bridge as Windows asks: the log says why, the tray icon goes, and
/// the process exits with 0, which Restart Manager takes for a clean close.
pub fn start(app: &AppHandle) {
    let app = app.clone();
    install(move |ending| {
        bridge::log!("the bridge ends: {}", ending.what());
        app.cleanup_before_exit();
        std::process::exit(0);
    });
}

/// Hooks the calling thread's sent messages, to run `end` on an end of the
/// session. Called on the event loop's thread, once.
fn install(end: impl Fn(Ending) + Send + Sync + 'static) {
    if END.set(Box::new(end)).is_err() {
        return;
    }
    // SAFETY: a hook procedure of this module for the calling thread; no
    // module handle is needed for a thread of this process.
    let hook =
        unsafe { SetWindowsHookExW(WH_CALLWNDPROC, Some(on_message), std::ptr::null_mut(), GetCurrentThreadId()) };
    if hook.is_null() {
        bridge::log!("the end of the session is not watched: {}", std::io::Error::last_os_error());
    }
}

unsafe extern "system" fn on_message(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && lparam != 0 {
        // SAFETY: for HC_ACTION, `lparam` points to the message's CWPSTRUCT.
        let sent = unsafe { &*(lparam as *const CWPSTRUCT) };
        if let (Some(ending), Some(end)) = (ending(sent.message, sent.wParam, sent.lParam), END.get()) {
            end(ending);
        }
    }
    // SAFETY: passes the message on to the next hook, as every hook must.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, ENDSESSION_CRITICAL, HWND_MESSAGE, MSG,
        PM_REMOVE, PeekMessageW, RegisterClassW, SendMessageW, WM_QUERYENDSESSION, WNDCLASSW,
    };

    use super::*;

    #[test]
    fn an_end_that_goes_ahead_is_told_by_its_cause() {
        let end = |lparam: u32| ending(WM_ENDSESSION, 1, lparam as LPARAM);
        assert_eq!(end(ENDSESSION_CLOSEAPP), Some(Ending::Close));
        assert_eq!(end(ENDSESSION_CLOSEAPP | ENDSESSION_CRITICAL), Some(Ending::Close));
        assert_eq!(end(ENDSESSION_LOGOFF), Some(Ending::SignOut));
        assert_eq!(end(0), Some(Ending::Shutdown));
        assert_eq!(end(ENDSESSION_CRITICAL), Some(Ending::Shutdown));
        assert_eq!(ending(WM_ENDSESSION, 0, ENDSESSION_CLOSEAPP as LPARAM), None, "called off");
        assert_eq!(ending(WM_QUERYENDSESSION, 1, ENDSESSION_CLOSEAPP as LPARAM), None, "only asked");
    }

    static ENDED: Mutex<Vec<Ending>> = Mutex::new(Vec::new());

    unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    /// The hook sees `WM_ENDSESSION` that another thread sends to a window of
    /// the hooked thread, as Windows sends it, before the window does.
    #[test]
    fn the_hook_sees_the_end_another_thread_sends() {
        install(|ending| ENDED.lock().unwrap().push(ending));
        let class: Vec<u16> = "oschess-bridge-session-test\0".encode_utf16().collect();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            lpszClassName: class.as_ptr(),
            ..unsafe { std::mem::zeroed() }
        };
        // SAFETY: a class and a message-only window of this test's own, destroyed at its end.
        let hwnd = unsafe {
            RegisterClassW(&wc);
            CreateWindowExW(
                0,
                class.as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        assert!(!hwnd.is_null());
        let target = hwnd as usize;
        let sender = std::thread::spawn(move || {
            for (wparam, lparam) in [(0, ENDSESSION_LOGOFF), (1, ENDSESSION_CLOSEAPP)] {
                // SAFETY: a window of the test thread, which pumps until both are delivered.
                unsafe { SendMessageW(target as HWND, WM_ENDSESSION, wparam, lparam as LPARAM) };
            }
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !sender.is_finished() && Instant::now() < deadline {
            let mut msg: MSG = unsafe { std::mem::zeroed() };
            // SAFETY: a plain pump of this thread's messages; sent ones are delivered inside it.
            while unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
                unsafe { DispatchMessageW(&msg) };
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        sender.join().unwrap();
        unsafe { DestroyWindow(hwnd) };
        assert_eq!(*ENDED.lock().unwrap(), [Ending::Close], "the end called off is passed over");
    }
}
