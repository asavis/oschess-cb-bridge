//! The full form's comment commands (asavis/oschess-cb-bridge#42): a text's
//! language, `[%cb…]` for every type the reading form leaves out, and the
//! undecoded rest of a record. `docs/api.md` lists the vocabulary.

use chesscore::{Board, Move, Piece, Square};

use crate::movetable::Sq;
use crate::v2::{self, Annotation, Arrow, Quotation, language};
use crate::view::PositionOrder;

/// The `[%lang]` code of a ChessBase language number: ISO 639-1 where there is
/// one, `any` for a text meant for every language, `cb-<nation>` for a classic
/// text's nation that names no language ChessBase writes, and `cb-l<number>`
/// for another 2CBH number.
pub fn language_code(l: u16) -> String {
    let iso = match l {
        language::ENGLISH => "en",
        language::GERMAN => "de",
        language::FRENCH => "fr",
        language::SPANISH => "es",
        language::ITALIAN => "it",
        language::DUTCH => "nl",
        language::PORTUGUESE => "pt",
        language::POLISH => "pl",
        language::GREEK => "el",
        language::ANY => "any",
        n if n >= 0x100 => return format!("cb-{}", n - 0x100),
        n => return format!("cb-l{n}"),
    };
    iso.to_string()
}

/// `[%cb<name> key=value;…]`, values percent-encoded, empty ones left out.
fn command(name: &str, fields: &[(&str, String)]) -> String {
    let body: Vec<String> =
        fields.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| format!("{k}={}", percent(v))).collect();
    format!("[%cb{name} {}]", body.join(";"))
}

/// Everything outside `A-Z a-z 0-9 - . _ ~` as `%XX` of its UTF-8 bytes, so
/// that `;`, `=`, `]`, `}`, `%`, spaces and line breaks never appear raw.
pub(super) fn percent(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Base64 with the URL alphabet and no padding.
pub(super) fn base64url(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, &b)| acc | u32::from(b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// The command for an annotation the reading form leaves out, in the
/// numbering of `order`'s format: its decoded fields where they are known,
/// and always its data, so that nothing is lost.
pub(super) fn for_other(code: u16, data: &[u8], order: PositionOrder) -> String {
    let raw = base64url(data);
    let classic = order == PositionOrder::Stored;
    let quote = match code {
        0x13 if classic => Quotation::parse_classic(data),
        0x13 => Quotation::parse_2cbh(data),
        _ => None,
    };
    if let Some(q) = quote {
        return quotation(&q, raw);
    }
    if let Some(m) = medal(code, data, order) {
        return m;
    }
    if classic {
        return command("raw", &[("type", format!("{code:02x}")), ("data", raw)]);
    }
    let byte = |i: usize| data.get(i).copied().unwrap_or_default();
    let int = |i: usize| data.get(i..i + 4).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    // An `int` length at `i` and that many bytes: the text and where the
    // next field starts.
    let text = |i: usize| -> (String, usize) {
        let n = data.get(i..i + 4).map_or(0, |b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]).max(0) as usize);
        let t = data.get(i + 4..i + 4 + n).map(crate::v2::annotations_text).unwrap_or_default();
        (t, i + 4 + n)
    };
    match (code, data.len()) {
        (0x18, 1) => {
            let phase = match byte(0) {
                1 => "opening",
                2 => "middlegame",
                3 => "endgame",
                _ => "",
            };
            command("critical", &[("phase", phase.into()), ("value", byte(0).to_string()), ("data", raw)])
        }
        (0x14, 1) => command("pawns", &[("value", byte(0).to_string()), ("data", raw)]),
        (0x15, _) => command("path", &[("data", raw)]),
        (0x23, 4) => command("colour", &[("data", raw)]),
        (0x1c, _) => {
            // 01, then the URL and the caption, each an `int` length and bytes.
            let (url, next) = text(1);
            let (caption, _) = text(next);
            command("link", &[("url", url), ("caption", caption), ("data", raw)])
        }
        (0x20, _) => {
            // 01 00, a short (likely a language), an `int` length and the text.
            let lang = u16::from_le_bytes([byte(2), byte(3)]);
            command("video", &[("language", lang.to_string()), ("caption", text(4).0), ("data", raw)])
        }
        (0x09, _) => {
            // The variant in the header's third byte, then the time allowed
            // and the points.
            command(
                "training",
                &[
                    ("variant", byte(2).to_string()),
                    ("seconds", int(6).to_string()),
                    ("points", u16::from_le_bytes([byte(10), byte(11)]).to_string()),
                    ("data", raw),
                ],
            )
        }
        _ => command("raw", &[("type", format!("{code:02x}")), ("data", raw)]),
    }
}

