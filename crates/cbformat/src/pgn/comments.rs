//! A game's annotations as PGN: comments, NAGs and `[%csl]` / `[%cal]`
//! graphics, placed by position in PGN order.

use std::collections::BTreeMap;

use super::Options;
use super::san::{file_char, rank_char};
use super::tree::Notes;
use crate::movetable::Sq;
use crate::v2::{Annotation, GAME_POSITION, GameAnnotations, language};

/// The annotations of one game, grouped by position, with the one language
/// its texts are written in.
pub(super) struct Commentary<'a> {
    by_position: BTreeMap<i32, Vec<&'a Annotation>>,
    language: Option<u16>,
}

impl<'a> Commentary<'a> {
    pub(super) fn new(annotations: &'a GameAnnotations, options: &Options) -> Self {
        let mut by_position: BTreeMap<i32, Vec<&Annotation>> = BTreeMap::new();
        for b in &annotations.blocks {
            by_position.entry(b.position).or_default().extend(&b.annotations);
        }
        let languages: Vec<u16> = annotations
            .blocks
            .iter()
            .flat_map(|b| &b.annotations)
            .filter_map(|a| match a {
                Annotation::Text { language, .. } if *language != language::ANY => Some(*language),
                _ => None,
            })
            .collect();
        // The first preferred language the game has, else English, else the
        // first one stored. Texts for any language are always written.
        let language = options
            .languages
            .iter()
            .copied()
            .chain([language::ENGLISH])
            .find(|l| languages.contains(l))
            .or(languages.first().copied());
        Commentary { by_position, language }
    }

    /// The comment on the game as a whole, before the first move: its texts
    /// and graphics. Says whether it wrote one.
    pub(super) fn game_comment(&self, out: &mut String) -> bool {
        let Some(anns) = self.by_position.get(&GAME_POSITION) else { return false };
        let mut parts = graphics(anns);
        parts.extend(self.texts(anns, true));
        parts.extend(self.texts(anns, false));
        comment(out, &parts, "", " ")
    }

    fn texts(&self, anns: &[&Annotation], before: bool) -> Option<String> {
        let texts: Vec<String> = anns
            .iter()
            .filter_map(|a| match a {
                Annotation::Text { before: b, language: l, text }
                    if *b == before && (*l == language::ANY || Some(*l) == self.language) =>
                {
                    Some(clean(text))
                }
                _ => None,
            })
            .filter(|t| !t.is_empty())
            .collect();
        (!texts.is_empty()).then(|| texts.join(" "))
    }
}

impl Notes for Commentary<'_> {
    fn before(&mut self, index: u32, out: &mut String) -> bool {
        let Some(anns) = self.by_position.get(&(index as i32)) else { return false };
        let text: Vec<String> = self.texts(anns, true).into_iter().collect();
        comment(out, &text, "", " ")
    }

    fn after(&mut self, index: u32, out: &mut String) -> bool {
        let Some(anns) = self.by_position.get(&(index as i32)) else { return false };
        for a in anns {
            if let Annotation::Symbols { on_move, on_position, prefix } = a {
                for nag in [on_move, on_position, prefix] {
                    if *nag != 0 {
                        out.push_str(" $");
                        out.push_str(&nag.to_string());
                    }
                }
            }
        }
        let mut parts = graphics(anns);
        parts.extend(self.texts(anns, false));
        comment(out, &parts, " ", "")
    }
}

/// Writes `{parts}` between `lead` and `trail` when there are parts.
fn comment(out: &mut String, parts: &[String], lead: &str, trail: &str) -> bool {
    if parts.is_empty() {
        return false;
    }
    out.push_str(lead);
    out.push('{');
    out.push_str(&parts.join(" "));
    out.push('}');
    out.push_str(trail);
    true
}

/// `[%csl ...]` and `[%cal ...]`, each when there is at least one mark of a
/// known colour. ChessBase's colours 7, 8 and 9 are unknown and left out.
fn graphics(anns: &[&Annotation]) -> Vec<String> {
    let (mut squares, mut arrows) = (Vec::new(), Vec::new());
    for a in anns {
        match a {
            Annotation::Squares(v) => {
                squares.extend(v.iter().filter_map(|s| Some(format!("{}{}", colour(s.colour)?, name(s.square)))))
            }
            Annotation::Arrows(v) => arrows
                .extend(v.iter().filter_map(|a| Some(format!("{}{}{}", colour(a.colour)?, name(a.from), name(a.to))))),
            _ => {}
        }
    }
    let mut parts = Vec::new();
    if !squares.is_empty() {
        parts.push(format!("[%csl {}]", squares.join(",")));
    }
    if !arrows.is_empty() {
        parts.push(format!("[%cal {}]", arrows.join(",")));
    }
    if parts.len() == 2 {
        // One token, as ChessBase and Lichess write them.
        let cal = parts.pop().unwrap_or_default();
        parts[0].push_str(&cal);
    }
    parts
}

fn colour(c: u8) -> Option<char> {
    match c {
        2 => Some('G'),
        3 => Some('Y'),
        4 => Some('R'),
        _ => None,
    }
}

fn name(sq: Sq) -> String {
    let s = chesscore::Square::new(sq & 7, sq >> 3);
    format!("{}{}", file_char(s), rank_char(s))
}

/// A text fit for a PGN comment: no braces, no line breaks.
fn clean(text: &str) -> String {
    let t: String = text
        .chars()
        .map(|c| match c {
            '{' => '(',
            '}' => ')',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}
