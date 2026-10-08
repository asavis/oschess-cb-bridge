//! Text in a Windows single-byte code page: what an older program wrote before
//! it wrote UTF-8. A PGN file of such a program is read with the code page of
//! the computer it is on, as ChessBase reads it (`crate::pgnfile`); the text of
//! a classic database also by its own letters ([`cyrillic_or_western`]).

use std::cmp::Ordering;

/// A Windows ANSI code page. Pages 1250 to 1258 have tables of their own;
/// any other page, such as a multi-byte one, reads as 1252.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodePage(u16);

impl CodePage {
    /// Western European, the default where no other page is known.
    pub const WESTERN: CodePage = CodePage(1252);
    /// Cyrillic, the page of Russian and Ukrainian text.
    pub const CYRILLIC: CodePage = CodePage(1251);

    /// Page `number`: 1250 to 1258, else [`CodePage::WESTERN`].
    pub fn new(number: u32) -> CodePage {
        match u16::try_from(number) {
            Ok(n @ 1250..=1258) => CodePage(n),
            _ => CodePage::WESTERN,
        }
    }

    pub fn number(self) -> u16 {
        self.0
    }

    /// The character of byte `b`. A byte the page leaves undefined reads as
    /// the C1 control or Latin-1 character of the same value, so no byte is
    /// lost.
    pub fn char(self, b: u8) -> char {
        if b < 0x80 {
            return char::from(b);
        }
        let table = match self.0 {
            1250 => &CP1250,
            1251 => &CP1251,
            1253 => &CP1253,
            1254 => &CP1254,
            1255 => &CP1255,
            1256 => &CP1256,
            1257 => &CP1257,
            1258 => &CP1258,
            _ => &CP1252,
        };
        let mapped = char::from_u32(u32::from(table[usize::from(b - 0x80)]));
        mapped.filter(|&c| c != char::REPLACEMENT_CHARACTER).unwrap_or(char::from(b))
    }

    /// `bytes` in this page.
    pub fn decode(self, bytes: &[u8]) -> String {
        bytes.iter().map(|&b| self.char(b)).collect()
    }

    /// `bytes` as UTF-8 when they are valid UTF-8, else in this page.
    pub fn utf8_or(self, bytes: &[u8]) -> String {
        match std::str::from_utf8(bytes) {
            Ok(s) => s.to_string(),
            Err(_) => self.decode(bytes),
        }
    }

    /// `text` in this page, each character as the byte that
    /// [`CodePage::char`] reads as it, so that [`CodePage::decode`] gives the
    /// text back. The first character that no byte of the page reads as is
    /// the error.
    pub fn encode(self, text: &str) -> Result<Vec<u8>, char> {
        let mut bytes: Vec<(char, u8)> = (0x80..=0xff).map(|b| (self.char(b), b)).collect();
        bytes.sort_unstable();
        text.chars()
            .map(|c| match u8::try_from(c) {
                Ok(b) if b < 0x80 => Ok(b),
                _ => bytes.binary_search_by_key(&c, |&(c, _)| c).map(|i| bytes[i].1).map_err(|_| c),
            })
            .collect()
    }
}

/// Letters of Windows-1251 outside 0xc0-0xff: Ё, Є, Ї, І, і, ґ, ё, є, ї.
/// Windows-1252 has symbols there.
const CYRILLIC_ONLY: [u8; 9] = [0xa8, 0xaa, 0xaf, 0xb2, 0xb3, 0xb4, 0xb8, 0xba, 0xbf];

/// `bytes` as UTF-8 when they are valid UTF-8; else in Windows-1251 when their
/// words show Cyrillic ([`cyrillic_or_western`]), and in Windows-1252
/// otherwise. For text a format keeps as UTF-8, such as 2CBH's, which an
/// older program sometimes wrote in a single-byte page: Russian text reads as
/// Russian, and any other text as it did before, on every computer (#308).
pub fn utf8_or_legacy(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) if cyrillic_or_western(bytes) == Some(CodePage::CYRILLIC) => CodePage::CYRILLIC.decode(bytes),
        Err(_) => CodePage::WESTERN.decode(bytes),
    }
}

/// Whether single-byte text reads as Cyrillic ([`CodePage::CYRILLIC`]) or as
/// Western ([`CodePage::WESTERN`]) text, from its words; `None` when its
/// words do not say ([`Evidence`]).
pub fn cyrillic_or_western(b: &[u8]) -> Option<CodePage> {
    Evidence::of(b).page()
}

