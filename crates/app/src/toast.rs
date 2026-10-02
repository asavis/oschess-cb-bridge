//! The toast the bridge hands Windows for a notice (#265), and the
//! AppUserModelID a direct copy shows it under, decided here so that both are
//! tested on every system.

use std::path::Path;

/// PowerShell's AppUserModelID, which Windows always knows. A build run from
/// Cargo's `target` folder shows its toasts under it, since no installer
/// registered the app's own there; the notification plugin did the same.
pub const POWERSHELL_APP_ID: &str = "{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe";

/// The AppUserModelID a direct copy's toast goes under: `identifier`, which
/// the NSIS installer's shortcut registers, unless the executable runs from
/// `exe_dir` in Cargo's `target/debug` or `target/release`.
pub fn direct_app_id<'a>(identifier: &'a str, exe_dir: &Path) -> &'a str {
    if exe_dir.ends_with("target/debug") || exe_dir.ends_with("target/release") {
        POWERSHELL_APP_ID
    } else {
        identifier
    }
}

/// A toast with `title` and, when it is not empty, `body`, in Windows'
/// generic template.
pub fn xml(title: &str, body: &str) -> String {
    let mut lines = format!("<text>{}</text>", escape(title));
    if !body.is_empty() {
        lines.push_str(&format!("<text>{}</text>", escape(body)));
    }
    format!(r#"<toast><visual><binding template="ToastGeneric">{lines}</binding></visual></toast>"#)
}

/// `text` as XML character data.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_toast_holds_its_title_and_body() {
        assert_eq!(
            xml("Міст оновлено до 1.3.3", "Готово."),
            r#"<toast><visual><binding template="ToastGeneric"><text>Міст оновлено до 1.3.3</text><text>Готово.</text></binding></visual></toast>"#
        );
    }

    #[test]
    fn an_empty_body_adds_no_line() {
        assert_eq!(
            xml("Встановлюю оновлення", ""),
            r#"<toast><visual><binding template="ToastGeneric"><text>Встановлюю оновлення</text></binding></visual></toast>"#
        );
    }

    #[test]
    fn an_installed_copy_uses_its_identifier_and_a_build_in_target_powershells() {
        let id = "org.oschess.bridge";
        assert_eq!(direct_app_id(id, Path::new("/apps/oschess bridge")), id);
        assert_eq!(direct_app_id(id, Path::new("/src/bridge/target/release")), POWERSHELL_APP_ID);
        assert_eq!(direct_app_id(id, Path::new("/src/bridge/target/debug")), POWERSHELL_APP_ID);
        assert_eq!(direct_app_id(id, Path::new("/apps/target")), id, "a folder merely named target");
        assert_eq!(direct_app_id(id, Path::new("/apps/my-target/release")), id, "the whole folder name counts");
    }

    #[test]
    fn text_is_escaped() {
        assert_eq!(escape(r#"a & b < c > d "e" 'f'"#), "a &amp; b &lt; c &gt; d &quot;e&quot; &apos;f&apos;");
        assert!(xml("<x>", "&").contains("<text>&lt;x&gt;</text><text>&amp;</text>"));
    }
}
