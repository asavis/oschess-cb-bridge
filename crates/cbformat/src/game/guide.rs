//! Guiding texts (asavis/oschess-cb-bridge#324): the body of a piece of
//! writing filed among the games, in every language stored.
//!
//! The classic format's text versions 1 and 2 hold single-byte text and
//! formatting data, which [`crate::cbh`] reads into paragraphs of styled text,
//! diagrams and links ([`Body::Paragraphs`]). Its version 3 and 2CBH hold one
//! HTML document per language, which is kept as stored ([`Body::Html`]).
//! `docs/format-notes.md` describes both.

/// The body of a guiding text: every language stored, in stored order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GuidingText {
    pub contents: Vec<Content>,
}

/// The text in one language.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Content {
    /// A language number of [`super::language`]; a 2CBH nation is mapped as
    /// a text annotation's is ([`crate::cbh::annotations::language_of`]).
    pub language: u16,
    pub body: Body,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// A complete HTML document, as stored.
    Html(String),
    /// Paragraphs of a classic text of version 1 or 2, in order.
    Paragraphs(Vec<Paragraph>),
}

/// One paragraph: the text up to a line break, with the objects placed in it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Paragraph {
    pub spans: Vec<Span>,
}

/// A piece of a paragraph, in text order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Span {
    /// Text in one style. A list label (`1.`) is text too.
    Text { text: String, style: Style },
    /// A diagram: its position as the piece placement field of a FEN. The
    /// side to move is not known.
    Diagram { board: String },
    /// A link to a game of the same database, which names it by a search.
    Game(GameLink),
    /// A link to another guiding text of the same database, by its title.
    TextLink { title: String },
}

/// The style of a run of text, as the formatting data defines it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Style {
    /// The font's name; empty when the style names none.
    pub font: String,
    /// The size as stored, 0 when the style holds none. Its unit is not known;
    /// a reader compares the sizes of one text.
    pub size: u32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// A game link. It does not name a record: it holds the search that finds the
/// game, of which these are the fields known, as stored (white and black as
/// `Last,First`), and the label the text shows for it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GameLink {
    /// What the text shows, such as `1.5`; empty when the link has none.
    pub label: String,
    pub white: String,
    pub black: String,
    pub event: String,
}

/// The piece placement of a diagram's 64 squares, stored as 4-bit codes in
/// the order a1, a2 … a8, b1 … h8, the high nibble of each byte first: 0 for
/// an empty square, 1-6 for a white king, queen, knight, bishop, rook and
/// pawn, and 9-14 for the black ones. `None` when a square holds another
/// code.
pub(crate) fn diagram_board(squares: &[u8; 32]) -> Option<String> {
    let code = |file: usize, rank: usize| {
        let i = file * 8 + rank;
        let b = squares[i / 2];
        if i.is_multiple_of(2) { b >> 4 } else { b & 15 }
    };
    let mut fen = String::with_capacity(72);
    for rank in (0..8).rev() {
        let mut empty = 0u8;
        for file in 0..8 {
            let c = code(file, rank);
            if c == 0 {
                empty += 1;
                continue;
            }
            let piece = match c & 7 {
                1 => 'K',
                2 => 'Q',
                3 => 'N',
                4 => 'B',
                5 => 'R',
                6 => 'P',
                _ => return None,
            };
            if empty > 0 {
                fen.push(char::from(b'0' + empty));
                empty = 0;
            }
            fen.push(if c & 8 == 0 { piece } else { piece.to_ascii_lowercase() });
        }
        if empty > 0 {
            fen.push(char::from(b'0' + empty));
        }
        if rank > 0 {
            fen.push('/');
        }
    }
    Some(fen)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Squares as `(square, code)`, the rest empty.
    fn board(pieces: &[(&str, u8)]) -> [u8; 32] {
        let mut b = [0u8; 32];
        for (square, code) in pieces {
            let s = square.as_bytes();
            let i = usize::from(s[0] - b'a') * 8 + usize::from(s[1] - b'1');
            b[i / 2] |= if i.is_multiple_of(2) { code << 4 } else { *code };
        }
        b
    }

    #[test]
    fn a_diagram_reads_square_by_square() {
        // White king d5 and pawn d4, black king d7 and pawn h7.
        let b = board(&[("d5", 1), ("d4", 6), ("d7", 9), ("h7", 14)]);
        assert_eq!(diagram_board(&b).as_deref(), Some("8/3k3p/8/3K4/3P4/8/8/8"));
        assert_eq!(diagram_board(&[0; 32]).as_deref(), Some("8/8/8/8/8/8/8/8"));
        let all = board(&[("a1", 5), ("b1", 3), ("c1", 4), ("d1", 2), ("h8", 13), ("g8", 11), ("f8", 12), ("e8", 10)]);
        assert_eq!(diagram_board(&all).as_deref(), Some("4qbnr/8/8/8/8/8/8/RNBQ4"));
    }

    #[test]
    fn a_code_no_piece_has_is_no_diagram() {
        for code in [7, 8, 15] {
            assert_eq!(diagram_board(&board(&[("e4", code)])), None, "{code}");
        }
    }
}
