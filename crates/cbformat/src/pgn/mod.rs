//! PGN output: standard algebraic notation, a game's full move tree and its
//! annotations.

mod comments;
mod san;
mod tree;

pub use san::san;

use crate::replay::{self, start_board};
use crate::v2::{Database, GameAnnotations, GameMoves, Record, RecordKind, Start, language};
use crate::{Error, Result};
use comments::Commentary;
use tree::{Bare, TreeBuilder, emit};

/// Choices for PGN output.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// ChessBase language numbers ([`language`]) in order of preference. A
    /// game's comments are written in the first of them it has, else in
    /// English, else in the first language stored; texts meant for any
    /// language are always written.
    pub languages: Vec<u16>,
}

impl Options {
    /// Options preferring the ISO 639-1 languages `codes`, in order; codes
    /// ChessBase has no number for are passed over.
    pub fn with_languages<'a>(codes: impl IntoIterator<Item = &'a str>) -> Self {
        Options { languages: codes.into_iter().filter_map(language::from_iso).collect() }
    }
}

/// How much of a game's annotations the PGN holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnotationStatus {
    /// The game has no annotations, or the database has no annotation file.
    None,
    /// Every annotation was decoded. Types PGN has no form for (training,
    /// clocks, game quotations and the like) are left out by design.
    Complete,
    /// A type of unknown layout ended decoding: annotations stored after it
    /// are missing.
    Incomplete { type_code: u16 },
}

/// A game as PGN, and how much of its annotations it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    pub pgn: String,
    pub annotations: AnnotationStatus,
}

/// The move tree of a game as PGN movetext, without the result or annotations.
pub fn movetext(db: &Database, record: &Record) -> Result<String> {
    movetext_of(&db.moves_of(record)?.moves()?)
}

/// The move tree of a parsed move record as PGN movetext, without annotations.
pub fn movetext_of(moves: &GameMoves<'_>) -> Result<String> {
    write_movetext(moves, None, &Options::default())
}

/// The move tree with its annotations: comments, NAGs and graphics.
pub fn movetext_annotated(moves: &GameMoves<'_>, annotations: &GameAnnotations, options: &Options) -> Result<String> {
    write_movetext(moves, Some(annotations), options)
}

fn write_movetext(moves: &GameMoves<'_>, annotations: Option<&GameAnnotations>, options: &Options) -> Result<String> {
    let mut tree = TreeBuilder::new();
    // walk() checks every move and the tree's shape, so a damaged record is an
    // error here exactly as it is in `cbtool verify`.
    replay::walk(moves, &mut tree)?;
    let mut out = String::with_capacity(tree.text_len());
    match annotations.filter(|a| !a.is_empty()) {
        Some(a) => {
            let mut commentary = Commentary::new(a, options);
            commentary.game_comment(&mut out);
            emit(&tree, &mut commentary, &mut out);
        }
        None => emit(&tree, &mut Bare, &mut out),
    }
    let len = out.trim_end().len();
    out.truncate(len);
    Ok(out)
}

fn tag(out: &mut String, name: &str, value: &str) {
    out.push('[');
    out.push_str(name);
    out.push_str(" \"");
    for c in value.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push_str("\"]\n");
}

/// A game as a complete PGN record with the seven-tag roster, Elo tags, for
/// games not from the standard position `SetUp`/`FEN`, and its annotations
/// with the default [`Options`].
pub fn game(db: &Database, id: u32) -> Result<String> {
    game_with(db, id, &Options::default()).map(|r| r.pgn)
}

/// [`game`] with `options`, and how much of the annotations it holds.
pub fn game_with(db: &Database, id: u32, options: &Options) -> Result<Rendered> {
    let r = db.record(id)?;
    if r.kind() != RecordKind::Game {
        return Err(Error::Format(format!("record {id} is not a game")));
    }
    let data = db.moves_of(&r)?;
    let annotations = db.annotations_of(&r)?;
    game_from(db, &r, &data.moves()?, annotations.as_ref(), options)
}

/// [`game_with`] for a record, move record and annotations already read, as
/// from a [`crate::v2::Batch`]; entities are read from `db`. `annotations` is
/// `None` when the database has no annotation file.
pub fn game_from(
    db: &Database,
    r: &Record,
    moves: &GameMoves<'_>,
    annotations: Option<&GameAnnotations>,
    options: &Options,
) -> Result<Rendered> {
    if r.kind() != RecordKind::Game {
        return Err(Error::Format(format!("record {} is not a game", r.id())));
    }
    let e = db.entities();
    let t = e.tournament(r.tournament())?;
    let name = |pid| -> Result<String> {
        Ok(e.player(pid)?.map(|p| p.pgn()).filter(|s| !s.is_empty()).unwrap_or_else(|| "?".into()))
    };
    let mut out = String::new();
    tag(&mut out, "Event", t.as_ref().map(|t| t.title.as_str()).filter(|s| !s.is_empty()).unwrap_or("?"));
    tag(&mut out, "Site", t.as_ref().map(|t| t.place.as_str()).filter(|s| !s.is_empty()).unwrap_or("?"));
    tag(&mut out, "Date", &r.played_date().pgn());
    let round = match (r.round(), r.subround()) {
        (0, _) => "?".to_string(),
        (n, 0) => n.to_string(),
        (n, s) => format!("{n}({s})"),
    };
    tag(&mut out, "Round", &round);
    tag(&mut out, "White", &name(r.white())?);
    tag(&mut out, "Black", &name(r.black())?);
    tag(&mut out, "Result", r.result().pgn());
    if r.white_elo() > 0 {
        tag(&mut out, "WhiteElo", &r.white_elo().to_string());
    }
    if r.black_elo() > 0 {
        tag(&mut out, "BlackElo", &r.black_elo().to_string());
    }
    if let Some(eco) = r.eco().pgn() {
        tag(&mut out, "ECO", &eco);
    }
    let start = moves.start()?;
    if start != Start::Standard {
        let board = start_board(&start)?;
        if moves.is_chess960() {
            tag(&mut out, "Variant", "Chess960");
        }
        tag(&mut out, "SetUp", "1");
        tag(&mut out, "FEN", &format!("{board}"));
    }
    out.push('\n');
    let text = write_movetext(moves, annotations, options)?;
    if !text.is_empty() {
        out.push_str(&text);
        out.push(' ');
    }
    out.push_str(r.result().pgn());
    out.push('\n');
    let status = match annotations.filter(|a| !a.is_empty()) {
        None => AnnotationStatus::None,
        Some(a) => match a.stopped_at {
            Some(u) => AnnotationStatus::Incomplete { type_code: u.type_code },
            None => AnnotationStatus::Complete,
        },
    };
    Ok(Rendered { pgn: out, annotations: status })
}
