//! A game's players and its tournament, as both formats name them, and all
//! the entities a game names together.

use super::Date;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Player {
    pub last: String,
    pub first: String,
}

impl Player {
    pub fn pgn(&self) -> String {
        if self.first.is_empty() { self.last.clone() } else { format!("{}, {}", self.last, self.first) }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tournament {
    pub title: String,
    pub place: String,
    pub start: Date,
    /// The type byte, as stored: the kind of event in the low bits and the
    /// time control in the high ones ([`TimeControl::of_kind`]); 0 where the
    /// format has none, as in a PGN file.
    pub kind: u8,
}

/// The entities a game names; `None` for an unused or unreadable entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Names {
    pub white: Option<Player>,
    pub black: Option<Player>,
    pub tournament: Option<Tournament>,
    pub annotator: Option<String>,
}

/// A game's time control as ChessBase marks a tournament's (#268): the high
/// bits of its type byte, `0x20` blitz, `0x40` rapid and `0x80`
/// correspondence, and none of them for a normal game. A tournament can carry
/// more than one. A PGN file has no type byte, and classes each game by its
/// `TimeControl` tag instead ([`TimeControl::of_pgn`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TimeControl(u8);

impl TimeControl {
    pub const NORMAL: TimeControl = TimeControl(0);
    pub const BLITZ: TimeControl = TimeControl(0x20);
    pub const RAPID: TimeControl = TimeControl(0x40);
    pub const CORRESPONDENCE: TimeControl = TimeControl(0x80);
    const BITS: u8 = 0xe0;

    /// The time control a tournament's type byte gives.
    pub fn of_kind(kind: u8) -> TimeControl {
        TimeControl(kind & Self::BITS)
    }

    /// The bits, as a type byte holds them.
    pub fn bits(self) -> u8 {
        self.0
    }

    /// Whether a game of this time control is one of `class`, which is one
    /// of the four constants: [`TimeControl::NORMAL`] has none of the bits,
    /// and the others have theirs.
    pub fn is(self, class: TimeControl) -> bool {
        match class.0 {
            0 => self.0 == 0,
            bit => self.0 & bit != 0,
        }
    }

    /// A PGN `TimeControl` tag's value classed by FIDE's limits on its first
    /// period, whose time is its seconds and 60 times its increment: at most
    /// 10 minutes is blitz, bullet included, less than 60 minutes rapid, and
    /// more normal. `-`, or a period that gives an hour or more a move (as
    /// `1/86400` does), is correspondence. A value that is missing, `?` or not
    /// read is normal, as a ChessBase tournament without a mark is.
    pub fn of_pgn(value: &[u8]) -> TimeControl {
        let value = value.trim_ascii();
        if value == b"-" {
            return TimeControl::CORRESPONDENCE;
        }
        let first = value.split(|&b| b == b':').next().unwrap_or_default();
        let first = first.strip_prefix(b"*").unwrap_or(first);
        // A period of `moves/seconds` gives its seconds to that many moves.
        let (moves, rest, per_moves) = match first.iter().position(|&b| b == b'/') {
            Some(at) => (seconds(&first[..at]), &first[at + 1..], true),
            None => (Some(1), first, false),
        };
        let (base, increment) = match rest.iter().position(|&b| b == b'+') {
            Some(at) => (seconds(&rest[..at]), seconds(&rest[at + 1..])),
            None => (seconds(rest), Some(0)),
        };
        let (Some(moves), Some(base), Some(increment)) = (moves, base, increment) else { return TimeControl::NORMAL };
        const HOUR: u64 = 3_600;
        let per_move_period = per_moves && moves > 0 && base / moves >= HOUR;
        if per_move_period || increment >= HOUR {
            return TimeControl::CORRESPONDENCE;
        }
        match base + 60 * increment {
            0 => TimeControl::NORMAL,
            t if t <= 600 => TimeControl::BLITZ,
            t if t < HOUR => TimeControl::RAPID,
            _ => TimeControl::NORMAL,
        }
    }
}

/// A whole number of seconds, at most 9 ASCII digits.
fn seconds(text: &[u8]) -> Option<u64> {
    if text.is_empty() || text.len() > 9 || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(text.iter().fold(0, |n, &d| n * 10 + u64::from(d - b'0')))
}

#[cfg(test)]
mod tests {
    use super::TimeControl;

    #[test]
    fn a_type_byte_keeps_its_time_control_bits() {
        assert_eq!(TimeControl::of_kind(0x23), TimeControl::BLITZ);
        assert_eq!(TimeControl::of_kind(0x41), TimeControl::RAPID);
        assert_eq!(TimeControl::of_kind(0x81), TimeControl::CORRESPONDENCE);
        assert_eq!(TimeControl::of_kind(0x05), TimeControl::NORMAL);
        let both = TimeControl::of_kind(0x60);
        assert!(both.is(TimeControl::BLITZ) && both.is(TimeControl::RAPID) && !both.is(TimeControl::NORMAL));
        assert!(TimeControl::NORMAL.is(TimeControl::NORMAL) && !TimeControl::NORMAL.is(TimeControl::BLITZ));
    }

    #[test]
    fn a_pgn_tag_is_classed_by_fide_limits() {
        for (tag, want) in [
            ("60", TimeControl::BLITZ),
            ("60+0", TimeControl::BLITZ),
            ("180+2", TimeControl::BLITZ),
            ("600", TimeControl::BLITZ),
            ("300+5", TimeControl::BLITZ),
            ("480+3", TimeControl::RAPID),
            ("601", TimeControl::RAPID),
            ("900+10", TimeControl::RAPID),
            ("3599", TimeControl::RAPID),
            ("3600", TimeControl::NORMAL),
            ("2700+15", TimeControl::NORMAL),
            ("5400+30", TimeControl::NORMAL),
            ("40/7200:3600", TimeControl::NORMAL),
            // Only the first period counts: a later `moves/seconds` one is not it.
            ("3600:40/7200", TimeControl::NORMAL),
            ("3600:/", TimeControl::NORMAL),
            ("600:1/86400", TimeControl::BLITZ),
            ("1/3600", TimeControl::CORRESPONDENCE),
            ("40/5400+30:1800+30", TimeControl::NORMAL),
            ("*180", TimeControl::BLITZ),
            ("-", TimeControl::CORRESPONDENCE),
            (" - ", TimeControl::CORRESPONDENCE),
            ("1/86400", TimeControl::CORRESPONDENCE),
            ("1/259200", TimeControl::CORRESPONDENCE),
            ("0+86400", TimeControl::CORRESPONDENCE),
            ("?", TimeControl::NORMAL),
            ("", TimeControl::NORMAL),
            ("0", TimeControl::NORMAL),
            ("90 min", TimeControl::NORMAL),
            ("40/", TimeControl::NORMAL),
            ("+5", TimeControl::NORMAL),
            ("9999999999", TimeControl::NORMAL),
        ] {
            assert_eq!(TimeControl::of_pgn(tag.as_bytes()), want, "{tag:?}");
        }
    }
}
