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

/// Whether `version` is one a release can have been stamped with: exactly
/// what `scripts/store_version.py` allocates (`docs/release.md`), a major up
/// to 65535 and `yymm.ddnn` of a day in 2000 to 2099, in numbers without
/// leading zeros.
pub fn is_release(version: &str) -> bool {
    let number = |n: &str| n.parse::<u32>().ok().filter(|v| v.to_string() == n);
    let numbers: Option<Vec<u32>> = version.split('.').map(number).collect();
    let Some(&[major, yymm, ddnn]) = numbers.as_deref() else { return false };
    (1..=65535).contains(&major)
        && yymm <= 9912
        && (1..=12).contains(&(yymm % 100))
        && (1..=31).contains(&(ddnn / 100))
        && (1..=99).contains(&(ddnn % 100))
}

/// What a person reads for the running `version`: a release's number, and
/// `dev` for a build made by hand, whose committed number no release ever
/// had (#387). Only for showing: what compares versions keeps the number.
pub fn shown(version: &str) -> &str {
    if is_release(version) { version } else { "dev" }
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
    fn a_release_reads_as_its_number_and_a_build_by_hand_as_dev() {
        // The allocator's own edges: 2000 to 2099, any day, major to 65535.
        let releases = ["1.2611.801", "1.2701.101", "1.2701.1001", "1.2612.3199", "2.2611.801"];
        let edges = ["1.1901.101", "1.1.101", "1.9912.3199", "65535.2611.801"];
        for release in releases.into_iter().chain(edges) {
            assert_eq!(shown(release), release);
        }
        let by_hand = ["1.4.0", "0.1.0", "0.2611.801", "65536.2611.801", "1.10001.101", "1.9913.801"];
        let not_a_day = ["1.1999.801", "1.2613.801", "1.2600.801", "1.2611.99", "1.2611.800", "1.2611.3201"];
        for version in by_hand.into_iter().chain(not_a_day) {
            assert_eq!(shown(version), "dev", "{version}");
        }
        for not_one in ["", "1.2611", "1.2611.801.0", "1.2611.x", "+1.2611.801", "01.2611.801", "1.2611.0801"] {
            assert_eq!(shown(not_one), "dev", "{not_one:?}");
        }
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
