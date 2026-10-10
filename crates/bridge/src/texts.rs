//! The body of `GET /v1/databases/{id}/texts/{number}` (`docs/api.md`,
//! asavis/oschess-cb-bridge#324): a guiding text in every language stored,
//! its game links and text links resolved to records of the same database.

use std::collections::HashMap;

use cbformat::Error;
use cbformat::game::RecordKind;
use cbformat::game::guide::{Body, GameLink, GuidingText, Paragraph, Span, Style};
use cbformat::pgn::language_code;

use crate::json::{Obj, array};
use crate::rows::Names;
use crate::store::{Head, Store};

/// Most records the links of one text are looked for in, from the first: a
/// book's database holds a few thousand. A link not found in them has no
/// number.
pub const MAX_LINK_SCAN: u32 = 100_000;
/// Headers read at once while links are looked for.
const SCAN_BATCH: u32 = 4096;

/// The record numbers the links of a text name, in the order the text holds
/// them; `None` for a link no record matches.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    pub games: Vec<Option<u32>>,
    pub texts: Vec<Option<u32>>,
}

/// The links of `text`, in text order.
fn links(text: &GuidingText) -> (Vec<&GameLink>, Vec<&str>) {
    let (mut games, mut texts) = (Vec::new(), Vec::new());
    for span in spans(text) {
        match span {
            Span::Game(link) => games.push(link),
            Span::TextLink { title } => texts.push(title.as_str()),
            _ => {}
        }
    }
    (games, texts)
}

fn spans(text: &GuidingText) -> impl Iterator<Item = &Span> {
    text.contents
        .iter()
        .filter_map(|c| match &c.body {
            Body::Paragraphs(p) => Some(p),
            Body::Html(_) => None,
        })
        .flatten()
        .flat_map(|p| &p.spans)
}

/// A name as a link and a player entity are compared: the words of each part
/// before and after its first comma, single-spaced, and the last name alone
/// where the first is empty. A link writes `Last,First` (or `Last,`), an
/// entity `Last, First` (or `Last`).
fn name_key(name: &str) -> String {
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    match name.split_once(',') {
        Some((last, first)) if !first.trim().is_empty() => format!("{},{}", words(last), words(first)),
        Some((last, _)) => words(last),
        None => words(name),
    }
}

/// A name read for a comparison: one that cannot be read, being damaged,
/// matches nothing, while a failed read is still an error.
fn soft(name: cbformat::Result<String>) -> cbformat::Result<String> {
    match name {
        Err(e @ Error::Io(..)) => Err(e),
        Err(_) => Ok(String::new()),
        ok => ok,
    }
}

/// A link's name as a row names a player, `Last, First`.
fn display_name(name: &str) -> String {
    match name.split_once(',') {
        Some((last, first)) if !first.trim().is_empty() => format!("{}, {}", last.trim(), first.trim()),
        Some((last, _)) => last.trim().to_string(),
        None => name.trim().to_string(),
    }
}

/// Finds the record each link of `text` names, in one pass over the first
/// [`MAX_LINK_SCAN`] records, which ends once every link is found.
///
/// A game link names the first game, in record order, whose white is the
/// link's white, whose black is the link's black where the link names one,
/// and whose tournament is the link's where it names one. Names compare as
/// [`name_key`] gives them. A text link names the first guiding text whose
/// title is the link's.
pub fn resolve<S: Store>(db: &S, text: &GuidingText) -> cbformat::Result<Resolved> {
    let (games, texts) = links(text);
    let mut out = Resolved { games: vec![None; games.len()], texts: vec![None; texts.len()] };
    let mut by_white: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, link) in games.iter().enumerate() {
        by_white.entry(name_key(&link.white)).or_default().push(i);
    }
    let mut by_title: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, title) in texts.iter().enumerate() {
        by_title.entry(title.trim()).or_default().push(i);
    }
    let mut names = Names::new(db);
    let last = db.record_count().min(MAX_LINK_SCAN);
    let mut first = 1;
    while first <= last && !(by_white.is_empty() && by_title.is_empty()) {
        let records = db.records(first, last.min(first.saturating_add(SCAN_BATCH - 1)))?;
        let Some(end) = records.last().map(|r| r.id()) else { break };
        for r in &records {
            match r.kind() {
                RecordKind::Game if !by_white.is_empty() => {
                    let white = name_key(&soft(names.player(r.white()))?);
                    let Some(pending) = by_white.get_mut(&white) else { continue };
                    let black = name_key(&soft(names.player(r.black()))?);
                    let event = soft(names.tournament(r.tournament()).map(|(title, _)| title))?;
                    pending.retain(|&i| {
                        let link = games[i];
                        let found = (link.black.trim().is_empty() || name_key(&link.black) == black)
                            && (link.event.trim().is_empty() || link.event.trim() == event.trim());
                        if found {
                            out.games[i] = Some(r.id());
                        }
                        !found
                    });
                    if pending.is_empty() {
                        by_white.remove(&white);
                    }
                }
                RecordKind::Text if !by_title.is_empty() => {
                    let Some((key, _)) = r.other() else { continue };
                    let title = soft(names.title(key))?;
                    if let Some(pending) = by_title.remove(title.trim()) {
                        for i in pending {
                            out.texts[i] = Some(r.id());
                        }
                    }
                }
                _ => {}
            }
        }
        first = end.saturating_add(1);
        if end == u32::MAX {
            break;
        }
    }
    Ok(out)
}