fn quotation(q: &Quotation, raw: String) -> String {
    let name = |p: &crate::v2::QuotedPlayer| {
        let (last, first) = (p.last.trim(), p.first.trim());
        if first.is_empty() { last.to_string() } else { format!("{last}, {first}") }
    };
    let elo = |e: u16| if e > 0 { e.to_string() } else { String::new() };
    let result = match q.result {
        0 => "0-1",
        1 => "1/2-1/2",
        2 => "1-0",
        _ => "*",
    };
    command(
        "quote",
        &[
            ("result", result.into()),
            ("white", name(&q.white)),
            ("whiteElo", elo(q.white.elo)),
            ("black", name(&q.black)),
            ("blackElo", elo(q.black.elo)),
            ("event", q.event.trim().to_string()),
            ("site", q.site.trim().to_string()),
            ("date", q.date.pgn()),
            ("round", if q.round > 0 { q.round.to_string() } else { String::new() }),
            ("subround", if q.subround != 0 { q.subround.to_string() } else { String::new() }),
            ("eco", q.eco.pgn().unwrap_or_default()),
            ("moves", moves(q).unwrap_or_default()),
            ("data", raw),
        ],
    )
}

/// A 2CBH quotation's moves as SAN movetext from the standard position, when
/// every move replays: the origin square in the low six bits of its byte,
/// numbered file by file, with bit 6 set on a promotion; the destination in
/// the low six bits of its byte, and the promoted piece in its top two (queen,
/// knight, bishop, rook).
fn moves(q: &Quotation) -> Option<String> {
    if q.moves.is_empty() {
        return None;
    }
    let square = |v: u8| Square::new((v & 63) / 8, v & 7);
    let mut board = Board::startpos();
    let mut out = String::new();
    for (k, &[from, to]) in q.moves.iter().enumerate() {
        let promotion =
            (from & 0x40 != 0).then(|| [Piece::Queen, Piece::Knight, Piece::Bishop, Piece::Rook][(to >> 6) as usize]);
        let (from, to) = (square(from), square(to));
        let mv = board.legal_moves().into_iter().find(|m| {
            m.from == from && m.promotion == promotion && (m.to == to || castle_to(&board, *m) == Some(to))
        })?;
        if k % 2 == 0 {
            if k > 0 {
                out.push(' ');
            }
            out.push_str(&format!("{}. ", k / 2 + 1));
        } else {
            out.push(' ');
        }
        out.push_str(&super::san(&board, mv));
        board.play_unchecked(mv);
    }
    Some(out)
}

/// The king's destination of a castling move, which chesscore writes as the
/// king taking its own rook.
fn castle_to(board: &Board, mv: Move) -> Option<Square> {
    let (piece, colour) = board.piece_at(mv.from)?;
    if piece != Piece::King || board.colors(colour) & mv.to.bit() == 0 {
        return None;
    }
    Some(Square::new(if mv.to.file() > mv.from.file() { 6 } else { 2 }, mv.from.rank()))
}

