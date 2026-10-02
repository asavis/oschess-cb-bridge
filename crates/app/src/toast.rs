//! The toast the Store copy hands Windows for a notice (#265): its XML, built
//! here so that it is tested on every system.

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
    fn text_is_escaped() {
        assert_eq!(escape(r#"a & b < c > d "e" 'f'"#), "a &amp; b &lt; c &gt; d &quot;e&quot; &apos;f&apos;");
        assert!(xml("<x>", "&").contains("<text>&lt;x&gt;</text><text>&amp;</text>"));
    }
}
