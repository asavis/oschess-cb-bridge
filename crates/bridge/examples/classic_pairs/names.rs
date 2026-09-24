//! How a game's names compare between the classic and the 2CBH copy of a
//! database. The differences the classic format explains are a closed list
//! ([`NameDiff`]); any other is a failure.

use std::collections::BTreeMap;

use cbformat::v2::{Player, RecordKind};
use cbformat::view::Base;

/// The name fields of a game, by index.
pub const FIELDS: [&str; 5] = ["white", "black", "event", "site", "annotator"];
pub const WHITE: usize = 0;
pub const BLACK: usize = 1;
pub const EVENT: usize = 2;
const SITE: usize = 3;
pub const ANNOTATOR: usize = 4;

/// The bit of a field in a set of fields.
pub fn bit(field: usize) -> u8 {
    1 << field
}

/// How one name compares between the copies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum NameDiff {
    Equal,
    /// Characters that Windows-1252 lacks are stored in some other form; the
    /// others are equal.
    CodePage,
    /// Cut where the classic field ends: at its width, or its width less a
    /// terminating zero (characters Windows-1252 lacks may be stored in
    /// another form before the cut).
    Cut,
    /// The same words in another order: only an annotator, which the classic
    /// format keeps as one text (`First Last` for `Last, First`).
    WordOrder,
    /// Anything else, which nothing explains.
    Other,
}

/// Whether `d` is a difference the classic format explains for `field`.
pub fn known(field: usize, d: NameDiff) -> bool {
    match d {
        NameDiff::Equal | NameDiff::CodePage | NameDiff::Cut => true,
        NameDiff::WordOrder => field == ANNOTATOR,
        NameDiff::Other => false,
    }
}

/// Whether Windows-1252 has `c`.
fn cp1252(c: char) -> bool {
    let n = u32::from(c);
    n < 0x80 || (0xa0..=0xff).contains(&n) || "€‚ƒ„…†‡ˆ‰Š‹ŒŽ‘’“”•–—˜™š›œžŸ".contains(c)
}

/// How a classic text field of `width` bytes holds the 2CBH text `two`.
fn text(classic: &str, two: &str, width: usize) -> NameDiff {
    if classic == two {
        return NameDiff::Equal;
    }
    let (c, t): (Vec<char>, Vec<char>) = (classic.chars().collect(), two.chars().collect());
    let held = c.len() <= t.len() && c.iter().zip(&t).all(|(a, b)| a == b || !cp1252(*b));
    if !held {
        return NameDiff::Other;
    }
    if c.len() == t.len() {
        return NameDiff::CodePage;
    }
    // A cut exhausts the field. ChessBase keeps a name's terminating zero in
    // its field, which then holds `width - 1` bytes, and its converter from
    // 2CBH fills tournament titles to the last byte. A cut holds that many
    // bytes: characters, one byte each, in Windows-1252; in UTF-8 that many
    // bytes, or fewer only when the next character would have crossed the
    // end. A shorter prefix of a name that fits is missing data, not a cut.
    let next = t[c.len()].len_utf8();
    let fills = |capacity: usize| {
        (c.len() == capacity && c.iter().all(|&x| cp1252(x)))
            || classic.len() == capacity
            || (classic.len() < capacity && classic.len() + next > capacity)
    };
    if fills(width - 1) || fills(width) { NameDiff::Cut } else { NameDiff::Other }
}

fn words(t: &str) -> Vec<String> {
    let mut w: Vec<String> =
        t.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect();
    w.sort();
    w
}

/// A player compared by its last name (30 bytes) and first name (20).
fn player(classic: Option<&Player>, two: Option<&Player>) -> NameDiff {
    let part = |p: Option<&Player>, first: bool| p.map(|p| if first { &p.first } else { &p.last }).cloned();
    let last = text(&part(classic, false).unwrap_or_default(), &part(two, false).unwrap_or_default(), 30);
    let first = text(&part(classic, true).unwrap_or_default(), &part(two, true).unwrap_or_default(), 20);
    last.max(first)
}

fn annotator(classic: &str, two: &str) -> NameDiff {
    match text(classic, two, 45) {
        NameDiff::Other if !classic.is_empty() && words(classic) == words(two) => NameDiff::WordOrder,
        d => d,
    }
}

