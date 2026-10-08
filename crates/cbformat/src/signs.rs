//! ChessBase's signs in text (#309): the Private Use Area code points recent
//! ChessBase writes for the chess signs of its fonts, which no other program
//! draws, read as the Unicode signs they stand for (`docs/format-notes.md`,
//! "ChessBase's signs"). A name is read so as it is read from the file
//! ([`decode`]), so that it is searched for as it shows; a comment keeps the
//! record's text and is read so where PGN shows it (`crate::pgn`), whose full
//! form keeps the original.
//!
//! No public table names them. Each was read from the Mega's comments: the
//! words beside it (a piece's before a square, a flank's before `attack`),
//! the evaluation it follows (`⩲/<U+E00A>` as ChessBase's own older text
//! writes `⩲/±`), and the bytes of the Windows-1252 text it replaced where
//! ChessBase converted such text (`n°5` became `n<U+E000>5`). A code point
//! the Mega gives no reading for is left as it stands.

use std::borrow::Cow;

/// A 2CBH string: UTF-8, or a single-byte page where it is not
/// ([`crate::codepage::utf8_or_legacy`]), with ChessBase's signs read
/// ([`read`]).
pub(crate) fn decode(bytes: &[u8]) -> String {
    let text = crate::codepage::utf8_or_legacy(bytes);
    match read(&text) {
        Cow::Borrowed(_) => text,
        Cow::Owned(read) => read,
    }
}

/// `text` with ChessBase's signs read: each Private Use Area code point this
/// module knows becomes its Unicode sign ([`sign`]), or the letter it was
/// where ChessBase took a Cyrillic letter for a sign ([`letter`]); a mark of
/// a course's layout becomes the bracket it stands for ([`markup`]).
pub(crate) fn read(text: &str) -> Cow<'_, str> {
    if !text.chars().any(|c| is_private(c) || c == '×') {
        return Cow::Borrowed(text);
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some((s, n)) = markup(&chars[i..]) {
            out.push_str(s);
            i += n;
            continue;
        }
        let c = chars[i];
        let at = |j: Option<usize>| j.and_then(|j| chars.get(j)).copied();
        match (letter(c, at(i.checked_sub(1)), at(Some(i + 1)), at(Some(i + 2))), sign(c)) {
            (Some(l), _) => out.push(l),
            (None, Some(s)) => out.push_str(s),
            (None, None) => out.push(c),
        }
        i += 1;
    }
    Cow::Owned(out)
}

fn is_private(c: char) -> bool {
    ('\u{e000}'..='\u{f8ff}').contains(&c)
}

/// The Unicode sign for ChessBase's code point `c`, as the Informator
/// symbols map to Unicode (L2/17-033R2, encoded in Unicode 11): the empty
/// string for the diagram mark, which only asks for a diagram to be printed.
fn sign(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{e000}' => "⯹", // with compensation for the material
        '\u{e001}' => "⮺", // pair of bishops
        '\u{e002}' => "⮻", // bishops of opposite colour
        '\u{e004}' | '\u{e02e}' => "⩱",
        '\u{e005}' => "", // diagram
        '\u{e007}' | '\u{e02f}' => "⩲",
        '\u{e008}' => "⯽", // passed pawn
        '\u{e009}' => "∞",
        '\u{e00a}' => "±",
        '\u{e00c}' => "⌓", // better is
        '\u{e00d}' => "△", // with the idea
        '\u{e00e}' => "⟪", // queenside
        '\u{e00f}' => "⟫", // kingside
        '\u{e010}' => "×", // weak point
        '\u{e012}' => "↑", // initiative
        '\u{e013}' => "→", // attack
        '\u{e017}' => "⇆", // counterplay
        '\u{e018}' => "⇔", // file
        '\u{e019}' => "⇗", // diagonal
        '\u{e01a}' => "∓",
        '\u{e01d}' => "⨀", // zugzwang
        '\u{e01e}' => "⊞", // centre
        '\u{e01f}' => "⊥", // endgame
        '\u{e021}' => "□", // only move
        '\u{e023}' => "○", // space
        '\u{e024}' => "♔",
        '\u{e025}' => "♕",
        '\u{e026}' => "♖",
        '\u{e027}' => "♗",
        '\u{e028}' => "♘",
        '\u{e029}' => "♙",
        _ => return None,
    })
}

