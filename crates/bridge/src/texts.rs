//! The body of `GET /v1/databases/{id}/texts/{number}` (`docs/api.md`,
//! asavis/oschess-cb-bridge#324): a guiding text in every language stored,
//! its game links and text links resolved to records of the same database.

use std::collections::HashMap;

use cbformat::Error;
use cbformat::game::RecordKind;
use cbformat::game::guide::{Body, GameLink, GuidingText, Paragraph, Span, Style};
use cbformat::pgn::language_code;

use crate::json::{Obj, array, string_len};
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

/// A link's name as a row names a player, `Last, First`.
fn display_name(name: &str) -> String {
    match name.split_once(',') {
        Some((last, first)) if !first.trim().is_empty() => format!("{}, {}", last.trim(), first.trim()),
        Some((last, _)) => last.trim().to_string(),
        None => name.trim().to_string(),
    }
}

/// The names a link is compared with, each read once and in full: a row's
/// names are cut for display, a comparison takes them as stored.
struct Stored<'a, S: Store> {
    db: &'a S,
    players: HashMap<i64, String>,
    tournaments: HashMap<i64, String>,
    titles: HashMap<i64, String>,
}

impl<'a, S: Store> Stored<'a, S> {
    fn player(&mut self, id: i64) -> cbformat::Result<String> {
        if let Some(name) = self.players.get(&id) {
            return Ok(name.clone());
        }
        let name = name_key(&soft(self.db.player(id).map(|p| p.map(|p| p.pgn())))?);
        self.players.insert(id, name.clone());
        Ok(name)
    }

    fn tournament(&mut self, id: i64) -> cbformat::Result<String> {
        if let Some(title) = self.tournaments.get(&id) {
            return Ok(title.clone());
        }
        let title = soft(self.db.tournament(id).map(|t| t.map(|t| t.title)))?.trim().to_string();
        self.tournaments.insert(id, title.clone());
        Ok(title)
    }

    fn title(&mut self, key: i64) -> cbformat::Result<String> {
        if let Some(title) = self.titles.get(&key) {
            return Ok(title.clone());
        }
        let title = soft(self.db.title(key))?.trim().to_string();
        self.titles.insert(key, title.clone());
        Ok(title)
    }
}

/// A name read for a comparison: one that cannot be read, being damaged,
/// matches nothing, while a failed read is still an error.
fn soft(name: cbformat::Result<Option<String>>) -> cbformat::Result<String> {
    match name {
        Ok(name) => Ok(name.unwrap_or_default()),
        Err(e @ Error::Io(..)) => Err(e),
        Err(_) => Ok(String::new()),
    }
}

/// Finds the record each link of `text` names, in one pass over the first
/// [`MAX_LINK_SCAN`] records, which ends once every link is found.
///
/// A game link names the first game, in record order, whose white, black and
/// tournament are the link's where the link names them; a link that names
/// none of the three names no game. Names compare as [`name_key`] gives them,
/// as stored, never cut for display. A text link names the first guiding
/// text whose title is the link's.
pub fn resolve<S: Store>(db: &S, text: &GuidingText) -> cbformat::Result<Resolved> {
    let (games, texts) = links(text);
    let mut out = Resolved { games: vec![None; games.len()], texts: vec![None; texts.len()] };
    struct Search {
        white: String,
        black: String,
        event: String,
    }
    let searches: Vec<Search> = games
        .iter()
        .map(|link| Search {
            white: name_key(&link.white),
            black: name_key(&link.black),
            event: link.event.trim().to_string(),
        })
        .collect();
    // Links by their white; those with none are tried on every game.
    let mut by_white: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut any_white: Vec<usize> = Vec::new();
    for (i, search) in searches.iter().enumerate() {
        match (search.white.is_empty(), search.black.is_empty() && search.event.is_empty()) {
            (false, _) => by_white.entry(search.white.as_str()).or_default().push(i),
            (true, false) => any_white.push(i),
            (true, true) => {}
        }
    }
    let mut by_title: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, title) in texts.iter().enumerate() {
        if !title.trim().is_empty() {
            by_title.entry(title.trim()).or_default().push(i);
        }
    }
    let mut names = Stored { db, players: HashMap::new(), tournaments: HashMap::new(), titles: HashMap::new() };
    let last = db.record_count().min(MAX_LINK_SCAN);
    let mut first = 1;
    while first <= last && !(by_white.is_empty() && any_white.is_empty() && by_title.is_empty()) {
        let records = db.records(first, last.min(first.saturating_add(SCAN_BATCH - 1)))?;
        let Some(end) = records.last().map(|r| r.id()) else { break };
        for r in &records {
            match r.kind() {
                RecordKind::Game if !(by_white.is_empty() && any_white.is_empty()) => {
                    let white = names.player(r.white())?;
                    let mut found = |pending: &mut Vec<usize>, names: &mut Stored<'_, S>| -> cbformat::Result<()> {
                        // Black and the tournament are read only for a game a link may name.
                        let mut black = None;
                        let mut event = None;
                        let mut kept = Vec::with_capacity(pending.len());
                        for &i in pending.iter() {
                            let search = &searches[i];
                            let mut matches = true;
                            if !search.black.is_empty() {
                                if black.is_none() {
                                    black = Some(names.player(r.black())?);
                                }
                                matches &= black.as_deref() == Some(search.black.as_str());
                            }
                            if matches && !search.event.is_empty() {
                                if event.is_none() {
                                    event = Some(names.tournament(r.tournament())?);
                                }
                                matches &= event.as_deref() == Some(search.event.as_str());
                            }
                            if matches {
                                out.games[i] = Some(r.id());
                            } else {
                                kept.push(i);
                            }
                        }
                        *pending = kept;
                        Ok(())
                    };
                    if let Some(pending) = by_white.get_mut(white.as_str()) {
                        found(pending, &mut names)?;
                        if pending.is_empty() {
                            by_white.remove(white.as_str());
                        }
                    }
                    if !any_white.is_empty() {
                        found(&mut any_white, &mut names)?;
                    }
                }
                RecordKind::Text if !by_title.is_empty() => {
                    let Some((key, _)) = r.other() else { continue };
                    let title = names.title(key)?;
                    if let Some(pending) = by_title.remove(title.as_str()) {
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

/// An upper bound of the length of [`contents`]: the escaped text of every
/// field and room for the keys and numbers of each span, so that an answer
/// over the limit is refused, and its budget reserved, before it is built.
pub fn contents_len(text: &GuidingText) -> usize {
    let mut len = 16;
    for content in &text.contents {
        len += 64;
        match &content.body {
            Body::Html(html) => len += string_len(html),
            Body::Paragraphs(paragraphs) => {
                for paragraph in paragraphs {
                    len += 16;
                    for span in &paragraph.spans {
                        len += 128
                            + match span {
                                Span::Text { text, style } => string_len(text) + string_len(&style.font),
                                Span::Diagram { board } => string_len(board),
                                Span::Game(link) => {
                                    string_len(&link.label)
                                        + string_len(&link.white)
                                        + string_len(&link.black)
                                        + string_len(&link.event)
                                }
                                Span::TextLink { title } => string_len(title),
                            };
                    }
                }
            }
        }
    }
    len
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
