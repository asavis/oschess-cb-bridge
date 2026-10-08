//! Text in the classic format: its fixed-size string fields, and the title of
//! a guiding text. Unlike 2CBH, whose text header names its title as an
//! entity, the classic format keeps the titles in the text's own `.cbg`
//! record, one per language, before the text itself.

use super::{Database, Record};
use crate::Result;
use crate::codepage::{self, CodePage};
use crate::game::RecordKind;

/// The bytes before the first title: flags, size, version and title count.
const HEAD: usize = 8;

/// A fixed-size string field, up to the first zero byte; the bytes after the
/// terminator are leftovers and are ignored. The format stores single-byte
/// text, but ChessBase also writes UTF-8 into these fields (seen in a
/// database it converted from 2CBH). Bytes that are valid UTF-8 are read as
/// UTF-8, the rest by [`single_byte`] on a computer whose code page is
/// `page`, in `fallback` where their own words show no page: a genuine
/// single-byte name almost never forms valid multi-byte UTF-8. A UTF-8 text
/// cut at the field's width may end in part of a character, which is dropped.
pub(super) fn text(field: &[u8], page: CodePage, fallback: CodePage) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    match std::str::from_utf8(&field[..end]) {
        Ok(s) => return s.to_owned(),
        Err(e) if e.error_len().is_none() => {
            let valid = &field[..e.valid_up_to()];
            if valid.iter().any(|&b| b >= 0x80) {
                return String::from_utf8_lossy(valid).into_owned();
            }
        }
        Err(_) => {}
    }
    single_byte(&field[..end], page, fallback)
}

/// Single-byte text of a classic database, on a computer whose code page is
/// `page` (`docs/format-notes.md`, "Text encoding"). ChessBase's Russian and
/// Western databases hold Windows-1251 and Windows-1252 text, and nothing in
/// a record says which. Where the computer's page is one of the two
/// ([`detects`]), each text is read in the one its words show
/// ([`codepage::cyrillic_or_western`]), else in `fallback`: the page the text
/// around it shows, which the caller finds, or the computer's. A byte at
/// 0xa2-0xa7, which ChessBase's chess fonts draw as a piece, is then the
/// figurine ♔ ♕ ♘ ♗ ♖ ♙ where it stands as a piece ([`is_piece`]: `¤d7`),
/// and the page's character elsewhere (Ukrainian `Ґалаґан`, `£100`). Where
/// the computer's page is another, such as Windows-1250, whose 0xa3 and 0xa5
/// are the letters Ł and Ą, the text is read in it as it stands.
pub(super) fn single_byte(b: &[u8], page: CodePage, fallback: CodePage) -> String {
    if !detects(page) {
        return page.decode(b);
    }
    let read = codepage::cyrillic_or_western(b).unwrap_or(fallback);
    (0..b.len())
        .map(|i| {
            let piece = |i: usize| figurine(b[i]).is_some() && is_piece(&b[i + 1..]);
            // The second of two pieces is one too, whatever follows (`¥¤9`).
            let second = i > 0 && piece(i - 1) && figurine(b[i]).is_some();
            figurine(b[i]).filter(|_| second || piece(i)).unwrap_or_else(|| read.char(b[i]))
        })
        .collect()
}

/// Whether a piece byte followed by `after` stands as a piece in a move, as
/// ChessBase's comments write them: before a file that a square, a second
/// file, a capture or no letter follows (`¤d7`, `¦fe1`, `¥b+`, `¦a-e8`, and a
/// file typed in Cyrillic, `¤с3`), before a rank that no digit follows
/// (`¦1d5`), before a capture (`¥xf3`, `¦:f6`), and before anything but a
/// letter or a digit (`d8£`, `£+¤`, `¦¥1`); [`single_byte`] takes the second
/// of two pieces for one too (`¥¤9`, `¦¥23`, endgame classes). Before other
/// letters it is a letter of a word (`Ґалаґан`), and before a number a sign
/// (`£100`).
fn is_piece(after: &[u8]) -> bool {
    // a-h, and а, с, е typed in Cyrillic for a, c, e.
    let file = |c: u8| matches!(c, b'a'..=b'h' | 0xe0 | 0xf1 | 0xe5);
    // x, х typed in Cyrillic, and the colon Russian notation writes.
    let capture = |c: u8| matches!(c, b'x' | 0xf5 | b':');
    let letter = |c: u8| c.is_ascii_alphabetic() || c >= 0xc0;
    match after {
        [] => true,
        [c, ..] if capture(*c) => true,
        [c, rest @ ..] if file(*c) => rest.first().is_none_or(|&d| !letter(d) || file(d) || capture(d)),
        [b'1'..=b'8', rest @ ..] => rest.first().is_none_or(|d| !d.is_ascii_digit()),
        [c, ..] => !letter(*c) && !c.is_ascii_digit(),
    }
}

