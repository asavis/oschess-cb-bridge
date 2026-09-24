//! Game quotations (type `13`): a game quoted in a comment, with a header of
//! its own and, in 2CBH, optionally its moves. The layouts are in
//! `docs/format-notes.md`; what is not understood is kept in the annotation's
//! data, which the full PGN form carries whole.

use super::decode_text;
use crate::v2::{Date, Eco};

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

/// Reads bytes in order, `None` past the end.
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
    /// A 2CBH string: a length byte that counts the terminating zero, the
    /// text and the zero.
    fn counted(&mut self) -> Option<String> {
        let n = self.u8()? as usize;
        let s = self.take(n)?;
        Some(decode_text(s.strip_suffix(&[0]).unwrap_or(s)))
    }
    /// A classic string: a length byte, the text and a zero.
    fn classic(&mut self) -> Option<String> {
        let n = self.u8()? as usize;
        let s = self.take(n)?;
        self.take(1)?;
        Some(decode_text(s))
    }
}

impl Quotation {
    /// A 2CBH quotation's data, the bytes after the type. `None` when it does
    /// not have the known layout.
    pub fn parse_2cbh(data: &[u8]) -> Option<Quotation> {
        let mut c = Cursor { b: data, i: 0 };
        c.take(10)?; // 01, mode, unknown, int 1, zero
        let (wl, wf, bl, bf) = (c.counted()?, c.counted()?, c.counted()?, c.counted()?);
        let (site, event) = (c.counted()?, c.counted()?);
        let fixed = c.take(79)?;
        let le16 = |o: usize| u16::from_le_bytes([fixed[o], fixed[o + 1]]);
        let mut q = Quotation {
            white: QuotedPlayer { last: wl, first: wf, elo: le16(28) },
            black: QuotedPlayer { last: bl, first: bf, elo: le16(30) },
            event,
            site,
            date: Date(i32::from_le_bytes(fixed[0..4].try_into().ok()?)),
            kind: fixed[4],
            round: fixed[43],
            subround: fixed[44] as i8,
            result: fixed[34],
            eco: Eco::from_field(le16(32)),
            ..Quotation::default()
        };
        for _ in 0..2 {
            c.take(5)?; // 01 00 01 00 00
            let n = usize::try_from(c.le32()?).ok()?;
            c.take(n)?;
        }
        c.take(26)?;
        match c.u8()? {
            1 => {}
            0 => {
                c.take(64 + 11)?;
                q.set_up = true;
            }
            _ => return None,
        }
        c.take(2)?;
        let n = usize::try_from(c.le32()?).ok()?;
        let moves = c.take(n.checked_mul(5)?)?;
        if !q.set_up {
            q.moves = moves.as_chunks::<5>().0.iter().map(|m| [m[0], m[1]]).collect();
        }
        Some(q)
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
