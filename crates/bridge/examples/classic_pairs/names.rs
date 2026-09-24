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
    /// Cut at the classic field's width (characters Windows-1252 lacks may
    /// be stored in another form before the cut).
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
    if held && c.len() == t.len() {
        return NameDiff::CodePage;
    }
    // A field cut at its width holds about `width` bytes: one a character in
    // Windows-1252, or UTF-8 less an incomplete last character.
    let full = |bytes: usize| (width.saturating_sub(3)..=width).contains(&bytes);
    if held && (full(c.len()) || full(classic.len())) {
        return NameDiff::Cut;
    }
    NameDiff::Other
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
