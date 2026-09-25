//! PGN of classic (`.cbh`) games, through the same writer as 2CBH ones.

use super::{Options, Rendered, Tags, finish, write_tree};
use crate::cbh::{self, Database, GameMoves, Record};
use crate::game::{GameAnnotations, Player, RecordKind, Start};
use crate::replay::start_board;
use crate::view::PositionOrder;
use crate::{Error, Result};

/// A classic game as PGN, as [`super::game`] writes a 2CBH one.
pub fn classic_game(db: &Database, id: u32) -> Result<String> {
    classic_game_with(db, id, &Options::default()).map(|r| r.pgn)
}

/// [`classic_game`] with `options`, and how much of the annotations it holds.
pub fn classic_game_with(db: &Database, id: u32, options: &Options) -> Result<Rendered> {
    let r = db.record(id)?;
    if r.kind() != RecordKind::Game {
        return Err(Error::Format(format!("record {id} is not a game")));
    }
    let data = db.moves_of(&r)?;
    let annotations = db.annotations_of(&r)?;
    classic_game_from(db, &r, &data.moves()?, annotations.as_ref(), options)
}

/// [`classic_game_with`] for a record, move record and annotations already
/// read, as from a [`crate::cbh::Batch`]; entities are read from `db`.
/// `annotations` is `None` when the database has no `.cba` file.
pub fn classic_game_from(
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
    let name = |pid| -> Result<Option<Player>> { e.player(pid) };
    // The position the moves are played from: a set-up game gets the castling
    // rights its castling moves use, so that the PGN replays.
    let start = cbh::start_as_played(moves)?;
    let tags = Tags {
        tournament: e.tournament(r.tournament())?,
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
    let text = write_tree(|tree| cbh::walk(moves, tree), annotations, PositionOrder::Stored, options)?;
    Ok(finish(&tags, &text, annotations))
}