/// The Cyrillic letter that ChessBase turned into the sign `c`, between
/// `prev` and `next` (and `after`, the character after `next`). Converting
/// text typed on a Windows whose page is 1251, ChessBase took the bytes of
/// some letters for the signs its fonts draw there: і and І (0xb3 and 0xb2,
/// ⩱ and ⩲), ю (0xfe, passed pawn), ч (0xf7, unclear) and Ч (0xd7, which it
/// kept as `×`). Such a sign touching a Cyrillic letter is that letter
/// (`Кл<U+E008>ев` is `Клюев`); `×` only where it starts a word of Cyrillic
/// letters (`×ернов`), since Russian notation writes it for a capture
/// (`Л×c3`, `Кр×е5`).
fn letter(c: char, prev: Option<char>, next: Option<char>, after: Option<char>) -> Option<char> {
    let cyrillic = |c: Option<char>| c.is_some_and(|c| matches!(c, '\u{400}'..='\u{4ff}') && c.is_alphabetic());
    let lower = |c: Option<char>| cyrillic(c) && c.is_some_and(char::is_lowercase);
    let touches = cyrillic(prev) || cyrillic(next);
    match c {
        '\u{e004}' | '\u{e02e}' if touches => Some('і'),
        '\u{e007}' | '\u{e02f}' if touches => Some('І'),
        '\u{e008}' if touches => Some('ю'),
        '\u{e009}' if touches => Some('ч'),
        '×' if !prev.is_some_and(char::is_alphanumeric) && lower(next) && lower(after) => Some('Ч'),
        _ => None,
    }
}