/// What the words of single-byte text show of its page: Cyrillic or Western.
///
/// Windows-1251 has letters at 0xc0-0xff and Windows-1252 has them there but
/// for `×` and `÷` (Ч and ч in Windows-1251, which never stand inside a
/// Western word), so a word is a run of ASCII letters and those bytes, with
/// the letters only Windows-1251 has. A Russian or Ukrainian word is made of
/// these bytes alone; in a Western word they stand among ASCII letters
/// (`Hübner`). Each word that shows a reading adds its count of them to it, and
/// the larger total decides:
///
/// - Cyrillic: a word of these bytes alone, or one with three of them in a row
///   (`Cлон`, typed with a Latin C), and Russian notation, a piece letter
///   before a square or a capture (`Фc2`, `Крg1`, `Kрg1` with a Latin K,
///   `Л:f6`); a square whose file is typed in Cyrillic (`Rе8`, `N:с2`, `е5`);
///   a letter at 0xc0-0xff that ends a number, as Russian books number
///   problems and variations (`1.49а`; Western text writes no `1.49à`, its
///   ordinals `1ª`, `2º` stand below 0xc0, and its `×` and `÷` stand between
///   numbers, `2×2`); also `№` before a digit.
/// - Western: a word with no more of them than ASCII letters (`für`).
/// - Neither: a word of one letter (French `à`, Russian `в`), a short word
///   with more of them than ASCII letters but no three in a row (`Süß`, `été`),
///   which could be either, and a word whose such letters are all Cyrillic
///   letters that look Latin among ASCII ones (`сorr.` typed with a Cyrillic
///   с), which reads alike either way.
///
/// Evidence adds up, so that texts that show nothing alone can be read as the
/// texts beside them show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Evidence {
    cyrillic: usize,
    western: usize,
}

impl Evidence {
    pub fn of(b: &[u8]) -> Evidence {
        let high = |c: u8| c >= 0xc0 || CYRILLIC_ONLY.contains(&c);
        let letter = |c: u8| c.is_ascii_alphabetic() || high(c);
        let mut e = Evidence::default();
        let mut i = 0;
        while i < b.len() {
            if !letter(b[i]) {
                // `№`, which Windows-1252 has as `¹`.
                if b[i] == 0xb9 && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
                    e.cyrillic += 1;
                }
                i += 1;
                continue;
            }
            let start = i;
            while i < b.len() && letter(b[i]) {
                i += 1;
            }
            let word = &b[start..i];
            let h = word.iter().filter(|&&c| high(c)).count();
            if h == 0 {
                continue;
            }
            let ascii = word.len() - h;
            let run = word.split(|&c| !high(c)).map(<[u8]>::len).max().unwrap_or(0);
            let next = b.get(i).copied();
            let lookalikes = ascii > 0 && word.iter().filter(|&&c| high(c)).all(|c| LOOKALIKES.contains(c));
            let ends_number = matches!(word, [0xc0..=0xff] if word[0] != 0xd7 && word[0] != 0xf7)
                && start > 0
                && b[start - 1].is_ascii_digit()
                && !next.is_some_and(|c| c.is_ascii_alphanumeric());
            if is_notation(word, next) || is_typed_square(word, next) || ends_number {
                e.cyrillic += h;
            } else if lookalikes {
                // Reads alike either way: shows neither.
            } else if word.len() > 1 && (ascii == 0 || run >= 3) {
                e.cyrillic += h;
            } else if word.len() > 1 && h <= ascii {
                e.western += h;
            }
        }
        e
    }

    /// This evidence and `other`'s together.
    pub fn add(&mut self, other: Evidence) {
        self.cyrillic += other.cyrillic;
        self.western += other.western;
    }

    /// The page the evidence shows; `None` when it shows neither more.
    pub fn page(self) -> Option<CodePage> {
        match self.cyrillic.cmp(&self.western) {
            Ordering::Greater => Some(CodePage::CYRILLIC),
            Ordering::Less => Some(CodePage::WESTERN),
            Ordering::Equal => None,
        }
    }
}

/// Windows-1251 letters that look like Latin ones (а с е о р х у, А В Е К М Н О
/// Р С Т Х У), whose bytes Windows-1252 reads as Latin letters with marks.
const LOOKALIKES: [u8; 19] =
    [0xe0, 0xf1, 0xe5, 0xee, 0xf0, 0xf5, 0xf3, 0xc0, 0xc2, 0xc5, 0xca, 0xcc, 0xcd, 0xce, 0xd0, 0xd1, 0xd2, 0xd5, 0xd3];

/// Whether `word`, followed by the byte `next`, is a square whose file is
/// typed in Cyrillic, а, с or е for a, c or e, possibly after a piece's Latin
/// letter: `е5`, `Rе8`, and `с2` of `N:с2`.
fn is_typed_square(word: &[u8], next: Option<u8>) -> bool {
    let file = match word {
        [f] | [b'K' | b'Q' | b'R' | b'B' | b'N', f] => *f,
        _ => return false,
    };
    matches!(file, 0xe0 | 0xf1 | 0xe5) && next.is_some_and(|c| matches!(c, b'1'..=b'8'))
}

