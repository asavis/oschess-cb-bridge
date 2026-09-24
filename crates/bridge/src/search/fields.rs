//! A game record's fields as the list shows them, without allocating.

use std::io::Write;

use cbformat::v2::Eco;

use crate::store::Head;

/// A record's date as it appears in PGN, `YYYY.MM.DD` with `?` for unknown parts.
pub fn date_text(r: &impl Head) -> [u8; 10] {
    let d = r.played_date();
    let mut out = *b"????.??.??";
    let mut put = |at: usize, width: usize, v: u32| {
        if v != 0 {
            let mut v = v;
            for i in (0..width).rev() {
                out[at + i] = b'0' + (v % 10) as u8;
                v /= 10;
            }
        }
    };
    put(0, 4, u32::from(d.year()));
    put(5, 2, u32::from(d.month()));
    put(8, 2, u32::from(d.day()));
    out
}

/// A record's ECO code, `A00`..`E99`, when it has one.
pub fn eco_text(r: &impl Head) -> Option<[u8; 3]> {
    match r.eco() {
        Eco::Code { code, .. } => {
            Some([b'A' + (code / 100) as u8, b'0' + (code / 10 % 10) as u8, b'0' + (code % 10) as u8])
        }
        _ => None,
    }
}

/// A record's round as the list shows it: `5`, `5(2)`, or empty.
pub fn round_text<'a>(r: &impl Head, buf: &'a mut [u8; 16]) -> &'a str {
    let len = {
        let mut w = &mut buf[..];
        let _ = match r.round() {
            (n, _) if n <= 0 => Ok(()),
            (n, s) if s <= 0 => write!(w, "{n}"),
            (n, s) => write!(w, "{n}({s})"),
        };
        16 - w.len()
    };
    std::str::from_utf8(&buf[..len]).unwrap_or("")
}
