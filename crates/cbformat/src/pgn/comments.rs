//! A game's annotations as PGN: comments, NAGs and `[%csl]` / `[%cal]`
//! graphics, placed by position in the numbering of the game's format: PGN
//! order for 2CBH, stored order for the classic format. The reading form
//! writes one language and game quotations as ChessBase writes them; the full
//! form (asavis/oschess-cb-bridge#42) writes every text as its own comment led
//! by `[%lang]`, and every other annotation as a command (`commands.rs`).
//! Both forms write the main line's evaluations as ChessBase does,
//! `[%evp]`; the full form also writes each move's `[%eval]` and `[%emt]`.

use std::collections::BTreeMap;

use super::Options;
use super::commands;
use super::san::{file_char, rank_char};
use super::tree::{At, Notes};
use crate::game::timing::{self, Score};
use crate::game::{Annotation, GAME_POSITION, GameAnnotations, language};
use crate::movetable::Sq;
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
    full: bool,
    /// For the full form: the type that ended decoding and the bytes after it.
    undecoded: Option<(u16, &'a [u8])>,
    /// The game's evaluations as `[%evp]`, and each main-line move's score
    /// from them, by the move's stored index.
    evp: Option<String>,
    scores: BTreeMap<u32, Score>,
}

impl<'a> Commentary<'a> {
    /// The annotations of a game with `moves` moves, whose main line is the
    /// stored moves `main_line`.
    pub(super) fn new(
        annotations: &'a GameAnnotations,
        order: PositionOrder,
        options: &Options,
        moves: u32,
        main_line: &[u32],
    ) -> Self {
        let last = main_line.last().copied();
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
        let undecoded = annotations.stopped_at.map(|u| (u.type_code, annotations.undecoded.as_slice()));
        // Type 26 on the game: entry `k` is the position after the main
        // line's ply `k`, the first the start position.
        let evaluations = by_position.get(&GAME_POSITION).into_iter().flatten().find_map(|a| match a {
            Annotation::Other { code: 0x26, data } => timing::evaluations(data, order == PositionOrder::Stored),
            _ => None,
        });
        let evp = evaluations.as_deref().and_then(commands::evp);
        let scores = main_line
            .iter()
            .enumerate()
            .filter_map(|(k, &stored)| Some((stored, evaluations.as_ref()?.get(k + 1)?.score()?)))
            .collect();
        Commentary { by_position, past_end, last, language, order, full: options.full, undecoded, evp, scores }
    }

