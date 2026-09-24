//! How a game's names compare between the classic and the 2CBH copy of a
//! database. The differences the classic format explains are a closed list
//! ([`NameDiff`]); any other is a failure.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::ops::Range;

use cbformat::cbh::Entity;
use cbformat::v2::{Player, RecordKind};
use cbformat::view::{Base, Header};

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

/// How a classic text field of `width` bytes, which stores `stored` bytes
/// before its terminating zero and reads as `classic`, holds the 2CBH text
/// `two`.
fn text(classic: &str, stored: usize, two: &str, width: usize) -> NameDiff {
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
    // 2CBH fills tournament titles to the last byte. A cut stores that many
    // bytes, counted as stored and not as decoded. Only a field that holds
    // UTF-8 may stop short of it, when the next character's bytes would have
    // crossed the end: Windows-1252 stores every character in one byte, and
    // a field of ASCII alone does not show which encoding it has. A shorter
    // prefix of a name that fits is missing data, not a cut.
    let utf8 = !classic.is_ascii() && classic.len() <= stored;
    let next = t[c.len()].len_utf8();
    let fills = |capacity: usize| stored == capacity || (utf8 && stored < capacity && stored + next > capacity);
    if fills(width - 1) || fills(width) { NameDiff::Cut } else { NameDiff::Other }
}

/// The bytes the classic fields of a game store before their terminating
/// zeros, in the order of [`FIELDS`], a player's last name before the first:
/// white, black, event, site, annotator.
struct Stored {
    white: [usize; 2],
    black: [usize; 2],
    event: usize,
    site: usize,
    annotator: usize,
}

impl Stored {
    fn of(classic: &Base, header: &Header) -> Option<Stored> {
        let (Base::Cbh(db), Header::Cbh(r)) = (classic, header) else { return None };
        let e = db.entities();
        let data = |entity, id| e.data(entity, id).ok().flatten();
        let len = |d: &Option<Vec<u8>>, range: Range<usize>| {
            d.as_ref().and_then(|d| d.get(range)).map_or(0, |f| f.iter().position(|&b| b == 0).unwrap_or(f.len()))
        };
        let player = |id| {
            let d = data(Entity::Player, id);
            [len(&d, 0..30), len(&d, 30..50)]
        };
        let tournament = data(Entity::Tournament, r.tournament());
        Some(Stored {
            white: player(r.white()),
            black: player(r.black()),
            event: len(&tournament, 0..40),
            site: len(&tournament, 40..70),
            annotator: len(&data(Entity::Annotator, r.annotator()), 0..45),
        })
    }
}

fn words(t: &str) -> Vec<String> {
    let mut w: Vec<String> =
        t.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect();
    w.sort();
    w
}

/// A player compared by its last name (30 bytes) and first name (20), whose
/// classic fields store `stored` bytes.
fn player(classic: Option<&Player>, stored: [usize; 2], two: Option<&Player>) -> NameDiff {
    let part = |p: Option<&Player>, first: bool| p.map(|p| if first { &p.first } else { &p.last }).cloned();
    let last = text(&part(classic, false).unwrap_or_default(), stored[0], &part(two, false).unwrap_or_default(), 30);
    let first = text(&part(classic, true).unwrap_or_default(), stored[1], &part(two, true).unwrap_or_default(), 20);
    last.max(first)
}

fn annotator(classic: &str, stored: usize, two: &str) -> NameDiff {
    match text(classic, stored, two, 45) {
        NameDiff::Other if !classic.is_empty() && words(classic) == words(two) => NameDiff::WordOrder,
        d => d,
    }
}

/// The most suggestions the bridge gives for one prefix (`docs/api.md`).
pub const SUGGESTION_LIMIT: usize = 20;

/// Whether two suggestion lists for one prefix, as (name, games) pairs,
/// differ only by names that differ in a known way (`differing`). Those are
/// left out of both lists, and the rest must be equal as far as both reach. A
/// list may end sooner only where the bridge's limit cut it: the names past the
/// limit may be the ones the other list shows, because the known names took
/// their places. Any other missing suggestion is unexplained.
pub fn suggestions_agree(a: &[(String, u64)], b: &[(String, u64)], differing: &HashSet<String>) -> bool {
    let keep = |v: &[(String, u64)]| v.iter().filter(|x| !differing.contains(&x.0)).cloned().collect::<Vec<_>>();
    let (ka, kb) = (keep(a), keep(b));
    let n = ka.len().min(kb.len());
    if ka[..n] != kb[..n] {
        return false;
    }
    match ka.len().cmp(&kb.len()) {
        Ordering::Equal => true,
        Ordering::Less => a.len() == SUGGESTION_LIMIT,
        Ordering::Greater => b.len() == SUGGESTION_LIMIT,
    }
}

