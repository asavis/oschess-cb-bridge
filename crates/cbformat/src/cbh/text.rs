//! Text in the classic format: its fixed-size string fields, and the title of
//! a guiding text. Unlike 2CBH, whose text header names its title as an
//! entity, the classic format keeps the titles in the text's own `.cbg`
//! record, one per language, before the text itself.

use super::{Database, Record};
use crate::Result;
use crate::codepage::{CodePage, Evidence, cyrillic_utf8_runs};
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
/// ([`crate::codepage::cyrillic_or_western`]), else in `fallback`: the page the text
/// around it shows, which the caller finds, or the computer's. A byte at
/// 0xa2-0xa7, which ChessBase's chess fonts draw as a piece, is then the
/// figurine ♔ ♕ ♘ ♗ ♖ ♙ where it stands as a piece ([`is_piece`]: `¤d7`),
/// and the page's character elsewhere (Ukrainian `Ґалаґан`, `£100`).
/// ChessBase's diagram mark, 0x9e touching no letter, is dropped
/// ([`is_diagram_mark`]). A run
/// of Cyrillic UTF-8 inside the text ([`cyrillic_utf8_runs`]) is read as UTF-8. Where
/// the computer's page is another, such as Windows-1250, whose 0xa3 and 0xa5
/// are the letters Ł and Ą, the text is read in it as it stands.
pub(super) fn single_byte(b: &[u8], page: CodePage, fallback: CodePage) -> String {
    if !detects(page) {
        return page.decode(b);
    }
    let runs = cyrillic_utf8_runs(b);
    let read = Evidence::outside(b, &runs).page().unwrap_or(fallback);
    let letter = |c: u8| c.is_ascii_alphabetic() || read.char(c).is_alphabetic();
    // The character byte `j` reads as: a figurine, a sign or the page's.
    let char_at = |j: usize| {
        let sign = || sign(b[j], read).filter(|_| !b.get(j + 1).is_some_and(|&c| letter(c)));
        figurine(b[j]).filter(|_| is_piece(b, j)).or_else(sign).unwrap_or_else(|| read.char(b[j]))
    };
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    for run in runs.iter().chain([&(b.len()..b.len())]) {
        while i < run.start {
            // The characters on either side as they read, a UTF-8 run's
            // included.
            let next = if i + 1 == run.start {
                std::str::from_utf8(&b[run.clone()]).ok().and_then(|s| s.chars().next())
            } else {
                b.get(i + 1).map(|_| char_at(i + 1))
            };
            if b[i] == 0x9e && is_diagram_mark(out.chars().next_back(), next) {
                i += 1;
                continue;
            }
            out.push(char_at(i));
            i += 1;
        }
        out.push_str(std::str::from_utf8(&b[run.clone()]).unwrap_or_default());
        i = run.end;
    }
    out
}

/// What the words of single-byte text `b` show of its page, the words of its
/// UTF-8 runs ([`cyrillic_utf8_runs`]) left out: the evidence a text, its game's texts
/// and a database's names give (#306).
pub(super) fn evidence(b: &[u8]) -> Evidence {
    Evidence::outside(b, &cyrillic_utf8_runs(b))
}

/// Whether a byte 0x9e between the characters `prev` and `next`, as the text
/// reads them, is ChessBase's diagram mark (#314): it is where neither is a
/// letter. Windows-1251 reads the byte as `ћ` and Windows-1252 as `ž`.
/// ChessBase's text writes the mark after the word for a diagram (`Diagram
/// <mark>`) and as a comment of its own, where the reader of 2CBH drops its
/// counterpart U+E005 (`crate::signs`); a letter beside it keeps the page's
/// letter (`Božidar`, Serbian `ћ`).
fn is_diagram_mark(prev: Option<char>, next: Option<char>) -> bool {
    !prev.is_some_and(char::is_alphabetic) && !next.is_some_and(char::is_alphabetic)
}

/// The chess sign ChessBase's fonts draw for byte `b` of text read in `read`,
/// where the page has a letter or another sign there that a chess text does
/// not mean (`docs/format-notes.md`, "Signs"): ½ at 0xbd, which
/// Windows-1251 reads as the Macedonian Ѕ (`Ѕ-Ѕ` for a draw), and ∓ at 0xb5,
/// which both pages read as µ (`♕d8-b6µ`). [`single_byte`] takes it only
/// where no letter follows, so a word keeps its letter.
fn sign(b: u8, read: CodePage) -> Option<char> {
    match b {
        0xbd if read == CodePage::CYRILLIC => Some('½'),
        0xb5 => Some('∓'),
        _ => None,
    }
}