/// Whether text read on a computer whose code page is `page` is read in the
/// page its words show: on a Cyrillic or a Western computer.
pub(super) fn detects(page: CodePage) -> bool {
    page == CodePage::CYRILLIC || page == CodePage::WESTERN
}

/// The piece ChessBase's chess fonts draw for byte `b`: king, queen, knight,
/// bishop, rook and pawn at 0xa2-0xa7.
fn figurine(b: u8) -> Option<char> {
    Some(match b {
        0xa2 => '♔',
        0xa3 => '♕',
        0xa4 => '♘',
        0xa5 => '♗',
        0xa6 => '♖',
        0xa7 => '♙',
        _ => return None,
    })
}

impl Database {
    /// The title of guiding text `record`: its first title that is not blank,
    /// whatever its language; empty when it has none, and for a game. At most
    /// `limit` bytes of the record are read, and a title that ends past them
    /// ends the list.
    pub fn text_title(&self, record: &Record, limit: usize) -> Result<String> {
        if record.kind() != RecordKind::Text {
            return Ok(String::new());
        }
        let (at, size) = self.move_extent(record)?;
        let mut buf = vec![0u8; size.min(limit.max(HEAD))];
        self.moves.read_into(at, &mut buf)?;
        Ok(first_title(&buf, self.page, self.entities.fallback()))
    }
}

