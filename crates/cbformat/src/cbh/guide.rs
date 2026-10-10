//! The body of a classic guiding text (asavis/oschess-cb-bridge#324), read
//! from its `.cbg` record as `docs/format-notes.md` ("Guiding texts")
//! describes: after the titles, each language's text, which versions 1 and 2
//! follow with formatting data and version 3 holds as HTML.

use std::collections::HashMap;

use super::text::{page_of, single_byte_pieces, text};
use super::{Database, Record};
use crate::bytes::Cursor;
use crate::codepage::{CodePage, utf8_or_legacy};
use crate::game::RecordKind;
use crate::game::guide::{Body, Content, GameLink, GuidingText, Paragraph, Span, Style, diagram_board};
use crate::{Error, Result};

/// The byte of the text where an object of the formatting data stands.
const MARKER: u8 = 0x04;
/// The end of a paragraph.
const LINE: u8 = b'\r';
/// The end of the runs.
const END_OF_RUNS: u16 = 0xffff;
/// The longest font name kept, in characters. Real ones are short (`FigurineCB
/// AriesSP`); every span of a style carries its font, so a longer stored name
/// is cut rather than copied into each of them.
pub const MAX_FONT_CHARS: usize = 64;

/// The objects read; each comes as a pair at one place, and either of a pair
/// gives what is read of it. The other types are left out.
const DIAGRAMS: [u16; 2] = [0x09, 0x11];
/// A game link; [`LABELLED_GAME`] is the same with its label after it.
const GAME: u16 = 0x02;
const LABELLED_GAME: u16 = 0x1a;
const TEXT_LINKS: [u16; 2] = [0x05, 0x19];
const LIST_LABEL: u16 = 0x0c;

impl Database {
    /// The body of guiding text `record`, its `.cbg` record read within
    /// `limit` bytes. An error for a record that is not a guiding text, and
    /// for one whose titles and contents do not fill it as the format says.
    /// Formatting data that does not read leaves its text unstyled, with no
    /// objects.
    pub fn guiding_text(&self, record: &Record, limit: usize) -> Result<GuidingText> {
        if record.kind() != RecordKind::Text {
            return Err(Error::Format(format!("record {} is not a guiding text", record.id())));
        }
        let data = self.moves_of_within(record, limit)?;
        read(data.bytes(), self.page, self.entities.fallback())
            .map_err(|what| Error::Format(format!("guiding text {}: {what}", record.id())))
    }
}

/// A text record of `.cbg`, its single-byte text read on a computer whose
/// code page is `page`, in `fallback` where its words show none.
pub(super) fn read(b: &[u8], page: CodePage, fallback: CodePage) -> std::result::Result<GuidingText, &'static str> {
    let mut c = Cursor::new(b);
    c.take(4).ok_or("no head")?;
    let version = c.le_u16().ok_or("no version")?;
    if !(1..=3).contains(&version) {
        return Err("a text format version that is not known");
    }
    let titles = c.le_u16().ok_or("no titles")?;
    for _ in 0..titles {
        c.le_u16().ok_or("a title is cut")?;
        let n = c.le_u16().ok_or("a title is cut")?;
        c.take(usize::from(n)).ok_or("a title is cut")?;
    }
    c.u8().ok_or("no contents")?;
    let count = c.le_u16().ok_or("no contents")?;
    let mut contents = Vec::new();
    for _ in 0..count {
        let language = c.le_u16().ok_or("a content is cut")?;
        let body = match version {
            1 | 2 => {
                let wide = version == 2;
                let text = sized(&mut c, wide).ok_or("a text is cut")?;
                let formatting = sized(&mut c, wide).ok_or("formatting data is cut")?;
                Body::Paragraphs(paragraphs(text, formatting, wide, page, fallback))
            }
            // Version 3.
            _ => {
                let html = sized(&mut c, true).ok_or("an HTML text is cut")?;
                c.take(4).ok_or("an HTML text is cut")?;
                Body::Html(utf8_or_legacy(html))
            }
        };
        contents.push(Content { language, body });
    }
    // In all 1,635 real texts the contents end the record exactly; bytes past
    // them mean the counts or lengths do not describe it.
    if c.left() != 0 {
        return Err("bytes after the contents");
    }
    Ok(GuidingText { contents })
}

/// Bytes after their length: a `u32` where `wide`, else a `u16`.
fn sized<'a>(c: &mut Cursor<'a>, wide: bool) -> Option<&'a [u8]> {
    let n = if wide { usize::try_from(c.le_u32()?).ok()? } else { usize::from(c.le_u16()?) };
    c.take(n)
}

