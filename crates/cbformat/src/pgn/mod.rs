//! PGN output: standard algebraic notation, a game's full move tree and its
//! annotations.

mod classic;
mod commands;
mod comments;
mod san;
mod tree;

pub use classic::{classic_game, classic_game_from, classic_game_with};

pub use commands::language_code;
pub use san::san;

use chesscore::Board;

use crate::replay::{self, TreeStats, start_board};
use crate::v2::{
    Database, Date, Eco, GameAnnotations, GameMoves, GameResult, Player, Record, RecordKind, Start, Tournament,
    language,
};
use crate::view::PositionOrder;
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
    /// The full form (asavis/oschess-cb-bridge#42): every text in every
    /// language as its own comment led by `[%lang]`, and every annotation the
    /// reading form leaves out as a `[%cb…]` command. `docs/api.md` lists them.
    pub full: bool,
}

impl Options {
    /// Options preferring the ISO 639-1 languages `codes`, in order; codes
    /// ChessBase has no number for are passed over.
    pub fn with_languages<'a>(codes: impl IntoIterator<Item = &'a str>) -> Self {
        Options { languages: codes.into_iter().filter_map(language::from_iso).collect(), full: false }
    }
}

/// How much of a game's annotations the PGN holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnotationStatus {
    /// The game has no annotations, or the database has no annotation file.
    None,
    /// Every annotation was decoded. The reading form leaves out the types
    /// PGN has no form for (training, clocks and the like) by design; the
    /// full form keeps them as commands.
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
    write_tree(|tree| replay::walk(moves, tree), annotations, PositionOrder::Pgn, options)
}

/// The movetext of the tree `walk` plays, with its annotations numbered in
/// `order`.
fn write_tree(
    walk: impl FnOnce(&mut TreeBuilder) -> Result<TreeStats>,
    annotations: Option<&GameAnnotations>,
    order: PositionOrder,
    options: &Options,
) -> Result<String> {
    let mut tree = TreeBuilder::new();
    // walk() checks every move and the tree's shape, so a damaged record is an
    // error here exactly as it is in `cbtool verify`.
    let stats = walk(&mut tree)?;
    let mut out = String::with_capacity(tree.text_len());
    match annotations.filter(|a| !a.is_empty()) {
        Some(a) => {
            // An annotation past the last move follows the main line's last
            // move; in a game without moves it is damage, not something to
            // drop quietly.
            a.check_positions(stats.total_plies)?;
            let mut commentary = Commentary::new(a, order, options, stats.total_plies, &tree.main_line());
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
    let name = |pid| -> Result<Option<Player>> { e.player(pid) };
    let start = moves.start()?;
    let tags = Tags {
        tournament: t,
        date: r.played_date(),
        round: (i32::from(r.round()), i32::from(r.subround())),
        white: name(r.white())?,
        black: name(r.black())?,
        result: r.result(),
        elo: (i32::from(r.white_elo()), i32::from(r.black_elo())),
        eco: r.eco(),
        start: (start != Start::Standard).then(|| start_board(&start)).transpose()?,
        chess960: moves.is_chess960(),
    };
    let text = write_movetext(moves, annotations, options)?;
    Ok(finish(&tags, &text, annotations))
}

/// The tag roster of a game, from either format.
struct Tags {
    tournament: Option<Tournament>,
    date: Date,
    /// Round and sub-round, 0 when unknown.
    round: (i32, i32),
    white: Option<Player>,
    black: Option<Player>,
    result: GameResult,
    /// Ratings, 0 or less when unknown.
    elo: (i32, i32),
    eco: Eco,
    /// The start position, for a game not from the standard one.
    start: Option<Board>,
    chess960: bool,
}

/// The whole PGN record: tags, movetext and result, and how much of the
/// annotations it holds.
fn finish(tags: &Tags, text: &str, annotations: Option<&GameAnnotations>) -> Rendered {
    let t = tags.tournament.as_ref();
    let name = |p: &Option<Player>| p.as_ref().map(|p| p.pgn()).filter(|s| !s.is_empty()).unwrap_or_else(|| "?".into());
    let mut out = String::new();
    tag(&mut out, "Event", t.map(|t| t.title.as_str()).filter(|s| !s.is_empty()).unwrap_or("?"));
    tag(&mut out, "Site", t.map(|t| t.place.as_str()).filter(|s| !s.is_empty()).unwrap_or("?"));
    tag(&mut out, "Date", &tags.date.pgn());
    let round = match tags.round {
        (0, _) => "?".to_string(),
        (n, 0) => n.to_string(),
        (n, s) => format!("{n}({s})"),
    };
    tag(&mut out, "Round", &round);
    tag(&mut out, "White", &name(&tags.white));
    tag(&mut out, "Black", &name(&tags.black));
    tag(&mut out, "Result", tags.result.pgn());
    if tags.elo.0 > 0 {
        tag(&mut out, "WhiteElo", &tags.elo.0.to_string());
    }
    if tags.elo.1 > 0 {
        tag(&mut out, "BlackElo", &tags.elo.1.to_string());
    }
    if let Some(eco) = tags.eco.pgn() {
        tag(&mut out, "ECO", &eco);
    }
    if let Some(board) = &tags.start {
        if tags.chess960 {
            tag(&mut out, "Variant", "Chess960");
        }
        tag(&mut out, "SetUp", "1");
        tag(&mut out, "FEN", &format!("{board}"));
    }
    out.push('\n');
    if !text.is_empty() {
        out.push_str(text);
        out.push(' ');
    }
    out.push_str(tags.result.pgn());
    out.push('\n');
    let status = match annotations.filter(|a| !a.is_empty()) {
        None => AnnotationStatus::None,
        Some(a) => match a.stopped_at {
            Some(u) => AnnotationStatus::Incomplete { type_code: u.type_code },
            None => AnnotationStatus::Complete,
        },
    };
    Rendered { pgn: out, annotations: status }
}
