//! The installer's hooks: a real uninstall removes the «Start with Windows»
//! entries, and an update keeps them.

use std::path::Path;

use serde_json::Value;

fn app_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// The configuration names the hook file, and the file is there.
fn hooks() -> String {
    let config: Value =
        serde_json::from_str(&std::fs::read_to_string(app_dir().join("tauri.conf.json")).unwrap()).unwrap();
    let path = config.pointer("/bundle/windows/nsis/installerHooks").and_then(Value::as_str).expect("installerHooks");
    std::fs::read_to_string(app_dir().join(path)).expect("the hook file exists")
}

#[test]
fn a_real_uninstall_removes_the_autostart_entries_and_an_update_keeps_them() {
    let hooks = hooks();
    let body = hooks
        .split("!macro NSIS_HOOK_POSTUNINSTALL")
        .nth(1)
        .and_then(|rest| rest.split("!macroend").next())
        .expect("a post-uninstall hook");
    // tauri-plugin-autostart names both values after the package, which Tauri
    // takes from `productName`: the installer's ${PRODUCTNAME}.
    for key in [
        r"Software\Microsoft\Windows\CurrentVersion\Run",
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run",
    ] {
        assert!(body.contains(&format!("DeleteRegValue HKCU \"{key}\" \"${{PRODUCTNAME}}\"")), "{key}");
    }
    // The updater runs the old uninstaller with /UPDATE, and the setting stays.
    let guard = body.find("${If} $UpdateMode <> 1").expect("guarded by the update mode");
    assert!(body.find("DeleteRegValue").is_some_and(|at| at > guard));
    assert!(body.contains("${EndIf}"));
}