/// The reading form's text of a game quotation, as ChessBase's own PGN writes
/// it, or `None` for another annotation or a quotation not understood.
pub(super) fn quotation_text(a: &Annotation, order: PositionOrder) -> Option<String> {
    let Annotation::Other { code: 0x13, data } = a else { return None };
    let q = match order {
        PositionOrder::Stored => Quotation::parse_classic(data),
        PositionOrder::Pgn => Quotation::parse_2cbh(data),
    }?;
    Some(q.chessbase_text())
}

/// Medals (type `22`) as ChessBase's own PGN writes them, `[%mdl <bits>]`:
/// the `int` of medal bits, little-endian in 2CBH and big-endian in the
/// classic format. Both forms write it; it holds the whole annotation.
pub(super) fn medal(code: u16, data: &[u8], order: PositionOrder) -> Option<String> {
    let b: [u8; 4] = data.try_into().ok().filter(|_| code == 0x22)?;
    let bits = match order {
        PositionOrder::Pgn => u32::from_le_bytes(b),
        PositionOrder::Stored => u32::from_be_bytes(b),
    };
    Some(format!("[%mdl {bits}]"))
}

/// `[%cbtext …]`: a text's original value, which the visible `[%lang]`
/// comment cannot hold as it is: a text that cleaning for PGN changes, an
/// empty one, or one meant to precede its move. It follows the text's visible
/// comment, or stands in its place with `alone=1` when cleaning leaves nothing
/// to show. `value` is always written, empty or not.
pub(super) fn text(language: u16, before: bool, alone: bool, value: &str) -> String {
    let before = if before { "before=1;" } else { "" };
    let alone = if alone { "alone=1;" } else { "" };
    format!("[%cbtext lang={};{before}{alone}value={}]", percent(&language_code(language)), percent(value))
}

/// Whether a text needs [`text`] beside its visible comment: a reader could
/// not tell its placement, its value, or its text from a command.
pub(super) fn text_needs_original(before: bool, value: &str, cleaned: &str) -> bool {
    before || value.is_empty() || value != cleaned || value.contains("[%")
}

/// `[%cbsymbols]`, `[%cbsquares]` and `[%cbarrows]`: the annotations the NAGs
/// and `[%csl]`/`[%cal]` show, with every slot and every colour. `data` is the
/// three NAG slots (move, position, prefix), or the (colour, square) pairs and
/// (colour, from, to) triples, squares numbered from 1 file by file, as both
/// formats store them.
pub(super) fn graphic(a: &Annotation) -> Option<String> {
    let cb = |sq: Sq| (sq % 8) * 8 + sq / 8 + 1;
    let (name, data): (&str, Vec<u8>) = match a {
        Annotation::Symbols { on_move, on_position, prefix } => ("symbols", vec![*on_move, *on_position, *prefix]),
        Annotation::Squares(v) => {
            ("squares", v.iter().flat_map(|&v2::Square { colour, square }| [colour, cb(square)]).collect())
        }
        Annotation::Arrows(v) => {
            ("arrows", v.iter().flat_map(|&Arrow { colour, from, to }| [colour, cb(from), cb(to)]).collect())
        }
        _ => return None,
    };
    Some(format!("[%cb{name} data={}]", base64url(&data)))
}

/// `[%cbrest]`: the bytes of a record after a type of unknown layout.
pub(super) fn rest(type_code: u16, data: &[u8]) -> String {
    command("rest", &[("type", format!("{type_code:02x}")), ("data", base64url(data))])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_percent_encoded() {
        assert_eq!(percent("a-b.c_d~e"), "a-b.c_d~e");
        assert_eq!(percent("x;y=z]}%\n é"), "x%3By%3Dz%5D%7D%25%0A%20%C3%A9");
        assert_eq!(percent("Київ"), "%D0%9A%D0%B8%D1%97%D0%B2");
    }

    #[test]
    fn base64url_has_no_padding() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn language_codes() {
        assert_eq!(language_code(language::ENGLISH), "en");
        assert_eq!(language_code(language::ANY), "any");
        assert_eq!(language_code(0x100 + 145), "cb-145");
        assert_eq!(language_code(9), "cb-l9");
    }
}
