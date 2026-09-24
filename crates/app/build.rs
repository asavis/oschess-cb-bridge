//! Tauri's build step, for Windows targets only. A build script cannot tell a
//! Windows target by `cfg` (it is compiled for the machine that builds), so it
//! asks CARGO_CFG_TARGET_OS.

/// The app's commands. Tauri makes an `allow-<command>` permission for each,
/// and each window's file in `capabilities/` allows only those it needs.
const COMMANDS: &[&str] = &[
    "view",
    "open_oschess",
    "open_settings",
    "hide_flyout",
    "fit_flyout",
    "settings",
    "add_folder",
    "remove_database",
    "engines",
    "choose_engine",
    "pick_engine",
    "set_autostart",
    "set_auto_update",
    "check_updates",
    "pairing_code",
    "copy_code",
    "new_code",
    "set_port",
    "open_pairing",
];

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest = tauri_build::AppManifest::new().commands(COMMANDS);
    if let Err(e) = tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest)) {
        println!("cargo::error={e:#}");
    }
}
