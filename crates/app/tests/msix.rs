//! The Microsoft Store package's manifest (#112) agrees with the app:
//! scripts/msix.py packs it as it is, with only the identity and the version
//! filled in.

use std::path::Path;

use serde_json::Value;

fn app_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn manifest() -> String {
    std::fs::read_to_string(app_dir().join("msix").join("AppxManifest.xml")).expect("the manifest template")
}

/// Every value of `attribute` in the manifest.
fn values<'a>(manifest: &'a str, attribute: &str) -> Vec<&'a str> {
    manifest.split(&format!(" {attribute}=\"")).skip(1).filter_map(|rest| rest.split('"').next()).collect()
}

#[test]
fn the_startup_task_is_the_one_the_app_enables() {
    assert_eq!(values(&manifest(), "TaskId"), [app::channel::STARTUP_TASK]);
}

#[test]
fn the_package_starts_the_executable_tauri_builds() {
    let config: Value =
        serde_json::from_str(&std::fs::read_to_string(app_dir().join("tauri.conf.json")).unwrap()).unwrap();
    let exe = format!("{}.exe", config["mainBinaryName"].as_str().expect("mainBinaryName"));
    let manifest = manifest();
    assert_eq!(values(&manifest, "Executable"), [exe.as_str(), exe.as_str()], "the app and its startup task");
    assert!(manifest.contains(r#"EntryPoint="Windows.FullTrustApplication""#));
    assert!(manifest.contains(r#"<rescap:Capability Name="runFullTrust" />"#));
}

#[test]
fn every_image_the_manifest_names_is_drawn() {
    let manifest = manifest();
    let mut named: Vec<&str> =
        ["Square150x150Logo", "Square44x44Logo"].iter().flat_map(|a| values(&manifest, a)).collect();
    named.extend(manifest.split("<Logo>").nth(1).and_then(|rest| rest.split("</Logo>").next()));
    assert_eq!(named.len(), 3);
    let drawn = app_dir().join("icons").join("msix");
    for image in named {
        let file = image.strip_prefix(r"Assets\").expect("an image in Assets");
        assert!(drawn.join(file).is_file(), "icons/msix/{file}");
    }
    // Windows takes the app list and taskbar icons from these, found through
    // the resources.pri scripts/msix.py writes: the unplated form on a dark
    // taskbar, the light-unplated one on a light taskbar. Without the form the
    // taskbar asks for, it lays the 44-pixel one on a plate of the accent
    // colour (#197, #262).
    for size in [16, 20, 24, 30, 32, 36, 40, 48, 60, 64, 72, 80, 96, 256] {
        let mut forms = Vec::new();
        for form in ["unplated", "lightunplated"] {
            let file = format!("Square44x44Logo.targetsize-{size}_altform-{form}.png");
            forms.push(std::fs::read(drawn.join(&file)).unwrap_or_else(|_| panic!("icons/msix/{file}")));
        }
        assert_eq!(forms[0], forms[1], "the {size} px icon is the same on a light and a dark taskbar");
    }
}

#[test]
fn only_the_identity_and_the_version_are_left_to_fill_in() {
    let manifest = manifest();
    let mut left: Vec<&str> = manifest.split('@').skip(1).step_by(2).collect();
    left.sort_unstable();
    assert_eq!(left, ["NAME", "PUBLISHER", "PUBLISHER_DISPLAY_NAME", "VERSION"]);
    assert!(manifest.contains(
        r#"<Identity Name="@NAME@" Publisher="@PUBLISHER@" Version="@VERSION@" ProcessorArchitecture="x64" />"#
    ));
}