/// The name fields whose known differences can explain a suggestion field's
/// lists: both players for `player`, the event for `event` and the annotator
/// for `annotator`.
pub fn suggestion_fields(field: &str) -> u8 {
    match field {
        "player" => bit(WHITE) | bit(BLACK),
        "event" => bit(EVENT),
        "annotator" => bit(ANNOTATOR),
        _ => 0,
    }
}

/// The names that differ in a known way in the name fields of `fields`,
/// from `differing`, which holds them per field.
pub fn exceptions(differing: &[HashSet<String>; 5], fields: u8) -> HashSet<String> {
    (0..5).filter(|&f| fields & bit(f) != 0).flat_map(|f| differing[f].iter().cloned()).collect()
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
            let (Ok(na), Ok(nb), Some(st)) = (classic.names(&a), two.names(&b), Stored::of(classic, &a)) else {
                names.kinds += 1;
                continue;
            };
            let title = |t: &Option<cbformat::v2::Tournament>| t.as_ref().map(|t| t.title.clone()).unwrap_or_default();
            let place = |t: &Option<cbformat::v2::Tournament>| t.as_ref().map(|t| t.place.clone()).unwrap_or_default();
            let mut diffs = [NameDiff::Equal; 5];
            diffs[WHITE] = player(na.white.as_ref(), st.white, nb.white.as_ref());
            diffs[BLACK] = player(na.black.as_ref(), st.black, nb.black.as_ref());
            diffs[EVENT] = text(&title(&na.tournament), st.event, &title(&nb.tournament), 40);
            diffs[SITE] = text(&place(&na.tournament), st.site, &place(&nb.tournament), 30);
            diffs[ANNOTATOR] =
                annotator(&na.annotator.unwrap_or_default(), st.annotator, &nb.annotator.unwrap_or_default());
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

    /// [`text`] for a field that stores `classic` in UTF-8.
    fn utf8(classic: &str, two: &str, width: usize) -> NameDiff {
        text(classic, classic.len(), two, width)
    }

    /// [`text`] for a field that stores `classic` in Windows-1252.
    fn cp(classic: &str, two: &str, width: usize) -> NameDiff {
        text(classic, classic.chars().count(), two, width)
    }

    #[test]
    fn a_cut_exhausts_the_field() {
        let long = "Abcdefghijklmnopqrstuvwxyzabcdefgh";
        assert_eq!(long.len(), 34);
        // A 34-byte name in a 30-byte field: cut at 30 bytes, or at 29 when
        // the field keeps its terminating zero.
        assert_eq!(utf8(&long[..30], long, 30), NameDiff::Cut);
        assert_eq!(utf8(&long[..29], long, 30), NameDiff::Cut);
        // A 30-byte name held as its 27-byte prefix: missing data.
        assert_eq!(utf8(&long[..27], &long[..30], 30), NameDiff::Other);
        assert!(!known(WHITE, utf8(&long[..27], &long[..30], 30)));
        // Two bytes short of the width, with an ASCII character next.
        assert_eq!(utf8(&long[..28], long, 30), NameDiff::Other);
        // ASCII does not show its encoding, so a character that would cross
        // the end in UTF-8 and fit in Windows-1252 does not explain a field
        // that stops short.
        assert_eq!(utf8(&long[..28], &format!("{}éb", &long[..28]), 30), NameDiff::Other);
    }

    #[test]
    fn a_cut_counts_the_bytes_stored() {
        // `A` and 15 `é`: 16 bytes in Windows-1252, 31 in UTF-8.
        let two = format!("A{}", "é".repeat(15));
        let short = format!("A{}", "é".repeat(14));
        // 15 bytes in Windows-1252 leave room for the last `é`: missing data,
        // though the text reads as 29 bytes of UTF-8.
        assert_eq!(short.len(), 29);
        assert_eq!(cp(&short, &two, 30), NameDiff::Other);
        // The same text stored as 29 bytes of UTF-8 fills the field.
        assert_eq!(utf8(&short, &two, 30), NameDiff::Cut);
        // The whole name fits in Windows-1252.
        assert_eq!(cp(&two, &two, 30), NameDiff::Equal);
        // Windows-1252 holds `é` in one byte: 30 characters fill the field.
        let x = |n: usize| "x".repeat(n);
        assert_eq!(cp(&format!("{}éb", x(28)), &format!("{}ébcd", x(28)), 30), NameDiff::Cut);
    }

    #[test]
    fn a_character_across_the_end_is_dropped() {
        let x = |n: usize| "x".repeat(n);
        // 28 bytes of UTF-8, then a two-byte character that would end at byte
        // 30 of a field that keeps its terminating zero.
        let two = format!("é{}éb", x(26));
        assert_eq!(utf8(&two[..28], &two, 30), NameDiff::Cut);
        // In Windows-1252 the same text is 27 bytes, and the `é` fits.
        assert_eq!(cp(&two[..28], &two, 30), NameDiff::Other);
        // Nine two-byte characters of ten in a 20-byte field (19 and a zero).
        let cyrillic = "ЖЖЖЖЖЖЖЖЖЖ";
        assert_eq!(utf8(&cyrillic[..18], cyrillic, 20), NameDiff::Cut);
        // A three-byte character after 27 bytes crosses byte 29; after 26
        // bytes it would have fitted either way.
        let euro = |n: usize| format!("é{}€€", x(n - 2));
        assert_eq!(utf8(&euro(27)[..27], &euro(27), 30), NameDiff::Cut);
        assert_eq!(utf8(&euro(26)[..26], &euro(26), 30), NameDiff::Other);
        // Part of that character stored up to byte 29 is dropped when read.
        assert_eq!(text(&euro(27)[..27], 29, &euro(27), 30), NameDiff::Cut);
    }

    fn list(names: &[&str]) -> Vec<(String, u64)> {
        names.iter().map(|n| (format!("\"{n}\""), 1)).collect()
    }

    #[test]
    fn suggestions_need_an_explanation() {
        let none = HashSet::new();
        let known: HashSet<String> = ["\"Morphy, Paul\"".to_string()].into();
        // A name only one copy suggests, with no known difference behind it,
        // is unexplained, even against an empty list.
        assert!(!suggestions_agree(&list(&["Alpha, Beta"]), &[], &none));
        assert!(!suggestions_agree(&list(&["A", "B"]), &list(&["A"]), &none));
        // A name that differs in a known way is left out of both.
        assert!(suggestions_agree(&list(&["A", "Morphy, Paul", "B"]), &list(&["A", "B"]), &known));
        // Another games count is a difference.
        assert!(!suggestions_agree(&[("\"A\"".into(), 2)], &list(&["A"]), &none));
        // A full list whose known name took the place of the last one: the
        // other copy's last name is past this list's limit.
        let names: Vec<String> = (0..SUGGESTION_LIMIT).map(|i| format!("N{i:02}")).collect();
        let full: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut with_known: Vec<&str> = full[..SUGGESTION_LIMIT - 1].to_vec();
        with_known.insert(3, "Morphy, Paul");
        assert!(suggestions_agree(&list(&with_known), &list(&full), &known));
        // The same shortfall in a list the limit did not cut is unexplained.
        assert!(!suggestions_agree(&list(&full[..SUGGESTION_LIMIT - 1]), &list(&full), &known));
    }

    #[test]
    fn suggestion_exceptions_stay_in_their_field() {
        // A white surname cut in the classic copy, and an annotator that reads
        // the same as the cut surname in both copies.
        let cut = format!("\"Alpha, {}\"", "B".repeat(22));
        let mut differing: [HashSet<String>; 5] = Default::default();
        differing[WHITE].insert(cut.clone());
        differing[WHITE].insert(format!("\"Alpha, {}CDEFG\"", "B".repeat(22)));
        let classic = [(cut.clone(), 1)];
        // The player cut explains player suggestions, never annotator ones.
        assert!(suggestions_agree(&classic, &[], &exceptions(&differing, suggestion_fields("player"))));
        assert!(!suggestions_agree(&classic, &[], &exceptions(&differing, suggestion_fields("annotator"))));
        assert!(!suggestions_agree(&classic, &[], &exceptions(&differing, suggestion_fields("event"))));
        assert_eq!(suggestion_fields("player"), bit(WHITE) | bit(BLACK));
        assert_eq!(suggestion_fields("annotator"), bit(ANNOTATOR));
    }

    #[test]
    fn code_pages_and_word_order() {
        assert_eq!(cp("Lód?", "Łódź", 30), NameDiff::CodePage);
        assert_eq!(cp("Lodz", "Łódź", 30), NameDiff::Other, "ó is in Windows-1252");
        assert_eq!(annotator("Paul Morphy", 11, "Morphy, Paul"), NameDiff::WordOrder);
        assert!(known(ANNOTATOR, NameDiff::WordOrder) && !known(WHITE, NameDiff::WordOrder));
    }
}
