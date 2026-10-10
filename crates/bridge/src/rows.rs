//! The row of `GET /v1/databases/{id}/games` (`docs/api.md`): the one form in
//! which every answer names a game. The game list and the explorer's notable
//! games both build it here, so that a field added to a row reaches every
//! answer that names a game (#144).

use std::collections::HashMap;

use cbformat::game::{Eco, ROUND_TEXT_BYTES, RecordKind, round_text};

use crate::json::Obj;
use crate::store::{Head, MAX_GAME_BYTES, Store};

/// The longest text, in characters, a list row carries in one field. Longer
/// names are cut and end with `…`; the game's PGN has them in full. With 500
/// rows this bounds a window to a few megabytes, whatever an entity holds.
pub const MAX_FIELD_CHARS: usize = 200;
/// An upper bound for one list row in JSON: nine text fields of at most
/// [`MAX_FIELD_CHARS`] characters, each character at most six bytes escaped,
/// plus the keys and numbers.
pub(crate) const MAX_ROW_BYTES: usize = 9 * MAX_FIELD_CHARS * 6 + 512;
/// The move buffer of a window with lines: one move record at a time, within
/// [`MAX_GAME_BYTES`] and its frame.
pub(crate) const LINE_BUFFER_BYTES: usize = MAX_GAME_BYTES + 64;

pub(crate) fn clip(text: String) -> String {
    match text.char_indices().nth(MAX_FIELD_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

/// The `line` a window's game rows carry, and the buffer their move records
/// are read into, one at a time.
pub(crate) struct Lines {
    plies: u8,
    buf: Vec<u8>,
}

impl Lines {
    /// `Some(None)` for a window without lines; `None` when the buffer
    /// cannot be had.
    pub(crate) fn new(plies: Option<u8>) -> Option<Option<Lines>> {
        let Some(plies) = plies else { return Some(None) };
        let mut buf = Vec::new();
        buf.try_reserve_exact(LINE_BUFFER_BYTES).ok()?;
        Some(Some(Lines { plies, buf }))
    }
}

/// Entity names for one window, each looked up once: rows of a tournament
/// share their players and event, and one entity can be up to a megabyte.
pub(crate) struct Names<'a, S: Store> {
    db: &'a S,
    players: HashMap<i64, String>,
    tournaments: HashMap<i64, (String, String)>,
    /// Where annotators are not players.
    annotators: HashMap<i64, String>,
    titles: HashMap<i64, String>,
}

impl<'a, S: Store> Names<'a, S> {
    pub(crate) fn new(db: &'a S) -> Self {
        Names {
            db,
            players: HashMap::new(),
            tournaments: HashMap::new(),
            annotators: HashMap::new(),
            titles: HashMap::new(),
        }
    }

    pub(crate) fn player(&mut self, id: i64) -> cbformat::Result<String> {
        if let Some(name) = self.players.get(&id) {
            return Ok(name.clone());
        }
        let name = clip(self.db.player(id)?.map(|p| p.pgn()).unwrap_or_default());
        self.players.insert(id, name.clone());
        Ok(name)
    }

    /// An annotator or author: a player where annotators are players.
    pub(crate) fn annotator(&mut self, id: i64) -> cbformat::Result<String> {
        if S::ANNOTATORS_ARE_PLAYERS {
            return self.player(id);
        }
        if let Some(name) = self.annotators.get(&id) {
            return Ok(name.clone());
        }
        let name = clip(self.db.annotator(id)?.unwrap_or_default());
        self.annotators.insert(id, name.clone());
        Ok(name)
    }

    /// The tournament's title and place.
    pub(crate) fn tournament(&mut self, id: i64) -> cbformat::Result<(String, String)> {
        if let Some(t) = self.tournaments.get(&id) {
            return Ok(t.clone());
        }
        let t = self.db.tournament(id)?.map_or_else(Default::default, |t| (clip(t.title), clip(t.place)));
        self.tournaments.insert(id, t.clone());
        Ok(t)
    }

    /// A guiding text's or an analysis's title, by its key ([`Head::other`]).
    pub(crate) fn title(&mut self, key: i64) -> cbformat::Result<String> {
        if let Some(t) = self.titles.get(&key) {
            return Ok(t.clone());
        }
        let t = clip(self.db.title(key)?.unwrap_or_default());
        self.titles.insert(key, t.clone());
        Ok(t)
    }
}

/// One list row, a game's `line` when the window has `lines`, and the ply
/// at which a fragment search found it, `matched` (#272).
pub(crate) fn row<S: Store>(
    names: &mut Names<'_, S>,
    lines: &mut Option<Lines>,
    r: &S::Head,
    matched: Option<u32>,
) -> cbformat::Result<String> {
    let row = row_obj(names, r)?;
    let row = match lines {
        Some(lines) if matches!(r.kind(), RecordKind::Game) => {
            match names.db.main_line(r, lines.plies, &mut lines.buf)? {
                Some(line) => row.str("line", &line),
                None => row.raw("line", "null"),
            }
        }
        _ => row,
    };
    Ok(match matched {
        Some(ply) => row.raw("match", &Obj::new().num("ply", i64::from(ply)).done()),
        None => row,
    }
    .done())
}

/// One list row, open for the fields an answer adds to it. Guiding texts and
/// analyses have header layouts of their own (only the first eight bytes are
/// shared with games): their row carries the title in `event` and the author
/// in `annotator`, and no game fields.
pub(crate) fn row_obj<S: Store>(names: &mut Names<'_, S>, r: &S::Head) -> cbformat::Result<Obj> {
    let base = Obj::new().num("number", r.id());
    let other = |base: Obj, kind: &str, title: String, author: String| {
        base.str("kind", kind)
            .str("white", "")
            .num("whiteElo", 0)
            .str("black", "")
            .num("blackElo", 0)
            .str("result", "*")
            .num("moves", 0)
            .str("eco", "")
            .str("event", &title)
            .str("site", "")
            .str("date", "????.??.??")
            .str("round", "")
            .str("annotator", &author)
            .raw("flags", &Obj::new().bool("deleted", r.is_deleted()).bool("chess960", false).done())
    };
    match r.kind() {
        kind @ (RecordKind::Text | RecordKind::Analysis) => {
            let (title, author) = r.other().unwrap_or((-1, -1));
            let kind = if kind == RecordKind::Text { "text" } else { "analysis" };
            Ok(other(base, kind, names.title(title)?, names.annotator(author)?))
        }
        RecordKind::Unknown(_) => Ok(other(base, "unknown", String::new(), String::new())),
        RecordKind::Game => {
            let (event, site) = names.tournament(r.tournament())?;
            // The fields as search matches them and PGN writes them (#68).
            let (n, s) = r.round();
            let mut buf = [0; ROUND_TEXT_BYTES];
            let round = round_text(n, s, &mut buf);
            let flags =
                Obj::new().bool("deleted", r.is_deleted()).bool("chess960", matches!(r.eco(), Eco::Chess960(_))).done();
            let (white_elo, black_elo) = r.elo();
            Ok(base
                .str("kind", "game")
                .str("white", &names.player(r.white())?)
                .num("whiteElo", white_elo.max(0))
                .str("black", &names.player(r.black())?)
                .num("blackElo", black_elo.max(0))
                .str("result", r.result().pgn())
                .num("moves", r.move_count().max(0))
                .str("eco", &r.eco().pgn().unwrap_or_default())
                .str("event", &event)
                .str("site", &site)
                .str("date", &r.played_date().pgn())
                .str("round", round)
                .str("annotator", &names.annotator(r.annotator())?)
                .raw("flags", &flags))
        }
    }
}
