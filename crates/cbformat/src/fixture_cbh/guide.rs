//! Guiding text records of the classic format (asavis/oschess-cb-bridge#324),
//! laid out by hand as `docs/format-notes.md` ("Guiding texts") describes.

/// `n` as a little-endian `u16`.
pub fn u16s(n: usize) -> [u8; 2] {
    (n as u16).to_le_bytes()
}

/// `n` as a little-endian `u32`.
pub fn u32s(n: usize) -> [u8; 4] {
    (n as u32).to_le_bytes()
}

/// The body of a classic text record after its flags and size: `version`, one
/// English `title`, the unknown byte, then `count` contents, already laid out.
pub fn body(version: u16, title: &[u8], count: usize, contents: &[u8]) -> Vec<u8> {
    let mut b = version.to_le_bytes().to_vec();
    b.extend(u16s(1));
    b.extend(u16s(0));
    b.extend(u16s(title.len()));
    b.extend(title);
    b.push(1);
    b.extend(u16s(count));
    b.extend(contents);
    b
}

/// A version 1 content: language, text and formatting data.
pub fn content_v1(language: u16, text: &[u8], formatting: &[u8]) -> Vec<u8> {
    let mut c = language.to_le_bytes().to_vec();
    c.extend(u16s(text.len()));
    c.extend(text);
    c.extend(u16s(formatting.len()));
    c.extend(formatting);
    c
}

/// Formatting data: header, objects as `(type, position, data)`, styles as
/// `(id, properties)`, runs as `(length, style)`, and three empty property
/// lists. Positions are `u32` where `wide`.
pub fn formatting(
    objects: &[(u16, usize, Vec<u8>)],
    styles: &[(u16, Vec<u8>)],
    runs: &[(usize, u16)],
    wide: bool,
) -> Vec<u8> {
    let mut f = 300u16.to_le_bytes().to_vec();
    f.extend(u16s(objects.len()));
    f.extend(u16s(0));
    for (kind, position, data) in objects {
        f.extend(kind.to_le_bytes());
        if wide {
            f.extend(u32s(*position));
        } else {
            f.extend(u16s(*position));
        }
        f.extend(u16s(data.len()));
        f.extend(data);
    }
    f.extend(u16s(styles.len()));
    for (id, properties) in styles {
        f.extend(id.to_le_bytes());
        f.extend(properties);
    }
    for (len, style) in runs {
        f.extend(u16s(*len));
        f.extend(style.to_le_bytes());
        f.extend(u16s(0));
    }
    f.extend([0xff, 0xff]);
    f.extend([0, 0, 0, 0, 0, 0]);
    f
}

/// A style's properties: font, bold, italic, underline and size, and a key
/// the reader leaves alone.
pub fn style(font: &str, size: u32, bold: bool, italic: bool) -> Vec<u8> {
    let mut p = u16s(6).to_vec();
    let mut property = |key: u16, value: &[u8]| {
        p.extend(key.to_le_bytes());
        p.extend(u16s(value.len()));
        p.extend(value);
    };
    property(0, &[&u16s(font.len())[..], font.as_bytes()].concat());
    property(1, &[u8::from(bold)]);
    property(2, &[u8::from(italic)]);
    property(3, &[0]);
    property(4, &size.to_le_bytes());
    property(9, &[0; 6]);
    p
}

/// `text` after its `u16` length.
pub fn counted(text: &[u8]) -> Vec<u8> {
    [&u16s(text.len())[..], text].concat()
}

/// A diagram's data, as objects 0x09 and 0x11 hold it: a word, the squares
/// as 4-bit codes, then data the reader leaves alone.
pub fn diagram(pieces: &[(&str, u8)], extra: usize) -> Vec<u8> {
    let mut squares = [0u8; 32];
    for (square, code) in pieces {
        let s = square.as_bytes();
        let i = usize::from(s[0] - b'a') * 8 + usize::from(s[1] - b'1');
        squares[i / 2] |= if i.is_multiple_of(2) { code << 4 } else { *code };
    }
    [&[0x20, 0][..], &squares, &vec![0; 70 + extra]].concat()
}

/// A game link's search, and where `label` its label after it.
pub fn game_link(white: &str, black: &str, event: &str, label: Option<&str>) -> Vec<u8> {
    let mut search = vec![0, 0, 0, 1, 0, 0];
    for (i, name) in [white, black].iter().enumerate() {
        search.push(name.len() as u8);
        search.extend(name.as_bytes());
        if i == 0 {
            search.push(0);
        }
    }
    search.extend([0; 7]);
    search.push(event.len() as u8);
    search.extend(event.as_bytes());
    search.extend([0; 10]);
    search.extend([3, 0, 0, 0xb5, 0xaf]);
    let size = search.len();
    search[1..3].copy_from_slice(&u16s(size));
    let mut d = counted(&search);
    if let Some(label) = label {
        d.extend(counted(label.as_bytes()));
    }
    d
}
