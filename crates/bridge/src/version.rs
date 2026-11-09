//! Versions that are dotted numbers: the bridge's own and Stockfish's.

/// Whether `text` is a version: numbers joined by dots, such as `19` or
/// `17.1`.
pub fn is_version(text: &str) -> bool {
    text.len() <= 16 && text.split('.').all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether the version `version` is newer than `than`, compared number by
/// number: 19 < 19.1 < 20, and 19.0 is 19. A text that is not a version is
/// newer than nothing, and nothing is newer than it.
pub fn is_newer(version: &str, than: &str) -> bool {
    let numbers = |text: &str| -> Option<Vec<u64>> {
        if !is_version(text) {
            return None;
        }
        let mut numbers: Vec<u64> = text.split('.').map(|n| n.parse().ok()).collect::<Option<_>>()?;
        while numbers.len() > 1 && numbers.last() == Some(&0) {
            numbers.pop();
        }
        Some(numbers)
    };
    numbers(version).zip(numbers(than)).is_some_and(|(version, than)| version > than)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_number_by_number() {
        assert!(is_newer("1.3.3", "1.3.2"));
        assert!(is_newer("1.10.0", "1.9.9"));
        assert!(is_newer("2.0.0", "1.99.99"));
        assert!(is_newer("19.1", "19"));
        assert!(is_newer("20", "19.1"));
        assert!(!is_newer("1.3.2", "1.3.2"));
        assert!(!is_newer("1.3.1", "1.3.2"));
    }

    /// Zeros at the end say nothing: the two comparisons the app and the
    /// bridge had disagreed here (#377).
    #[test]
    fn zeros_at_the_end_say_nothing() {
        assert!(!is_newer("1.2.0", "1.2"));
        assert!(!is_newer("1.2", "1.2.0"));
        assert!(!is_newer("19.0", "19"));
    }

    #[test]
    fn what_is_not_a_version_is_not_compared() {
        assert!(!is_newer("", "1.3.2"));
        assert!(!is_newer("1.3.3", ""));
        assert!(!is_newer("1.3.x", "1.3.2"));
        assert!(!is_newer("1.3.3", "1.3.x"));
        assert!(!is_version("1..2") && !is_version("1.2.") && is_version("17.1"));
    }
}
