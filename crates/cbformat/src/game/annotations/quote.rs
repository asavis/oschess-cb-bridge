//! Game quotations (type `13`): a game quoted in a comment, with a header of
//! its own and, in 2CBH, optionally its moves. The layouts are in
//! `docs/format-notes.md`; what is not understood is kept in the annotation's
//! data, which the full PGN form carries whole.

use std::ops::Range;

use super::decode_text;
use crate::game::{Date, Eco};

/// A player of a quoted game.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuotedPlayer {
    pub last: String,
    pub first: String,
    /// The rating, 0 when unknown.
    pub elo: u16,
}

/// A decoded game quotation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Quotation {
    pub white: QuotedPlayer,
    pub black: QuotedPlayer,
    pub event: String,
    pub site: String,
    pub date: Date,
    /// The event's type byte: bit `0x20` blitz, `0x40` rapid, `0x80`
    /// correspondence; the low bits the kind of event.
    pub kind: u8,
    pub round: u8,
    /// Stored as a signed byte; ChessBase shows a negative one as its 16-bit
    /// two's complement.
    pub subround: i8,
    /// 0 black won, 1 draw, 2 white won.
    pub result: u8,
    pub eco: Eco,
    /// The quoted moves as stored, origin and destination byte each, for a
    /// 2CBH quotation from the standard position; empty otherwise.
    pub moves: Vec<[u8; 2]>,
    /// The quoted game starts from a set-up position, whose encoding is not
    /// fully understood: its moves are left in the data.
    pub set_up: bool,
}

/// Reads bytes in order: `None` past the end, or for [`quote_offsets`] the
/// [`QuoteDamage`] there.
struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.i..self.i.checked_add(n)?)?;
        self.i += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn be16(&mut self) -> Option<u16> {
        let b = self.take(2)?;
        Some(u16::from_be_bytes([b[0], b[1]]))
    }
    fn le32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn be32(&mut self) -> Option<i32> {
        Some(i32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }
    /// A classic string: a length byte, the text and a zero.
    fn classic(&mut self) -> Option<String> {
        let n = self.u8()? as usize;
        let s = self.take(n)?;
        self.take(1)?;
        Some(decode_text(s))
    }

    /// Where the next `n` bytes lie, for [`quote_offsets`].
    fn span(&mut self, n: usize) -> Result<Range<usize>, QuoteDamage> {
        let start = self.i;
        self.take(n).map(|_| start..self.i).ok_or_else(|| self.damage(PAST_END))
    }
    fn byte(&mut self) -> Result<u8, QuoteDamage> {
        self.u8().ok_or_else(|| self.damage(PAST_END))
    }
    /// A non-negative `int` length that fits in what is left.
    fn length(&mut self) -> Result<usize, QuoteDamage> {
        let n = self.le32().ok_or_else(|| self.damage(PAST_END))?;
        let left = self.b.len().saturating_sub(self.i);
        usize::try_from(n).ok().filter(|&n| n <= left).ok_or_else(|| self.damage("length out of range"))
    }
    fn damage(&self, what: &'static str) -> QuoteDamage {
        QuoteDamage { at: self.i, what }
    }
}

/// The damage of a 2CBH quotation whose data ends before its layout does, in
/// the `.2cba` reader's words.
const PAST_END: &str = "runs past the end of the record";

/// Where [`quote_offsets`] found a 2CBH quotation damaged: the offset in its
/// data, and what is wrong there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct QuoteDamage {
    pub(crate) at: usize,
    pub(crate) what: &'static str,
}

/// Where the fields of a 2CBH quotation lie in its data, and where it ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QuoteOffsets {
    /// The six header strings, each after its length byte and with its
    /// terminating zero: white's last and first name, black's last and first
    /// name, the site and the event.
    pub(crate) strings: [Range<usize>; 6],
    /// The 35 + 44 fixed bytes after them.
    pub(crate) fixed: Range<usize>,
    /// The quoted game starts from a set-up position.
    pub(crate) set_up: bool,
    /// The moves, 5 bytes each.
    pub(crate) moves: Range<usize>,
    /// The bytes the quotation takes, the 4 after its moves included.
    pub(crate) len: usize,
}

/// The one walk of the 2CBH quotation layout (`docs/format-notes.md`) from
/// the start of `data`, the bytes after the type up to the end of the record:
/// the `.2cba` reader skips a quotation by it, and [`Quotation::parse_2cbh`]
/// reads each field where it finds it. The layout: `01`, the mode, two
/// unknown bytes, `int` 1 and a zero; six strings, each a length byte that
/// counts its terminating zero; 35 + 44 fixed bytes; two rating lists, each
/// `01 00 01 00 00`, an `int` length and that many bytes; 26 bytes; the start,
/// 1 for the standard position, or 0 and a set-up position's 64 squares and 11
/// bytes; 2 bytes; an `int` count of 5-byte moves; and 4 bytes. `Ok(None)` for
/// a start of unknown meaning.
pub(crate) fn quote_offsets(data: &[u8]) -> Result<Option<QuoteOffsets>, QuoteDamage> {
    let mut c = Cursor { b: data, i: 0 };
    if c.byte()? != 1 {
        return Err(c.damage("expected 01"));
    }
    c.span(2 + 2 + 4 + 1)?; // mode, unknown, int 1, zero
    let mut string = || {
        let n = c.byte()?; // counts the terminating zero
        c.span(usize::from(n))
    };
    let strings = [string()?, string()?, string()?, string()?, string()?, string()?];
    let fixed = c.span(35 + 44)?;
    for _ in 0..2 {
        c.span(5)?; // 01 00 01 00 00
        let n = c.length()?;
        c.span(n)?;
    }
    c.span(26)?;
    let set_up = match c.byte()? {
        1 => false,
        // A set-up start: 64 squares file by file, then 11 bytes of side to
        // move, move number and the like.
        0 => {
            c.span(64 + 11)?;
            true
        }
        _ => return Ok(None),
    };
    c.span(2)?;
    let n = c.length()?;
    let moves = c.span(n.checked_mul(5).ok_or_else(|| c.damage("quotation move count"))?)?;
    c.span(4)?;
    Ok(Some(QuoteOffsets { strings, fixed, set_up, moves, len: c.i }))
}