/// The formatting data of a text of version 1, or of version 2 where `wide`.
#[derive(Default)]
struct Formatting<'a> {
    objects: Vec<Object<'a>>,
    styles: HashMap<u16, Style>,
    /// Lengths in bytes of the text, and the style of each.
    runs: Vec<(usize, u16)>,
}

struct Object<'a> {
    kind: u16,
    /// The index of the marker it stands at: its stored position less one.
    /// `None` for position 0, which no marker has.
    at: Option<usize>,
    data: &'a [u8],
}

/// Reads formatting data: its header, the objects, the styles and the runs.
/// The property lists after the runs are not read. `None` when it does not
/// read to the end of the runs.
fn formatting<'a>(b: &'a [u8], wide: bool, field: &impl Fn(&[u8]) -> String) -> Option<Formatting<'a>> {
    let mut c = Cursor::new(b);
    c.le_u16()?;
    let count = c.le_u16()?;
    c.le_u16()?;
    let mut objects = Vec::new();
    for _ in 0..count {
        let kind = c.le_u16()?;
        let position = if wide { usize::try_from(c.le_u32()?).ok()? } else { usize::from(c.le_u16()?) };
        let n = c.le_u16()?;
        objects.push(Object { kind, at: position.checked_sub(1), data: c.take(usize::from(n))? });
    }
    let count = c.le_u16()?;
    let mut styles = HashMap::new();
    for _ in 0..count {
        let id = c.le_u16()?;
        styles.insert(id, style(&mut c, field)?);
    }
    let mut runs = Vec::new();
    loop {
        let len = c.le_u16()?;
        if len == END_OF_RUNS {
            break;
        }
        let id = c.le_u16()?;
        c.le_u16()?;
        runs.push((usize::from(len), id));
    }
    Some(Formatting { objects, styles, runs })
}

/// A style's properties: a count, then each a `u16` key, a `u16` length and
/// the value. Keys 0 to 4 are the font, bold, italic, underline and size;
/// the others are not read.
fn style(c: &mut Cursor<'_>, field: &impl Fn(&[u8]) -> String) -> Option<Style> {
    let mut s = Style::default();
    for _ in 0..c.le_u16()? {
        let key = c.le_u16()?;
        let n = c.le_u16()?;
        let v = c.take(usize::from(n))?;
        let on = v.first().is_some_and(|&b| b != 0);
        match key {
            0 => s.font = counted(v).map(|name| field(name).chars().take(MAX_FONT_CHARS).collect()).unwrap_or_default(),
            1 => s.bold = on,
            2 => s.italic = on,
            3 => s.underline = on,
            4 => s.size = v.iter().take(4).rev().fold(0, |n, &b| n << 8 | u32::from(b)),
            _ => {}
        }
    }
    Some(s)
}

/// Bytes after their `u16` length, at the start of `b`.
fn counted(b: &[u8]) -> Option<&[u8]> {
    let mut c = Cursor::new(b);
    let n = c.le_u16()?;
    c.take(usize::from(n))
}