    /// The annotations at the move `at`, in this game's numbering.
    fn at(&self, at: At) -> Option<&Vec<&'a Annotation>> {
        let position = match self.order {
            PositionOrder::Pgn => at.pgn,
            PositionOrder::Stored => at.stored,
        };
        self.by_position.get(&(position as i32))
    }

    /// The comment on the game as a whole, before the first move: its texts,
    /// graphics and quotations, and in the full form its commands and the
    /// record's undecoded rest. Says whether it wrote one.
    pub(super) fn game_comment(&self, out: &mut String) -> bool {
        let anns = self.by_position.get(&GAME_POSITION).map_or(&[][..], Vec::as_slice);
        // A comment of its own, first, as ChessBase writes it.
        let evp = self.evp.as_ref().is_some_and(|e| comment(out, std::slice::from_ref(e), "", " "));
        if self.full {
            let mut wrote = comment(out, &graphics(anns), "", " ") || evp;
            for t in self.tagged(anns, None) {
                wrote |= comment(out, &[t], "", " ");
            }
            let mut cmds = self.commands(anns);
            cmds.extend(self.undecoded.map(|(t, d)| commands::rest(t, d)));
            return comment(out, &cmds, "", " ") || wrote;
        }
        let mut parts = self.medals(anns);
        parts.extend(graphics(anns));
        parts.extend(self.texts(anns, true));
        parts.extend(self.texts(anns, false));
        parts.extend(self.quotes(anns));
        comment(out, &parts, "", " ") || evp
    }

    /// The full form's engine score and time spent for the move `at`: its own
    /// evaluation (type 21), else the main line's (type 26), and its own time
    /// spent (type 07).
    fn timing(&self, at: At, own: &[&Annotation]) -> Vec<String> {
        let classic = self.order == PositionOrder::Stored;
        let own_score = own.iter().find_map(|a| match a {
            Annotation::Other { code: 0x21, data } => timing::engine_evaluation(data, classic),
            _ => None,
        });
        let score = own_score.or_else(|| self.scores.get(&at.stored).copied());
        let spent = own.iter().find_map(|a| match a {
            Annotation::Other { code: 0x07, data } => timing::time_spent(data, classic),
            _ => None,
        });
        score.map(commands::eval).into_iter().chain(spent.map(commands::emt)).collect()
    }

    /// The full form's texts in stored order, each a comment of its own:
    /// `[%lang xx] text`, cleaned for PGN, followed by `[%cbtext]` with the
    /// original when the comment cannot hold it as it is. Those before the
    /// move, those after it, or (`None`) all.
    fn tagged(&self, anns: &[&Annotation], before: Option<bool>) -> Vec<String> {
        let mut out = Vec::new();
        for a in anns {
            let Annotation::Text { before: b, language: l, text } = a else { continue };
            if before.is_some_and(|x| x != *b) {
                continue;
            }
            let t = clean(text);
            if !t.is_empty() {
                out.push(format!("[%lang {}] {t}", commands::language_code(*l)));
            }
            if commands::text_needs_original(*b, text, &t) {
                out.push(commands::text(*l, *b, t.is_empty(), text));
            }
        }
        out
    }

    /// The full form's commands: every annotation that is not a text, with
    /// all its data; symbols and graphics are shown by NAGs and `[%csl]` /
    /// `[%cal]` as well.
    fn commands(&self, anns: &[&Annotation]) -> Vec<String> {
        anns.iter()
            .filter_map(|a| match a {
                Annotation::Other { code, data } => Some(commands::for_other(*code, data, self.order)),
                a => commands::graphic(a),
            })
            .collect()
    }

    /// The reading form's medals, `[%mdl]` as ChessBase writes them.
    fn medals(&self, anns: &[&Annotation]) -> Vec<String> {
        anns.iter()
            .filter_map(|a| match a {
                Annotation::Other { code, data } => commands::medal(*code, data, self.order),
                _ => None,
            })
            .collect()
    }

    /// The reading form's game quotations, as ChessBase writes them.
    fn quotes(&self, anns: &[&Annotation]) -> Vec<String> {
        anns.iter().filter_map(|a| commands::quotation_text(a, self.order)).map(|q| clean(&q)).collect()
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
        if self.full {
            let mut wrote = false;
            for t in self.tagged(anns, Some(true)) {
                wrote |= comment(out, &[t], "", " ");
            }
            return wrote;
        }
        let text: Vec<String> = self.texts(anns, true).into_iter().collect();
        comment(out, &text, "", " ")
    }

    fn after(&mut self, at: At, out: &mut String) -> bool {
        let own = self.at(at).map_or(&[][..], Vec::as_slice);
        // Past the last move, every text follows the main line's last move,
        // those meant to precede a move included.
        let past_end = if self.last == Some(at.stored) { &self.past_end[..] } else { &[] };
        let timing = if self.full { self.timing(at, own) } else { Vec::new() };
        if own.is_empty() && past_end.is_empty() && timing.is_empty() {
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
        if self.full {
            let mut wrote = comment(out, &timing, " ", "");
            wrote |= comment(out, &graphics(&all), " ", "");
            let mut texts = self.tagged(own, Some(false));
            for anns in past_end {
                texts.extend(self.tagged(anns, None));
            }
            for t in texts {
                wrote |= comment(out, &[t], " ", "");
            }
            return comment(out, &self.commands(&all), " ", "") || wrote;
        }
        let mut parts = self.medals(&all);
        parts.extend(graphics(&all));
        parts.extend(self.texts(own, false));
        for anns in past_end {
            parts.extend(self.texts(anns, true));
            parts.extend(self.texts(anns, false));
        }
        parts.extend(self.quotes(&all));
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