/// A mark of a course's layout at the start of `chars`, with how many
/// characters it takes: a word between runs of U+E02D, which a course's
/// export writes for its brackets, positions and links
/// (`<U+E02D><U+E02D>StartBracket<U+E02D><U+E02D>`). A bracket's mark is the
/// bracket, a position's sets its FEN apart, and a link's leaves the address
/// alone.
fn markup(chars: &[char]) -> Option<(&'static str, usize)> {
    const MARK: char = '\u{e02d}';
    let run = |from: usize| chars[from..].iter().take_while(|&&c| c == MARK).count();
    let open = run(0);
    if open == 0 {
        return None;
    }
    let word: String = chars[open..].iter().take(16).take_while(|c| c.is_ascii_alphabetic()).collect();
    let close = run(open + word.len());
    if close == 0 {
        return None;
    }
    let s = match word.to_ascii_lowercase().as_str() {
        "startbracket" => "(",
        "endbracket" => ")",
        "startsquare" => "[",
        "endsquare" => "]",
        "startfen" => "[FEN ",
        "endfen" => "]",
        "linkstart" | "linkend" => "",
        _ => return None,
    };
    Some((s, open + word.len() + close))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(s: &str) -> String {
        super::read(s).into_owned()
    }

    #[test]
    fn pieces_and_signs_read_as_unicode() {
        // Made-up comments.
        assert_eq!(read("the \u{e028}d4 is strong, the \u{e024}g8 is safe"), "the ♘d4 is strong, the ♔g8 is safe");
        assert_eq!(read("\u{e025}\u{e026}\u{e027}\u{e029}"), "♕♖♗♙");
        assert_eq!(read("/\u{e00a}"), "/±");
        assert_eq!(read("\u{e00d}\u{e028}g3\u{e012}"), "△♘g3↑");
        assert_eq!(read("/\u{e02f}, /\u{e02e}, /\u{e017}, /\u{e01a}"), "/⩲, /⩱, /⇆, /∓");
        assert_eq!(read("a \u{e00e} majority, a \u{e00f} attack"), "a ⟪ majority, a ⟫ attack");
        assert_eq!(read("an endgame with \u{e001}, \u{e002}"), "an endgame with ⮺, ⮻");
        assert_eq!(read("n\u{e000}5"), "n⯹5", "the ° ChessBase took for compensation stays a sign");
        assert_eq!(read("\u{e005} A mistake."), " A mistake.", "the diagram mark goes");
        assert_eq!(read("Club \u{e024} cup"), "Club ♔ cup");
    }

    #[test]
    fn unknown_code_points_stay() {
        assert_eq!(read("\u{e020}!?"), "\u{e020}!?");
        assert_eq!(read("~= \u{e02d}"), "~= \u{e02d}");
        assert_eq!(read("\u{f000}"), "\u{f000}");
    }

    #[test]
    fn signs_inside_cyrillic_words_are_the_letters_they_were() {
        // Made-up names.
        assert_eq!(read("Кл\u{e008}ева, Ольга"), "Клюева, Ольга");
        assert_eq!(read("Сав\u{e009}ук"), "Савчук");
        assert_eq!(read("Кор\u{e004}нь, \u{e007}ван"), "Корінь, Іван");
        assert_eq!(read("Б\u{e02e}лоус"), "Білоус");
        assert_eq!(read("\u{e02f}ра"), "Іра");
        assert_eq!(read("×ернова, Ольга"), "Чернова, Ольга");
        assert_eq!(read("с\u{e008}да"), "сюда");
        // Signs beside Cyrillic words, not in them.
        assert_eq!(read("проходная \u{e008}"), "проходная ⯽");
        assert_eq!(read("белые лучше /\u{e02f}"), "белые лучше /⩲");
        // × as a capture or a weak point stays.
        assert_eq!(read("Л×c3, Кр×е5, ×d5, 2×2"), "Л×c3, Кр×е5, ×d5, 2×2");
    }

    #[test]
    fn a_course_layout_reads_as_brackets() {
        let m = "\u{e02d}\u{e02d}";
        assert_eq!(read(&format!("both flanks {m}StartBracket{m}...a6{m}EndBracket{m}.")), "both flanks (...a6).");
        assert_eq!(
            read(&format!("it \u{e02d}{m}StartBracket{m}x{m}EndBRacket{m}")),
            "it (x)",
            "a third mark and a typo"
        );
        assert_eq!(read(&format!("{m}StartSquare{m}Smith,J{m}EndSquare{m}")), "[Smith,J]");
        assert_eq!(
            read(&format!("If {m}StartFEN{m}8/8/8/8/8/8/8/K1k5 w - - 0 1{m}EndFEN{m} 1.Kb1")),
            "If [FEN 8/8/8/8/8/8/8/K1k5 w - - 0 1] 1.Kb1"
        );
        assert_eq!(read(&format!("({m}LinkStart{m}https://example.org/{m}LinkEnd{m})")), "(https://example.org/)");
        assert_eq!(read(&format!("{m}Unknown{m}")), format!("{m}Unknown{m}"), "another word stays");
    }

    /// ChessBase's sign for ч in UTF-8 inside Windows-1251 text (#311) is
    /// the letter again.
    #[test]
    fn a_sign_in_utf8_inside_windows_1251_is_its_letter() {
        assert_eq!(decode(b"\xed\xe5\xf0\xe0\xe7\xe1\xee\xf0\xee\x80\x89\xe8\xe2\xee"), "неразборчиво");
    }

    #[test]
    fn text_without_signs_is_unchanged() {
        assert_eq!(read("Петров ±"), "Петров ±");
        assert_eq!(decode("Петров".as_bytes()), "Петров");
        assert_eq!(decode(b"\xcf\xe5\xf2\xf0\xee\xe2"), "Петров");
    }
}