/// Whether `word`, followed by the byte `next`, is a move in Russian notation:
/// a piece letter or two (`К`, `Кр`, `Ф`, `Л`, `С`, `П`; any letter at
/// 0xc0-0xff is taken, and the K of Кр may be Latin), then up to three of
/// `a`-`h` and `x` ending at a rank (`Фc2`, `Лbc2`, `Крxg1`, `Kрg1`), or
/// nothing more before a `:` (`Л:f6`).
fn is_notation(word: &[u8], next: Option<u8>) -> bool {
    let word = match word {
        [b'K', c, ..] if *c >= 0xc0 => &word[1..],
        _ => word,
    };
    let pieces = word.iter().take_while(|&&c| c >= 0xc0).count();
    let rest = &word[pieces..];
    if !(1..=2).contains(&pieces) {
        return false;
    }
    let square = !rest.is_empty()
        && rest.len() <= 3
        && rest.iter().all(|&c| matches!(c, b'a'..=b'h' | b'x'))
        && next.is_some_and(|c| matches!(c, b'1'..=b'8'));
    square || (rest.is_empty() && next == Some(b':'))
}

// The bytes 0x80-0xFF of each page, from the Unicode mapping tables of the
// Windows code pages; U+FFFD where a page leaves a byte undefined, which
// `CodePage::char` reads as the byte's own value.
const CP1250: [u16; 128] = [
    0x20AC, 0xFFFD, 0x201A, 0xFFFD, 0x201E, 0x2026, 0x2020, 0x2021, 0xFFFD, 0x2030, 0x0160, 0x2039, 0x015A, 0x0164,
    0x017D, 0x0179, 0xFFFD, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0xFFFD, 0x2122, 0x0161, 0x203A,
    0x015B, 0x0165, 0x017E, 0x017A, 0x00A0, 0x02C7, 0x02D8, 0x0141, 0x00A4, 0x0104, 0x00A6, 0x00A7, 0x00A8, 0x00A9,
    0x015E, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x017B, 0x00B0, 0x00B1, 0x02DB, 0x0142, 0x00B4, 0x00B5, 0x00B6, 0x00B7,
    0x00B8, 0x0105, 0x015F, 0x00BB, 0x013D, 0x02DD, 0x013E, 0x017C, 0x0154, 0x00C1, 0x00C2, 0x0102, 0x00C4, 0x0139,
    0x0106, 0x00C7, 0x010C, 0x00C9, 0x0118, 0x00CB, 0x011A, 0x00CD, 0x00CE, 0x010E, 0x0110, 0x0143, 0x0147, 0x00D3,
    0x00D4, 0x0150, 0x00D6, 0x00D7, 0x0158, 0x016E, 0x00DA, 0x0170, 0x00DC, 0x00DD, 0x0162, 0x00DF, 0x0155, 0x00E1,
    0x00E2, 0x0103, 0x00E4, 0x013A, 0x0107, 0x00E7, 0x010D, 0x00E9, 0x0119, 0x00EB, 0x011B, 0x00ED, 0x00EE, 0x010F,
    0x0111, 0x0144, 0x0148, 0x00F3, 0x00F4, 0x0151, 0x00F6, 0x00F7, 0x0159, 0x016F, 0x00FA, 0x0171, 0x00FC, 0x00FD,
    0x0163, 0x02D9,
];
const CP1251: [u16; 128] = [
    0x0402, 0x0403, 0x201A, 0x0453, 0x201E, 0x2026, 0x2020, 0x2021, 0x20AC, 0x2030, 0x0409, 0x2039, 0x040A, 0x040C,
    0x040B, 0x040F, 0x0452, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0xFFFD, 0x2122, 0x0459, 0x203A,
    0x045A, 0x045C, 0x045B, 0x045F, 0x00A0, 0x040E, 0x045E, 0x0408, 0x00A4, 0x0490, 0x00A6, 0x00A7, 0x0401, 0x00A9,
    0x0404, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x0407, 0x00B0, 0x00B1, 0x0406, 0x0456, 0x0491, 0x00B5, 0x00B6, 0x00B7,
    0x0451, 0x2116, 0x0454, 0x00BB, 0x0458, 0x0405, 0x0455, 0x0457, 0x0410, 0x0411, 0x0412, 0x0413, 0x0414, 0x0415,
    0x0416, 0x0417, 0x0418, 0x0419, 0x041A, 0x041B, 0x041C, 0x041D, 0x041E, 0x041F, 0x0420, 0x0421, 0x0422, 0x0423,
    0x0424, 0x0425, 0x0426, 0x0427, 0x0428, 0x0429, 0x042A, 0x042B, 0x042C, 0x042D, 0x042E, 0x042F, 0x0430, 0x0431,
    0x0432, 0x0433, 0x0434, 0x0435, 0x0436, 0x0437, 0x0438, 0x0439, 0x043A, 0x043B, 0x043C, 0x043D, 0x043E, 0x043F,
    0x0440, 0x0441, 0x0442, 0x0443, 0x0444, 0x0445, 0x0446, 0x0447, 0x0448, 0x0449, 0x044A, 0x044B, 0x044C, 0x044D,
    0x044E, 0x044F,
];
const CP1252: [u16; 128] = [
    0x20AC, 0xFFFD, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0xFFFD,
    0x017D, 0xFFFD, 0xFFFD, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A,
    0x0153, 0xFFFD, 0x017E, 0x0178, 0x00A0, 0x00A1, 0x00A2, 0x00A3, 0x00A4, 0x00A5, 0x00A6, 0x00A7, 0x00A8, 0x00A9,
    0x00AA, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x00AF, 0x00B0, 0x00B1, 0x00B2, 0x00B3, 0x00B4, 0x00B5, 0x00B6, 0x00B7,
    0x00B8, 0x00B9, 0x00BA, 0x00BB, 0x00BC, 0x00BD, 0x00BE, 0x00BF, 0x00C0, 0x00C1, 0x00C2, 0x00C3, 0x00C4, 0x00C5,
    0x00C6, 0x00C7, 0x00C8, 0x00C9, 0x00CA, 0x00CB, 0x00CC, 0x00CD, 0x00CE, 0x00CF, 0x00D0, 0x00D1, 0x00D2, 0x00D3,
    0x00D4, 0x00D5, 0x00D6, 0x00D7, 0x00D8, 0x00D9, 0x00DA, 0x00DB, 0x00DC, 0x00DD, 0x00DE, 0x00DF, 0x00E0, 0x00E1,
    0x00E2, 0x00E3, 0x00E4, 0x00E5, 0x00E6, 0x00E7, 0x00E8, 0x00E9, 0x00EA, 0x00EB, 0x00EC, 0x00ED, 0x00EE, 0x00EF,
    0x00F0, 0x00F1, 0x00F2, 0x00F3, 0x00F4, 0x00F5, 0x00F6, 0x00F7, 0x00F8, 0x00F9, 0x00FA, 0x00FB, 0x00FC, 0x00FD,
    0x00FE, 0x00FF,
];
const CP1253: [u16; 128] = [
    0x20AC, 0xFFFD, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0xFFFD, 0x2030, 0xFFFD, 0x2039, 0xFFFD, 0xFFFD,
    0xFFFD, 0xFFFD, 0xFFFD, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0xFFFD, 0x2122, 0xFFFD, 0x203A,
    0xFFFD, 0xFFFD, 0xFFFD, 0xFFFD, 0x00A0, 0x0385, 0x0386, 0x00A3, 0x00A4, 0x00A5, 0x00A6, 0x00A7, 0x00A8, 0x00A9,
    0xFFFD, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x2015, 0x00B0, 0x00B1, 0x00B2, 0x00B3, 0x0384, 0x00B5, 0x00B6, 0x00B7,
    0x0388, 0x0389, 0x038A, 0x00BB, 0x038C, 0x00BD, 0x038E, 0x038F, 0x0390, 0x0391, 0x0392, 0x0393, 0x0394, 0x0395,
    0x0396, 0x0397, 0x0398, 0x0399, 0x039A, 0x039B, 0x039C, 0x039D, 0x039E, 0x039F, 0x03A0, 0x03A1, 0xFFFD, 0x03A3,
    0x03A4, 0x03A5, 0x03A6, 0x03A7, 0x03A8, 0x03A9, 0x03AA, 0x03AB, 0x03AC, 0x03AD, 0x03AE, 0x03AF, 0x03B0, 0x03B1,
    0x03B2, 0x03B3, 0x03B4, 0x03B5, 0x03B6, 0x03B7, 0x03B8, 0x03B9, 0x03BA, 0x03BB, 0x03BC, 0x03BD, 0x03BE, 0x03BF,
    0x03C0, 0x03C1, 0x03C2, 0x03C3, 0x03C4, 0x03C5, 0x03C6, 0x03C7, 0x03C8, 0x03C9, 0x03CA, 0x03CB, 0x03CC, 0x03CD,
    0x03CE, 0xFFFD,
];
const CP1254: [u16; 128] = [
    0x20AC, 0xFFFD, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0xFFFD,
    0xFFFD, 0xFFFD, 0xFFFD, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A,
    0x0153, 0xFFFD, 0xFFFD, 0x0178, 0x00A0, 0x00A1, 0x00A2, 0x00A3, 0x00A4, 0x00A5, 0x00A6, 0x00A7, 0x00A8, 0x00A9,
    0x00AA, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x00AF, 0x00B0, 0x00B1, 0x00B2, 0x00B3, 0x00B4, 0x00B5, 0x00B6, 0x00B7,
    0x00B8, 0x00B9, 0x00BA, 0x00BB, 0x00BC, 0x00BD, 0x00BE, 0x00BF, 0x00C0, 0x00C1, 0x00C2, 0x00C3, 0x00C4, 0x00C5,
    0x00C6, 0x00C7, 0x00C8, 0x00C9, 0x00CA, 0x00CB, 0x00CC, 0x00CD, 0x00CE, 0x00CF, 0x011E, 0x00D1, 0x00D2, 0x00D3,
    0x00D4, 0x00D5, 0x00D6, 0x00D7, 0x00D8, 0x00D9, 0x00DA, 0x00DB, 0x00DC, 0x0130, 0x015E, 0x00DF, 0x00E0, 0x00E1,
    0x00E2, 0x00E3, 0x00E4, 0x00E5, 0x00E6, 0x00E7, 0x00E8, 0x00E9, 0x00EA, 0x00EB, 0x00EC, 0x00ED, 0x00EE, 0x00EF,
    0x011F, 0x00F1, 0x00F2, 0x00F3, 0x00F4, 0x00F5, 0x00F6, 0x00F7, 0x00F8, 0x00F9, 0x00FA, 0x00FB, 0x00FC, 0x0131,
    0x015F, 0x00FF,
];
const CP1255: [u16; 128] = [
    0x20AC, 0xFFFD, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0xFFFD, 0x2039, 0xFFFD, 0xFFFD,
    0xFFFD, 0xFFFD, 0xFFFD, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0xFFFD, 0x203A,
    0xFFFD, 0xFFFD, 0xFFFD, 0xFFFD, 0x00A0, 0x00A1, 0x00A2, 0x00A3, 0x20AA, 0x00A5, 0x00A6, 0x00A7, 0x00A8, 0x00A9,
    0x00D7, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x00AF, 0x00B0, 0x00B1, 0x00B2, 0x00B3, 0x00B4, 0x00B5, 0x00B6, 0x00B7,
    0x00B8, 0x00B9, 0x00F7, 0x00BB, 0x00BC, 0x00BD, 0x00BE, 0x00BF, 0x05B0, 0x05B1, 0x05B2, 0x05B3, 0x05B4, 0x05B5,
    0x05B6, 0x05B7, 0x05B8, 0x05B9, 0xFFFD, 0x05BB, 0x05BC, 0x05BD, 0x05BE, 0x05BF, 0x05C0, 0x05C1, 0x05C2, 0x05C3,
    0x05F0, 0x05F1, 0x05F2, 0x05F3, 0x05F4, 0xFFFD, 0xFFFD, 0xFFFD, 0xFFFD, 0xFFFD, 0xFFFD, 0xFFFD, 0x05D0, 0x05D1,
    0x05D2, 0x05D3, 0x05D4, 0x05D5, 0x05D6, 0x05D7, 0x05D8, 0x05D9, 0x05DA, 0x05DB, 0x05DC, 0x05DD, 0x05DE, 0x05DF,
    0x05E0, 0x05E1, 0x05E2, 0x05E3, 0x05E4, 0x05E5, 0x05E6, 0x05E7, 0x05E8, 0x05E9, 0x05EA, 0xFFFD, 0xFFFD, 0x200E,
    0x200F, 0xFFFD,
];
const CP1256: [u16; 128] = [
    0x20AC, 0x067E, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0679, 0x2039, 0x0152, 0x0686,
    0x0698, 0x0688, 0x06AF, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x06A9, 0x2122, 0x0691, 0x203A,
    0x0153, 0x200C, 0x200D, 0x06BA, 0x00A0, 0x060C, 0x00A2, 0x00A3, 0x00A4, 0x00A5, 0x00A6, 0x00A7, 0x00A8, 0x00A9,
    0x06BE, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x00AF, 0x00B0, 0x00B1, 0x00B2, 0x00B3, 0x00B4, 0x00B5, 0x00B6, 0x00B7,
    0x00B8, 0x00B9, 0x061B, 0x00BB, 0x00BC, 0x00BD, 0x00BE, 0x061F, 0x06C1, 0x0621, 0x0622, 0x0623, 0x0624, 0x0625,
    0x0626, 0x0627, 0x0628, 0x0629, 0x062A, 0x062B, 0x062C, 0x062D, 0x062E, 0x062F, 0x0630, 0x0631, 0x0632, 0x0633,
    0x0634, 0x0635, 0x0636, 0x00D7, 0x0637, 0x0638, 0x0639, 0x063A, 0x0640, 0x0641, 0x0642, 0x0643, 0x00E0, 0x0644,
    0x00E2, 0x0645, 0x0646, 0x0647, 0x0648, 0x00E7, 0x00E8, 0x00E9, 0x00EA, 0x00EB, 0x0649, 0x064A, 0x00EE, 0x00EF,
    0x064B, 0x064C, 0x064D, 0x064E, 0x00F4, 0x064F, 0x0650, 0x00F7, 0x0651, 0x00F9, 0x0652, 0x00FB, 0x00FC, 0x200E,
    0x200F, 0x06D2,
];
const CP1257: [u16; 128] = [
    0x20AC, 0xFFFD, 0x201A, 0xFFFD, 0x201E, 0x2026, 0x2020, 0x2021, 0xFFFD, 0x2030, 0xFFFD, 0x2039, 0xFFFD, 0x00A8,
    0x02C7, 0x00B8, 0xFFFD, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0xFFFD, 0x2122, 0xFFFD, 0x203A,
    0xFFFD, 0x00AF, 0x02DB, 0xFFFD, 0x00A0, 0xFFFD, 0x00A2, 0x00A3, 0x00A4, 0xFFFD, 0x00A6, 0x00A7, 0x00D8, 0x00A9,
    0x0156, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x00C6, 0x00B0, 0x00B1, 0x00B2, 0x00B3, 0x00B4, 0x00B5, 0x00B6, 0x00B7,
    0x00F8, 0x00B9, 0x0157, 0x00BB, 0x00BC, 0x00BD, 0x00BE, 0x00E6, 0x0104, 0x012E, 0x0100, 0x0106, 0x00C4, 0x00C5,
    0x0118, 0x0112, 0x010C, 0x00C9, 0x0179, 0x0116, 0x0122, 0x0136, 0x012A, 0x013B, 0x0160, 0x0143, 0x0145, 0x00D3,
    0x014C, 0x00D5, 0x00D6, 0x00D7, 0x0172, 0x0141, 0x015A, 0x016A, 0x00DC, 0x017B, 0x017D, 0x00DF, 0x0105, 0x012F,
    0x0101, 0x0107, 0x00E4, 0x00E5, 0x0119, 0x0113, 0x010D, 0x00E9, 0x017A, 0x0117, 0x0123, 0x0137, 0x012B, 0x013C,
    0x0161, 0x0144, 0x0146, 0x00F3, 0x014D, 0x00F5, 0x00F6, 0x00F7, 0x0173, 0x0142, 0x015B, 0x016B, 0x00FC, 0x017C,
    0x017E, 0x02D9,
];
const CP1258: [u16; 128] = [
    0x20AC, 0xFFFD, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0xFFFD, 0x2039, 0x0152, 0xFFFD,
    0xFFFD, 0xFFFD, 0xFFFD, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0xFFFD, 0x203A,
    0x0153, 0xFFFD, 0xFFFD, 0x0178, 0x00A0, 0x00A1, 0x00A2, 0x00A3, 0x00A4, 0x00A5, 0x00A6, 0x00A7, 0x00A8, 0x00A9,
    0x00AA, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x00AF, 0x00B0, 0x00B1, 0x00B2, 0x00B3, 0x00B4, 0x00B5, 0x00B6, 0x00B7,
    0x00B8, 0x00B9, 0x00BA, 0x00BB, 0x00BC, 0x00BD, 0x00BE, 0x00BF, 0x00C0, 0x00C1, 0x00C2, 0x0102, 0x00C4, 0x00C5,
    0x00C6, 0x00C7, 0x00C8, 0x00C9, 0x00CA, 0x00CB, 0x0300, 0x00CD, 0x00CE, 0x00CF, 0x0110, 0x00D1, 0x0309, 0x00D3,
    0x00D4, 0x01A0, 0x00D6, 0x00D7, 0x00D8, 0x00D9, 0x00DA, 0x00DB, 0x00DC, 0x01AF, 0x0303, 0x00DF, 0x00E0, 0x00E1,
    0x00E2, 0x0103, 0x00E4, 0x00E5, 0x00E6, 0x00E7, 0x00E8, 0x00E9, 0x00EA, 0x00EB, 0x0301, 0x00ED, 0x00EE, 0x00EF,
    0x0111, 0x00F1, 0x0323, 0x00F3, 0x00F4, 0x01A1, 0x00F6, 0x00F7, 0x00F8, 0x00F9, 0x00FA, 0x00FB, 0x00FC, 0x01B0,
    0x20AB, 0x00FF,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_and_fallbacks() {
        assert_eq!(CodePage::new(1251).number(), 1251);
        assert_eq!(CodePage::new(936), CodePage::WESTERN);
        assert_eq!(CodePage::new(70_000), CodePage::WESTERN);
        let cyrillic = [0xD1, 0xEF, 0xE0, 0xF1, 0xF1, 0xEA, 0xE8, 0xE9];
        assert_eq!(CodePage::new(1251).decode(&cyrillic), "Спасский");
        assert_eq!(CodePage::WESTERN.decode(&[0xC5, b'n', b'g', b's', b't', b'r', 0xF6, b'm']), "Ångström");
        assert_eq!(CodePage::new(1251).char(0x88), '€');
        assert_eq!(CodePage::WESTERN.char(0x80), '€');
        assert_eq!(CodePage::WESTERN.char(0x81), '\u{81}');
        assert_eq!(CodePage::new(1250).char(0x8A), 'Š');
    }

    #[test]
    fn undefined_bytes_keep_their_value() {
        let undefined = [0x81, 0x8D, 0x8F, 0x90, 0x9D];
        assert_eq!(CodePage::WESTERN.decode(&undefined), "\u{81}\u{8d}\u{8f}\u{90}\u{9d}");
        assert_eq!(CodePage::new(1253).char(0xAA), '\u{aa}');
        assert_eq!(CodePage::new(1255).char(0xFF), '\u{ff}');
        // Above 0x9f, 1252 is Latin-1.
        assert!((0xA0..=0xFF).all(|b| CodePage::WESTERN.char(b) == char::from(b)));
    }

    /// Every byte of every page encodes back from the character it reads as,
    /// and the first character that no byte reads as is named.
    #[test]
    fn text_encodes_to_the_bytes_it_reads_from() {
        let all: Vec<u8> = (0..=0xff).collect();
        for number in 1250..=1258 {
            let page = CodePage::new(number);
            assert_eq!(page.encode(&page.decode(&all)).as_deref(), Ok(&all[..]), "{number}");
        }
        assert_eq!(CodePage::new(1251).encode("Спасский"), Ok(vec![0xD1, 0xEF, 0xE0, 0xF1, 0xF1, 0xEA, 0xE8, 0xE9]));
        assert_eq!(CodePage::WESTERN.encode("Ångström €"), Ok(b"\xc5ngstr\xf6m \x80".to_vec()));
        assert_eq!(CodePage::WESTERN.encode("Tal, Михаил ♔"), Err('М'));
        assert_eq!(CodePage::new(1251).encode("Таль ♔"), Err('♔'));
    }

    /// Words tell Cyrillic text from Western text whichever page reads them.
    #[test]
    fn words_tell_cyrillic_from_western_text() {
        let guess = cyrillic_or_western;
        let cyrillic = Some(CodePage::CYRILLIC);
        let western = Some(CodePage::WESTERN);
        assert_eq!(guess(b"\xcf\xe5\xf2\xf0\xee\xe2"), cyrillic, "Петров");
        assert_eq!(guess(b"\xe1\xb3\xeb\xb3"), cyrillic, "білі: і is a letter only Windows-1251 has");
        assert_eq!(guess(b"\xaf\xe6\xe0\xea"), cyrillic, "Їжак");
        // Squares with a file typed in Cyrillic.
        assert_eq!(guess(b", R\xe58 \xe8 \xf2.\xe4."), cyrillic, "Rе8 и т.д.");
        assert_eq!(guess(b"\xe8 N:\xf12"), cyrillic, "и N:с2");
        assert_eq!(guess(b"11. \xe55"), cyrillic, "е5");
        assert_eq!(guess(b"R\xe5x"), None, "no rank after the file");
        // A letter that ends a number.
        assert_eq!(guess(b"3.7\xe0"), cyrillic, "3.7а");
        assert_eq!(guess(b"9.4\xf1"), cyrillic, "9.4с");
        assert_eq!(guess(b"3 \xe0"), None, "a word of its own");
        // Western multiplication and division between numbers.
        assert_eq!(guess(b"2\xd72 = 4"), None, "2×2 = 4");
        assert_eq!(guess(b"10\xf75 = 2"), None, "10÷5 = 2");
        assert_eq!(guess(b"3\xe22"), None, "a letter that does not end the number: 3в2");
        // Western ordinals stay Western.
        assert_eq!(guess(b"1\xaa Divisi\xf3n"), None, "1ª División: ó looks like у");
        assert_eq!(guess(b"1\xaa Divisi\xf3n, M\xfcller"), western);
        assert_eq!(guess(b"2\xba Open"), None, "2º Open");
        // Cyrillic letters that look Latin among ASCII ones read alike: neither.
        assert_eq!(guess(b"\xf1orr."), None, "сorr.");
        assert_eq!(guess(b"(\xf1ontinuation 1)"), None);
        assert_eq!(guess(b"Mu\xf1oz"), None, "Muñoz too");
        assert_eq!(guess(b"Mu\xf1oz G\xf3mez Mart\xedn"), western, "beside other Western letters");
        // `иначе`: `ч` is `÷` in Windows-1252, and still a letter of the word.
        assert_eq!(guess(b"\xe8\xed\xe0\xf7\xe5"), cyrillic);
        // Кр with a Latin K, on its own.
        assert_eq!(guess(b"1.K\xf0g1"), cyrillic, "1.Kрg1");
        assert_eq!(guess(b"4...K\xf0:d4"), cyrillic, "4...Kр:d4");
        assert_eq!(guess(b"C\xeb\xee\xed"), cyrillic, "Cлон, with a Latin C");
        assert_eq!(guess(b"H\xfcbner, R"), western);
        assert_eq!(guess(b"Copyright 1994 K\xf6nemann"), western);
        assert_eq!(guess(b"Diese Partie ist ein Beispiel f\xfcr die Schw\xe4che"), western);
        assert_eq!(guess(b"K\xf6lner"), western, "Kölner: K before a high byte, but no move");
        // Short words that could be either show neither.
        assert_eq!(guess(b"S\xfc\xdf"), None, "Süß");
        assert_eq!(guess(b"P\xe4\xe4"), None, "Pää");
        assert_eq!(guess(b"Cet \xe9t\xe9"), None, "Cet été");
        assert_eq!(guess(b"Cet \xe9t\xe9 \xe0 Z\xfcrich"), western, "beside a word that shows Western");
        // Russian notation and `№`.
        assert_eq!(guess(b"\xd4c2"), cyrillic, "Фc2");
        assert_eq!(guess(b"\xca\xf0xg1"), cyrillic, "Крxg1");
        assert_eq!(guess(b"\xcb:f6"), cyrillic, "Л:f6");
        assert_eq!(guess(b"\xb943"), cyrillic, "№43");
        // The larger total decides: Позиция, then a German name.
        assert_eq!(guess(b"\xcf\xee\xe7\xe8\xf6\xe8\xff K\xf6nig"), cyrillic);
        // Nothing to go by: one-letter words, ASCII, symbols.
        assert_eq!(guess(b"\xe0"), None, "à or а");
        assert_eq!(guess(b"\xe9 forte [Kasparov]"), None);
        assert_eq!(guess(b"Kasparov"), None);
        assert_eq!(guess(b"\x96 \xab\xbb"), None);
        assert_eq!(guess(b""), None);
        // Evidence adds up: `и т.д.` shows nothing alone, and Петров beside it does.
        let mut e = Evidence::of(b"\xe8 \xf2.\xe4.");
        assert_eq!(e.page(), None);
        e.add(Evidence::of(b"\xcf\xe5\xf2\xf0\xee\xe2"));
        assert_eq!(e.page(), cyrillic);
        // Six Western letters against Петров's six: neither shows more.
        e.add(Evidence::of(b"Sch\xf6n M\xfcller K\xf6nig H\xfcbner M\xe4rz T\xe4ter"));
        assert_eq!(e.page(), None);
        e.add(Evidence::of(b"Br\xfccke"));
        assert_eq!(e.page(), western);
    }

    /// UTF-8 first; Windows-1251 only for text whose words show Cyrillic;
    /// Windows-1252 for anything else, as before.
    #[test]
    fn utf8_or_legacy_reads_russian_and_leaves_the_rest() {
        assert_eq!(utf8_or_legacy("Петров".as_bytes()), "Петров");
        assert_eq!(utf8_or_legacy(b"\xcf\xe5\xf2\xf0\xee\xe2"), "Петров");
        assert_eq!(utf8_or_legacy(b"M\xfcller"), "Müller");
        assert_eq!(utf8_or_legacy(b"\xe0"), "à", "nothing to go by: Western, as before");
        assert_eq!(utf8_or_legacy(b"Visit\xe9 \xa4d7"), "Visité ¤d7", "no signs");
        assert_eq!(utf8_or_legacy(b"\x81"), "\u{81}");
    }

    #[test]
    fn utf8_wins_when_valid() {
        assert_eq!(CodePage::new(1251).utf8_or("Спасский".as_bytes()), "Спасский");
        assert_eq!(CodePage::new(1251).utf8_or(&[0xD2, 0xE0, 0xEB, 0xFC]), "Таль");
    }
}