impl Quotation {
    /// A 2CBH quotation's data, the bytes after the type, each field read
    /// where the walk the `.2cba` reader skips quotations by finds it. `None`
    /// when it does not have the known layout.
    pub fn parse_2cbh(data: &[u8]) -> Option<Quotation> {
        let at = quote_offsets(data).ok()??;
        let [wl, wf, bl, bf, site, event] =
            at.strings.map(|s| data.get(s).map(|s| decode_text(s.strip_suffix(&[0]).unwrap_or(s))));
        let fixed = data.get(at.fixed)?;
        let le16 = |o: usize| Some(u16::from_le_bytes([*fixed.get(o)?, *fixed.get(o + 1)?]));
        let moves = if at.set_up {
            Vec::new()
        } else {
            data.get(at.moves)?.as_chunks::<5>().0.iter().map(|m| [m[0], m[1]]).collect()
        };
        Some(Quotation {
            white: QuotedPlayer { last: wl?, first: wf?, elo: le16(28)? },
            black: QuotedPlayer { last: bl?, first: bf?, elo: le16(30)? },
            event: event?,
            site: site?,
            date: Date(i32::from_le_bytes(fixed.get(0..4)?.try_into().ok()?)),
            kind: *fixed.get(4)?,
            round: *fixed.get(43)?,
            subround: *fixed.get(44)? as i8,
            result: *fixed.get(34)?,
            eco: Eco::from_field(le16(32)?),
            moves,
            set_up: at.set_up,
        })
    }

    /// A classic quotation's data. Its header is read; the moves of a classic
    /// quotation are not understood and stay in the data.
    pub fn parse_classic(data: &[u8]) -> Option<Quotation> {
        let mut c = Cursor { b: data, i: 0 };
        c.take(6)?; // size, mode, unknown
        let (white, black) = (c.classic()?, c.classic()?);
        let (white_elo, black_elo, eco) = (c.be16()?, c.be16()?, c.be16()?);
        let (event, site) = (c.classic()?, c.classic()?);
        let date = c.be32()?;
        let kind = c.be16()?.to_le_bytes()[0];
        c.take(2 + 4)?; // nation, unknown and rounds
        let subround = c.u8()? as i8;
        let round = c.u8()?;
        let result = c.u8()?;
        let split = |s: &str| match s.split_once(',') {
            Some((last, first)) => (last.to_string(), first.to_string()),
            None => (s.to_string(), String::new()),
        };
        let ((wl, wf), (bl, bf)) = (split(&white), split(&black));
        Some(Quotation {
            white: QuotedPlayer { last: wl, first: wf, elo: white_elo },
            black: QuotedPlayer { last: bl, first: bf, elo: black_elo },
            event,
            site,
            date: Date(date),
            kind,
            round,
            subround,
            result,
            eco: Eco::from_field(eco),
            ..Quotation::default()
        })
    }

    /// The quotation as ChessBase writes it in its own PGN export: the result,
    /// both players as `Last,F (Elo)`, the event with its site and its speed
    /// unless the title holds them, the year unless the title holds it, and
    /// the round.
    pub fn chessbase_text(&self) -> String {
        let result = match self.result {
            0 => "0-1",
            1 => "1/2",
            2 => "1-0",
            _ => "*",
        };
        let player = |p: &QuotedPlayer| {
            let (last, first) = (p.last.trim(), p.first.trim());
            let name = match first.chars().next() {
                Some(initial) => format!("{last},{initial}"),
                None => last.to_string(),
            };
            if p.elo > 0 { format!("{name} ({})", p.elo) } else { name }
        };
        let (event, site) = (self.event.trim(), self.site.trim());
        let mut x = event.to_string();
        if !site.is_empty() && !event.contains(site) {
            x.push(' ');
            x.push_str(site);
        }
        let lower = event.to_lowercase();
        for (bit, label) in [(0x20, "blitz"), (0x40, "rapid")] {
            if self.kind & bit != 0 && !lower.contains(label) {
                x.push(' ');
                x.push_str(label);
            }
        }
        let year = self.date.year();
        if year > 0 && !x.contains(&year.to_string()) {
            x.push_str(&format!(" {year}"));
        }
        let sub = i16::from(self.subround) as u16;
        match (self.round, sub) {
            (0, 0) => {}
            (0, s) if self.kind & 0x80 != 0 => x.push_str(&format!(" [{s}]")),
            (r, 0) => x.push_str(&format!(" ({r})")),
            (r, s) => x.push_str(&format!(" ({r}.{s})")),
        }
        format!("{result} {}-{} {x}", player(&self.white), player(&self.black))
    }
}