/// Whether the piece byte `b[i]` of single-byte text stands as a piece, as
/// ChessBase's comments write them. A whole move must follow it, possibly
/// after one of ` `, `.`, `,` or `-`, and end before anything but an ASCII
/// letter or digit ([`is_move`]; Russian comments often run a word on,
/// `¦b1и`): `¤d7`, `¦fe1`, `¦1d5`, `¤3:h4`, `¥xf3`, `¦:f6`, `¥d7-e6`,
/// `¤7-b6`, `¤ :d5`, `¤ : h3`, `¥ xc6+`, `¤.f6`, `¢-b4`, `£f7x` with a mate
/// sign, and а, с, е and х typed in Cyrillic (`¤с3`, `¥хf3`). Without a move it is a piece only after a square, as a
/// promotion that no ASCII letter or digit follows (`d8£`, `c8£#`), beside
/// another piece (`¦¥1`, `¥¤9`), and across a plus from one (`£+¤`).
///
/// Anywhere else it is the page's character: a letter of a word (`Ґалаґан`,
/// `Ґаєвський`, `Јасна`) or a sign (`£5`, `£5-£10`, `£5bn`, `§4-6`). A piece
/// standing alone (`¥ (any move)`) or in a broken move (`¤qd7`) is taken for
/// a character too.
fn is_piece(b: &[u8], i: usize) -> bool {
    let piece = |c: u8| figurine(c).is_some();
    let edge = |rest: &[u8]| rest.first().is_none_or(|c| !c.is_ascii_alphanumeric());
    let (before, after) = (&b[..i], &b[i + 1..]);
    let follows = match after {
        [c, ..] if piece(*c) => true,
        [b'+', c, ..] if piece(*c) => true,
        [b' ' | b'.' | b',' | b'-', rest @ ..] => is_move(rest, edge),
        rest => is_move(rest, edge),
    };
    follows
        || match before {
            [.., c] if piece(*c) => true,
            [.., p, b'+'] => piece(*p),
            [.., f, r] => is_file(*f) && is_rank(*r) && edge(after),
            _ => false,
        }
}

/// a-h, and а, с, е typed in Cyrillic for a, c, e.
fn is_file(c: u8) -> bool {
    matches!(c, b'a'..=b'h' | 0xe0 | 0xf1 | 0xe5)
}

fn is_rank(c: u8) -> bool {
    matches!(c, b'1'..=b'8')
}

/// x, х typed in Cyrillic, and the colon Russian notation writes.
fn is_capture(c: u8) -> bool {
    matches!(c, b'x' | 0xf5 | b':')
}

