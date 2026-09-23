// The Windows app has no console; errors reach the user through the tray.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    app::desktop::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("The oschess bridge app runs on Windows only. Elsewhere, run `cbtool bridge` or `oschess-bridge`.");
    std::process::exit(1);
}