/// The paragraphs of text `b` with its formatting data `fmt`. The text is read
/// once, whole, in the page its words show ([`page_of`]), and cut where a run,
/// a line or an object begins or ends ([`single_byte_pieces`]); names, titles
/// and labels in the objects are read as the format's string fields are
/// ([`text`]). Neighbouring text of one style is one span.
fn paragraphs(b: &[u8], fmt: &[u8], wide: bool, page: CodePage, fallback: CodePage) -> Vec<Paragraph> {
    let read = page_of(b, page, fallback);
    let field = |s: &[u8]| text(s, page, fallback);
    let f = formatting(fmt, wide, &field).unwrap_or_default();
    let mut at_marker: HashMap<usize, Vec<&Object<'_>>> = HashMap::new();
    for o in &f.objects {
        if let Some(at) = o.at {
            at_marker.entry(at).or_default().push(o);
        }
    }
    // Each run's start and style, in order; past the last run, no style.
    let mut starts = Vec::with_capacity(f.runs.len());
    let mut end = 0usize;
    for &(len, id) in &f.runs {
        if len > 0 && end < b.len() {
            starts.push((end, id));
        }
        end = end.saturating_add(len);
    }
    if end < b.len() {
        starts.push((end, u16::MAX));
    }
    let style_at = |at: usize| {
        let run = starts.partition_point(|&(start, _)| start <= at);
        run.checked_sub(1).and_then(|r| f.styles.get(&starts[r].1)).cloned().unwrap_or_default()
    };
    // Cut at every run's start, and around every line end and marker.
    let mut cuts: Vec<usize> = starts.iter().map(|&(start, _)| start).filter(|&start| start > 0).collect();
    for (i, &byte) in b.iter().enumerate() {
        if matches!(byte, LINE | b'\n' | MARKER) {
            cuts.extend([i, i + 1]);
        }
    }
    cuts.retain(|&cut| cut > 0 && cut < b.len());
    cuts.sort_unstable();
    cuts.dedup();
    let pieces = single_byte_pieces(b, page, read, &cuts);

    let mut out = vec![Paragraph::default()];
    for (k, piece) in pieces.into_iter().enumerate() {
        let start = if k == 0 { 0 } else { cuts[k - 1] };
        match b.get(start) {
            Some(&LINE) => out.push(Paragraph::default()),
            Some(&b'\n') => {}
            Some(&MARKER) => {
                let style = style_at(start);
                for span in objects(at_marker.get(&start).map_or(&[][..], Vec::as_slice), &style, &field) {
                    push(&mut out, span);
                }
            }
            Some(_) if !piece.is_empty() => push(&mut out, Span::Text { text: piece, style: style_at(start) }),
            _ => {}
        }
    }
    if out.len() > 1 && out.last().is_some_and(|p| p.spans.is_empty()) {
        out.pop();
    }
    out
}

/// Adds `span` to the last paragraph, joining text to text of the same style.
fn push(out: &mut [Paragraph], span: Span) {
    let Some(p) = out.last_mut() else { return };
    if let (Some(Span::Text { text: last, style: last_style }), Span::Text { text, style }) =
        (p.spans.last_mut(), &span)
        && last_style == style
    {
        last.push_str(text);
        return;
    }
    p.spans.push(span);
}

/// The spans of the objects at one marker, in stored order: one diagram, one
/// game link, one text link and one list label at most, each from the first
/// object of its kind that reads. A list label takes `style`, the style of
/// its marker.
fn objects(at: &[&Object<'_>], style: &Style, field: &impl Fn(&[u8]) -> String) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut done = [false; 4];
    for o in at {
        let (slot, span) = match o.kind {
            k if DIAGRAMS.contains(&k) => (0, diagram(o.data)),
            GAME | LABELLED_GAME => (1, game_link(o.data, o.kind == LABELLED_GAME, field).map(Span::Game)),
            k if TEXT_LINKS.contains(&k) => (2, counted(o.data).map(|t| Span::TextLink { title: field(t) })),
            LIST_LABEL => (3, counted(o.data).map(|t| Span::Text { text: field(t), style: style.clone() })),
            _ => continue,
        };
        if let Some(span) = span.filter(|_| !done[slot]) {
            done[slot] = true;
            spans.push(span);
        }
    }
    spans
}

/// A diagram's data: a `u16`, then the 32 bytes of its squares
/// ([`diagram_board`]), then data that is not read.
fn diagram(d: &[u8]) -> Option<Span> {
    let squares = d.get(2..34)?.try_into().ok()?;
    diagram_board(squares).map(|board| Span::Diagram { board })
}

/// A game link's data: the search as a `u16` length and its bytes, then where
/// `labelled` the label as a `u16` length and its bytes. The search holds six
/// bytes, white and an unknown byte, black, seven unknown bytes, the
/// tournament, and fields that are not read; each name is a byte length and
/// its bytes.
fn game_link(d: &[u8], labelled: bool, field: &impl Fn(&[u8]) -> String) -> Option<GameLink> {
    let mut c = Cursor::new(d);
    let size = usize::from(c.le_u16()?);
    let search = c.take(size)?;
    let label = if labelled { counted(c.rest()).map(field).unwrap_or_default() } else { String::new() };
    let mut s = Cursor::new(search);
    s.take(6)?;
    let white = short(&mut s)?;
    s.u8()?;
    let black = short(&mut s)?;
    s.take(7)?;
    let event = short(&mut s)?;
    Some(GameLink { label, white: field(white), black: field(black), event: field(event) })
}

/// Bytes after their one-byte length.
fn short<'a>(c: &mut Cursor<'a>) -> Option<&'a [u8]> {
    let n = c.u8()?;
    c.take(usize::from(n))
}