/// Whether `s` starts with a move after its piece, which `edge` says ends
/// where a word would, or before a mate sign `x` or `X` that ends so: a
/// square, which a file, a rank or a square may come before, and then a
/// capture or, after them, a dash; the capture may have a space after it
/// (`d7`, `fe1`, `1d5`, `3:h4`, `xf3`, `: h3`, `d7-e6`, `7-b6`, `f7x`).
fn is_move(s: &[u8], edge: impl Fn(&[u8]) -> bool) -> bool {
    let square = |at: usize| s.len() >= at + 2 && is_file(s[at]) && is_rank(s[at + 1]);
    let ends = |at: usize| edge(&s[at..]) || (matches!(s.get(at), Some(b'x' | b'X')) && edge(&s[at + 1..]));
    let from = [Some(0), s.first().filter(|&&c| is_file(c) || is_rank(c)).map(|_| 1), square(0).then_some(2)];
    from.into_iter().flatten().any(|d| {
        let mut at = d;
        if s.get(at).is_some_and(|&c| is_capture(c) || (d > 0 && c == b'-')) {
            at += 1;
            if s.get(at) == Some(&b' ') {
                at += 1;
            }
        }
        square(at) && ends(at + 2)
    })
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

    /// ChessBase's diagram mark, 0x9e touching no letter, is dropped in both
    /// pages' text; beside a letter it is the page's letter (#314). Made-up
    /// texts.
    #[test]
    fn the_diagram_mark_is_dropped() {
        for page in [WESTERN, CYRILLIC] {
            // `Диаграмма` and the mark, in Windows-1251.
            assert_eq!(single_byte(b"\xc4\xe8\xe0\xe3\xf0\xe0\xec\xec\xe0 \x9e", page, page), "Диаграмма ");
            assert_eq!(single_byte(b"Diagram \x9e", page, page), "Diagram ");
            assert_eq!(single_byte(b"\x9e", page, page), "");
            assert_eq!(single_byte(b"\x9e\r\nWhite is better.", page, page), "\r\nWhite is better.");
        }
        // Between letters it stays the page's letter.
        assert_eq!(single_byte(b"Bo\x9eidar", WESTERN, WESTERN), "Božidar");
        assert_eq!(single_byte(b"Bo\x9eidar", CYRILLIC, CYRILLIC), "Boћidar", "a name that shows no page");
        // Serbian `Ћирић`, in Windows-1251: its ћ ends a word.
        assert_eq!(single_byte(b"\x8e\xe8\xf0\xe8\x9e", CYRILLIC, CYRILLIC), "Ћирић");
        // Beside a UTF-8 run, the run's characters decide: a letter keeps the
        // byte, a sign drops it.
        let after_word = ["Работа".as_bytes(), b"\x9e"].concat();
        assert_eq!(single_byte(&after_word, CYRILLIC, CYRILLIC), "Работаћ");
        let before_word = [&b"\x9e"[..], "Ходы".as_bytes()].concat();
        assert_eq!(single_byte(&before_word, CYRILLIC, CYRILLIC), "ћХоды");
        let after_sign = ["Ход 😀".as_bytes(), b"\x9e"].concat();
        assert_eq!(single_byte(&after_sign, CYRILLIC, CYRILLIC), "Ход 😀");
        // A figurine beside it is no letter: `ў` reads as ♔ before a square.
        assert_eq!(single_byte(b"\x9e\xa2g2", CYRILLIC, CYRILLIC), "♔g2");
    }

    /// ChessBase's piece bytes are figurines where they stand as pieces in
    /// both pages' text, and letters and signs elsewhere; another page's
    /// letters there are never figurines.
    #[test]
    fn piece_bytes_are_figurines() {
        // `Ход 3...¤d7 4.¥xf3 £d2 ¦e1 ¢g2 §e4`, in Windows-1251.
        let moves = b"\xd5\xee\xe4 3...\xa4d7 4.\xa5xf3 \xa3d2 \xa6e1 \xa2g2 \xa7e4";
        for page in [WESTERN, CYRILLIC] {
            assert_eq!(single_byte(moves, page, page), "Ход 3...♘d7 4.♗xf3 ♕d2 ♖e1 ♔g2 ♙e4");
            assert_eq!(single_byte(b"\xa3+\xa4", page, page), "♕+♘");
            // `¦fe1 ¦1d5 ¤3:h4 ¥d7-e6 ¦:f6 d8£ c8£# ¦¥1 £+¤`, `¤ e5 ¤ :d5 ¥ xc6+
            // ¤.f6 ¢-b4 ¤ : h3 ¤7-b6 £f7x`, `¤с3 ¥хf3` with Cyrillic с and х, and
            // `¦b1и` run on.
            let more = b"\xa6fe1 \xa61d5 \xa43:h4 \xa5d7-e6 \xa6:f6 d8\xa3 c8\xa3# \xa6\xa51 \xa3+\xa4 \
                \xa4 e5 \xa4 :d5 \xa5 xc6+ \xa4.f6 \xa2-b4 \xa4 : h3 \xa47-b6 \xa3f7x \xa4\xf13 \xa5\xf5f3 \xa6b1\xe8";
            assert_eq!(
                single_byte(more, page, CYRILLIC),
                "♖fe1 ♖1d5 ♘3:h4 ♗d7-e6 ♖:f6 d8♕ c8♕# ♖♗1 ♕+♘ ♘ e5 ♘ :d5 ♗ xc6+ ♘.f6 ♔-b4 ♘ : h3 ♘7-b6 ♕f7x ♘с3 ♗хf3 ♖b1и"
            );
            // Endgame classes: `¥¤9`, `¦¥23`.
            assert_eq!(single_byte(b"\xa5\xa49 \xa6\xa523", page, page), "♗♘9 ♖♗23");
            // Names in Windows-1251, not pieces: Ukrainian `Ґалаґан` and
            // `Ґаєвський`, whose є is a letter below 0xc0, and Serbian
            // `Јасна`, whose а and с look like files.
            assert_eq!(single_byte(b"\xa5\xe0\xeb\xe0\xb4\xe0\xed", page, page), "Ґалаґан");
            assert_eq!(single_byte(b"\xa5\xe0\xba\xe2\xf1\xfc\xea\xe8\xe9", page, page), "Ґаєвський");
            assert_eq!(single_byte(b"\xa3\xe0\xf1\xed\xe0", page, page), "Јасна");
        }
        // Cyrillic UTF-8 after Windows-1251: `Ход? Ход белых`, the second in
        // UTF-8, and `СССР` in Windows-1251, which is no UTF-8.
        let mixed = [&b"\xd5\xee\xe4? "[..], "Ход белых".as_bytes()].concat();
        assert_eq!(single_byte(&mixed, WESTERN, CYRILLIC), "Ход? Ход белых");
        assert_eq!(single_byte(&mixed, CYRILLIC, CYRILLIC), "Ход? Ход белых");
        assert_eq!(cyrillic_utf8_runs(b"\xd1\xd1\xd1\xd0"), []);
        // A sequence that encodes no character, a surrogate or an overlong
        // form, ends a run and keeps the runs on either side.
        let surrogate = [&mixed[..], b"\xed\xa0\x80"].concat();
        // The surrogate's bytes read in Windows-1251: н, a no-break space, Ђ.
        assert_eq!(single_byte(&surrogate, CYRILLIC, CYRILLIC), "Ход? Ход белыхн\u{a0}Ђ");
        // A character of four bytes stays inside a run.
        let emoji = [&b"\xd5\xee\xe4? "[..], "Ход 😀 белых".as_bytes()].concat();
        assert_eq!(single_byte(&emoji, CYRILLIC, CYRILLIC), "Ход? Ход 😀 белых");
        let above = ["Ход".as_bytes(), b"\xf4\x90\x80\x80", "белых".as_bytes()].concat();
        assert!(single_byte(&above, CYRILLIC, CYRILLIC).starts_with("Ход"), "above U+10FFFF ends a run");
        assert!(single_byte(&above, CYRILLIC, CYRILLIC).ends_with("белых"));
        // The runs' words show no page: `1ª División` stays Western.
        let ordinal = [&b"1\xaa Divisi\xf3n "[..], "Ход белых".as_bytes()].concat();
        assert_eq!(single_byte(&ordinal, WESTERN, WESTERN), "1ª División Ход белых");
        let overlong = ["Ход".as_bytes(), b"\xe0\x80\x80", "белых".as_bytes()].concat();
        assert!(single_byte(&overlong, CYRILLIC, CYRILLIC).starts_with("Ход"));
        assert!(single_byte(&overlong, CYRILLIC, CYRILLIC).ends_with("белых"));
        let runs = cyrillic_utf8_runs("Ход".as_bytes());
        assert_eq!((runs.len(), runs.first()), (1, Some(&(0..6))));
        assert_eq!(cyrillic_utf8_runs("Хо".as_bytes()), [], "two letters are not enough");
        // ½ and ∓ in either page, and not inside a word: Macedonian `Ѕвезда`,
        // `5µm`.
        for page in [WESTERN, CYRILLIC] {
            assert_eq!(single_byte(b"\xbd-\xbd, 6\xbd \xa3d8-b6\xb5 \xb5/-+", page, page), "½-½, 6½ ♕d8-b6∓ ∓/-+");
            assert_eq!(single_byte(b"5\xb5m", page, page), "5µm");
        }
        assert_eq!(single_byte(b"\xbd\xe2\xe5\xe7\xe4\xe0", CYRILLIC, CYRILLIC), "Ѕвезда");
        // Signs, not pieces.
        for (bytes, text) in [
            (&b"Prize \xa3100"[..], "Prize £100"),
            (b"Prize \xa35", "Prize £5"),
            (b"Prize \xa31.50", "Prize £1.50"),
            (b"See \xa74", "See §4"),
            (b"\xa2 and \xa5 signs", "¢ and ¥ signs"),
            (b"\xa3 50 or \xa7 4", "£ 50 or § 4"),
            (b"Prize \xa35-\xa310", "Prize £5-£10"),
            (b"Prize \xa35bn", "Prize £5bn"),
            (b"See \xa74-6", "See §4-6"),
            // A broken move, and a piece standing alone.
            (b"\xa4qd7, \xa5 (any)", "¤qd7, ¥ (any)"),
        ] {
            assert_eq!(single_byte(bytes, WESTERN, WESTERN), text);
        }
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