/// The `contents` of the answer: each language's text, its links numbered as
/// `resolved` found them.
pub fn contents(text: &GuidingText, resolved: &Resolved) -> String {
    let mut games = resolved.games.iter().copied();
    let mut texts = resolved.texts.iter().copied();
    array(text.contents.iter().map(|c| {
        let content = Obj::new().str("lang", &language_code(c.language));
        match &c.body {
            Body::Html(html) => content.str("html", html),
            Body::Paragraphs(paragraphs) => {
                content.raw("paragraphs", &array(paragraphs.iter().map(|p| paragraph(p, &mut games, &mut texts))))
            }
        }
        .done()
    }))
}

fn paragraph(
    p: &Paragraph,
    games: &mut dyn Iterator<Item = Option<u32>>,
    texts: &mut dyn Iterator<Item = Option<u32>>,
) -> String {
    let spans: Vec<String> = p.spans.iter().map(|s| span(s, &mut *games, &mut *texts)).collect();
    Obj::new().raw("spans", &array(spans)).done()
}

/// One span; a link takes the next number of its kind.
fn span(
    s: &Span,
    games: &mut dyn Iterator<Item = Option<u32>>,
    texts: &mut dyn Iterator<Item = Option<u32>>,
) -> String {
    match s {
        Span::Text { text, style } => styled(Obj::new().str("text", text), style),
        Span::Diagram { board } => Obj::new().raw("diagram", &Obj::new().str("board", board).done()),
        Span::Game(link) => {
            let game = Obj::new()
                .str("label", &link.label)
                .str("white", &display_name(&link.white))
                .str("black", &display_name(&link.black))
                .str("event", link.event.trim());
            Obj::new().raw("game", &number(game, games.next().flatten()).done())
        }
        Span::TextLink { title } => {
            let link = Obj::new().str("title", title.trim());
            Obj::new().raw("textLink", &number(link, texts.next().flatten()).done())
        }
    }
    .done()
}

fn styled(o: Obj, s: &Style) -> Obj {
    o.str("font", &s.font)
        .num("size", i64::from(s.size))
        .bool("bold", s.bold)
        .bool("italic", s.italic)
        .bool("underline", s.underline)
}

fn number(o: Obj, n: Option<u32>) -> Obj {
    match n {
        Some(n) => o.num("number", n),
        None => o.raw("number", "null"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_compare_by_their_words() {
        assert_eq!(name_key("Coull,Alison"), name_key("Coull, Alison"));
        assert_eq!(name_key(" Coull ,  Alison "), "Coull,Alison");
        assert_eq!(name_key("Moravec,"), name_key("Moravec"));
        assert_eq!(name_key("1.1"), "1.1");
        assert_ne!(name_key("Coull,Alison"), name_key("Coull,Alice"));
        assert_eq!(display_name("Coull,Alison"), "Coull, Alison");
        assert_eq!(display_name("Moravec,"), "Moravec");
        assert_eq!(display_name("1.1"), "1.1");
    }
}