/// The first title of a text record's head that is not blank, read on a
/// computer whose code page is `page`, in `fallback` where its words show no
/// page.
fn first_title(b: &[u8], page: CodePage, fallback: CodePage) -> String {
    let Some(count) = b.get(6..HEAD).map(|c| u16::from_le_bytes([c[0], c[1]])) else { return String::new() };
    let mut at = HEAD;
    for _ in 0..count {
        let Some(len) = b.get(at + 2..at + 4).map(|l| u16::from_le_bytes([l[0], l[1]]) as usize) else { break };
        let Some(title) = b.get(at + 4..at + 4 + len) else { break };
        let t = text(title, page, fallback);
        if !t.trim().is_empty() {
            return t;
        }
        at += 4 + len;
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(titles: &[(u16, &[u8])]) -> Vec<u8> {
        let mut r = vec![0x80, 0, 0, 0, 1, 0];
        r.extend((titles.len() as u16).to_le_bytes());
        for (language, t) in titles {
            r.extend(language.to_le_bytes());
            r.extend((t.len() as u16).to_le_bytes());
            r.extend(*t);
        }
        r
    }

    const WESTERN: CodePage = CodePage::WESTERN;
    const CYRILLIC: CodePage = CodePage::CYRILLIC;

    #[test]
    fn single_byte_text() {
        let text = |b: &[u8]| text(b, WESTERN, WESTERN);
        assert_eq!(text(b"Anand\0junk"), "Anand");
        assert_eq!(text(b"Full"), "Full");
        assert_eq!(text(&[0x4d, 0xfc, 0x6c, 0x6c, 0x65, 0x72, 0]), "Müller");
        assert_eq!(text(&[0x80, 0x96, 0]), "€–");
        // Valid UTF-8 is read as UTF-8.
        assert_eq!(text("Łódź\0".as_bytes()), "Łódź");
        assert_eq!(text(&[0x4d, 0xc3, 0xbc, 0x6c, 0]), "Mül");
        // UTF-8 cut inside its last character: the part is dropped...
        assert_eq!(text(&[0xc3, 0xbc, 0x41, 0xc5]), "üA");
        // ...but a single-byte text is not taken for cut UTF-8.
        assert_eq!(text(&[0x41, 0xe2]), "Aâ");
    }

    /// Each text is read in the page its words show, on a Western computer
    /// and on a Cyrillic one alike; text that shows neither, in the
    /// computer's page.
    #[test]
    fn single_byte_text_is_read_in_the_page_its_words_show() {
        for page in [WESTERN, CYRILLIC] {
            for fallback in [WESTERN, CYRILLIC] {
                assert_eq!(single_byte(b"\xcf\xe5\xf2\xf0\xee\xe2, \xc8.", page, fallback), "Петров, И.");
                assert_eq!(single_byte(b"H\xfcbner, R", page, fallback), "Hübner, R");
                assert_eq!(single_byte(b"\xd4c2", page, fallback), "Фc2");
            }
        }
        // `и т.д.` shows no page: the fallback decides.
        assert_eq!(single_byte(b"\xe8 \xf2.\xe4.", CYRILLIC, WESTERN), "è ò.ä.");
        assert_eq!(single_byte(b"\xe8 \xf2.\xe4.", WESTERN, CYRILLIC), "и т.д.");
        assert_eq!(text(b"\xcf\xe5\xf2\xf0\xee\xe2\0", WESTERN, WESTERN), "Петров");
    }

    /// ChessBase's piece bytes are figurines where they stand as pieces in
    /// both pages' text, and letters and signs elsewhere; another page's
    /// letters there are never figurines.
    #[test]
    fn piece_bytes_are_figurines() {
        // `Ход 3...¤d7 4.¥xf3 £d2 ¦e1 ¢g2 §`, in Windows-1251.
        let moves = b"\xd5\xee\xe4 3...\xa4d7 4.\xa5xf3 \xa3d2 \xa6e1 \xa2g2 \xa7";
        for page in [WESTERN, CYRILLIC] {
            assert_eq!(single_byte(moves, page, page), "Ход 3...♘d7 4.♗xf3 ♕d2 ♖e1 ♔g2 ♙");
            assert_eq!(single_byte(b"\xa3+\xa4", page, page), "♕+♘");
            // `¦fe1 ¦1d5 ¦:f6 d8£ ¦¥1 ¥b+ ¦a-e8`, and `¤с3 ¥хf3` with Cyrillic с and х.
            let more = b"\xa6fe1 \xa61d5 \xa6:f6 d8\xa3 \xa6\xa51 \xa5b+ \xa6a-e8 \xa4\xf13 \xa5\xf5f3";
            assert_eq!(single_byte(more, page, CYRILLIC), "♖fe1 ♖1d5 ♖:f6 d8♕ ♖♗1 ♗b+ ♖a-e8 ♘с3 ♗хf3");
            // Endgame classes: `¥¤9`, `¦¥23`.
            assert_eq!(single_byte(b"\xa5\xa49 \xa6\xa523", page, page), "♗♘9 ♖♗23");
            // `Ґалаґан` in Windows-1251: a Ukrainian name, not a bishop.
            assert_eq!(single_byte(b"\xa5\xe0\xeb\xe0\xb4\xe0\xed", page, page), "Ґалаґан");
        }
        assert_eq!(single_byte(b"Prize \xa3100", WESTERN, WESTERN), "Prize £100");
        assert_eq!(single_byte(b"\xa2 and \xa5 signs", WESTERN, WESTERN), "♔ and ♗ signs");
        // Windows-1250: `Łódź`, whatever a fallback says.
        let central = CodePage::new(1250);
        assert_eq!(single_byte(b"\xa3\xf3d\x9f", central, CYRILLIC), "Łódź");
    }

    #[test]
    fn the_first_title_that_is_not_blank() {
        let first_title = |b: &[u8]| first_title(b, WESTERN, WESTERN);
        assert_eq!(first_title(&record(&[(1, b"Schach"), (0, b"Chess")])), "Schach");
        assert_eq!(first_title(&record(&[(0, b" "), (1, b"Er\xf6ffnung")])), "Eröffnung");
        assert_eq!(first_title(&record(&[])), "");
        assert_eq!(first_title(&[0x80, 0, 0]), "");
        // A title cut by the read ends the list.
        let whole = record(&[(0, b""), (1, b"Endgame")]);
        assert_eq!(first_title(&whole[..whole.len() - 1]), "");
    }
}
