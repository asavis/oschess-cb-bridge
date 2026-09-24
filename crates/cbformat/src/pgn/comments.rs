//! A game's annotations as PGN: comments, NAGs and `[%csl]` / `[%cal]`
//! graphics, placed by position in the numbering of the game's format: PGN
//! order for 2CBH, stored order for the classic format.

use std::collections::BTreeMap;

use super::Options;
use super::san::{file_char, rank_char};
use super::tree::{At, Notes};
use crate::movetable::Sq;
use crate::v2::{Annotation, GAME_POSITION, GameAnnotations, language};
use crate::view::PositionOrder;

/// The annotations of one game, grouped by position, with the one language
/// its texts are written in.
pub(super) struct Commentary<'a> {
    by_position: BTreeMap<i32, Vec<&'a Annotation>>,
    /// The annotations past the game's last move, by position: they follow
    /// the main line's last move.
    past_end: Vec<Vec<&'a Annotation>>,
    /// The main line's last move in stored order.
    last: Option<u32>,
    language: Option<u16>,
    order: PositionOrder,
}

impl<'a> Commentary<'a> {
    /// The annotations of a game with `moves` moves whose main line ends at
    /// the stored move `last`.
    pub(super) fn new(
        annotations: &'a GameAnnotations,
        order: PositionOrder,
        options: &Options,
        moves: u32,
        last: Option<u32>,
    ) -> Self {
        let mut by_position: BTreeMap<i32, Vec<&Annotation>> = BTreeMap::new();
        for b in &annotations.blocks {
            by_position.entry(b.position).or_default().extend(&b.annotations);
        }
        let past_end = by_position.split_off(&i32::try_from(moves).unwrap_or(i32::MAX)).into_values().collect();
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
        Commentary { by_position, past_end, last, language, order }
    }

    /// The annotations at the move `at`, in this game's numbering.
    fn at(&self, at: At) -> Option<&Vec<&'a Annotation>> {
        let position = match self.order {
            PositionOrder::Pgn => at.pgn,
            PositionOrder::Stored => at.stored,
        };
        self.by_position.get(&(position as i32))
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
    fn before(&mut self, at: At, out: &mut String) -> bool {
        let Some(anns) = self.at(at) else { return false };
        let text: Vec<String> = self.texts(anns, true).into_iter().collect();
        comment(out, &text, "", " ")
    }

    fn after(&mut self, at: At, out: &mut String) -> bool {
        let own = self.at(at).map_or(&[][..], Vec::as_slice);
        // Past the last move, every text follows the main line's last move,
        // those meant to precede a move included.
        let past_end = if self.last == Some(at.stored) { &self.past_end[..] } else { &[] };
        if own.is_empty() && past_end.is_empty() {
            return false;
        }
        let all: Vec<&Annotation> = own.iter().chain(past_end.iter().flatten()).copied().collect();
        for a in &all {
            if let Annotation::Symbols { on_move, on_position, prefix } = a {
                for nag in [on_move, on_position, prefix] {
                    if *nag != 0 {
                        out.push_str(" $");
                        out.push_str(&nag.to_string());
                    }
                }
            }
        }
        let mut parts = graphics(&all);
        parts.extend(self.texts(own, false));
        for anns in past_end {
            parts.extend(self.texts(anns, true));
            parts.extend(self.texts(anns, false));
        }
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