/// The names of both copies, compared game by game.
pub struct Names {
    per_record: Vec<[NameDiff; 5]>,
    /// Games per field and difference.
    pub counts: BTreeMap<(&'static str, NameDiff), u64>,
    /// Records of another kind in the other copy, or unreadable.
    pub kinds: u64,
}

impl Names {
    pub fn compare(classic: &Base, two: &Base) -> Names {
        let n = classic.record_count().min(two.record_count());
        let mut names =
            Names { per_record: vec![[NameDiff::Equal; 5]; n as usize + 1], counts: BTreeMap::new(), kinds: 0 };
        for id in 1..=n {
            let (Ok(a), Ok(b)) = (classic.header(id), two.header(id)) else {
                names.kinds += 1;
                continue;
            };
            if a.kind() != b.kind() {
                names.kinds += 1;
                continue;
            }
            if a.kind() != RecordKind::Game {
                continue;
            }
            let (Ok(na), Ok(nb)) = (classic.names(&a), two.names(&b)) else {
                names.kinds += 1;
                continue;
            };
            let title = |t: &Option<cbformat::v2::Tournament>| t.as_ref().map(|t| t.title.clone()).unwrap_or_default();
            let place = |t: &Option<cbformat::v2::Tournament>| t.as_ref().map(|t| t.place.clone()).unwrap_or_default();
            let mut diffs = [NameDiff::Equal; 5];
            diffs[WHITE] = player(na.white.as_ref(), nb.white.as_ref());
            diffs[BLACK] = player(na.black.as_ref(), nb.black.as_ref());
            diffs[EVENT] = text(&title(&na.tournament), &title(&nb.tournament), 40);
            diffs[SITE] = text(&place(&na.tournament), &place(&nb.tournament), 30);
            diffs[ANNOTATOR] = annotator(&na.annotator.unwrap_or_default(), &nb.annotator.unwrap_or_default());
            for (field, d) in diffs.iter().enumerate() {
                if *d != NameDiff::Equal {
                    *names.counts.entry((FIELDS[field], *d)).or_insert(0) += 1;
                }
            }
            names.per_record[id as usize] = diffs;
        }
        names
    }

    /// The fields of record `n` whose names differ in a known way.
    pub fn known_bits(&self, n: u32) -> u8 {
        let Some(diffs) = self.per_record.get(n as usize) else { return 0 };
        (0..5).filter(|&f| diffs[f] != NameDiff::Equal && known(f, diffs[f])).fold(0, |acc, f| acc | bit(f))
    }

    /// Whether a difference in `field` of record `n` is a known one.
    pub fn explains(&self, n: u32, field: usize) -> bool {
        self.known_bits(n) & bit(field) != 0
    }

    /// Games with a name difference no known kind explains.
    pub fn unknown(&self) -> u64 {
        self.counts
            .iter()
            .filter(|((f, d), _)| !known(FIELDS.iter().position(|x| x == f).unwrap(), *d))
            .map(|(_, n)| n)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_exhausts_the_field() {
        let long = "Abcdefghijklmnopqrstuvwxyzabcdefgh";
        assert_eq!(long.len(), 34);
        // A 34-byte name in a 30-byte field: cut at 30 bytes, or at 29 when
        // the field keeps its terminating zero.
        assert_eq!(text(&long[..30], long, 30), NameDiff::Cut);
        assert_eq!(text(&long[..29], long, 30), NameDiff::Cut);
        // A 30-byte name held as its 27-byte prefix: missing data.
        assert_eq!(text(&long[..27], &long[..30], 30), NameDiff::Other);
        assert!(!known(WHITE, text(&long[..27], &long[..30], 30)));
        // Two bytes short of the width, with an ASCII character next.
        assert_eq!(text(&long[..28], long, 30), NameDiff::Other);
    }

    #[test]
    fn a_character_across_the_end_is_dropped() {
        // 28 bytes, then a two-byte character that would end at byte 30 of
        // a field that keeps its terminating zero.
        let two = format!("{}ébc", "x".repeat(28));
        assert_eq!(text(&"x".repeat(28), &two, 30), NameDiff::Cut);
        // Nine two-byte characters of ten in a 20-byte field (19 and a zero).
        let cyrillic = "ЖЖЖЖЖЖЖЖЖЖ";
        assert_eq!(text(&cyrillic[..18], cyrillic, 20), NameDiff::Cut);
        // Windows-1252 holds `é` in one byte: 30 characters fill the field.
        assert_eq!(text(&format!("{}éb", "x".repeat(28)), &format!("{two}d"), 30), NameDiff::Cut);
        // A three-byte character after 27 bytes crosses byte 29; after 26
        // bytes it would have fitted either way.
        let euro = |n: usize| format!("{}€€", "x".repeat(n));
        assert_eq!(text(&"x".repeat(27), &euro(27), 30), NameDiff::Cut);
        assert_eq!(text(&"x".repeat(26), &euro(26), 30), NameDiff::Other);
    }

    #[test]
    fn code_pages_and_word_order() {
        assert_eq!(text("Lód?", "Łódź", 30), NameDiff::CodePage);
        assert_eq!(text("Lodz", "Łódź", 30), NameDiff::Other, "ó is in Windows-1252");
        assert_eq!(annotator("Paul Morphy", "Morphy, Paul"), NameDiff::WordOrder);
        assert!(known(ANNOTATOR, NameDiff::WordOrder) && !known(WHITE, NameDiff::WordOrder));
    }
}
