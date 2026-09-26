//! The database formats as the bridge reads them: [`Store`] over a format's
//! database and [`Head`], `cbformat`'s, over its header records, for search, sort,
//! suggestions, the game list and the position index, and [`with_store`]
//! for a database of any format, a [`cbformat::view::Base`].
//!
//! The formats differ where the bridge looks: 2CBH names an annotator as a
//! player and a guiding text's title as an entity, while the classic format
//! keeps annotators in a table of their own and a text's titles in the text's
//! own `.cbg` record. A PGN file holds games only, with annotators apart from
//! players, and its games are served as the file writes them.

use std::ops::Range;

pub use cbformat::game::Head;
use cbformat::game::{Player, Start, Tournament};
use cbformat::pgn::{self, Options, Rendered};
use cbformat::pgnfile::lex::Lexer;
use cbformat::pgnfile::line::{LineEnd, main_line};
use cbformat::replay::{self, TreeVisitor};
use cbformat::v2;
use cbformat::{Error, Result, cbh, pgnfile};
use chesscore::{Board, Move};

/// The largest move or annotation record, content or spare area, served as
/// PGN. The largest record of any kind in a Mega Database is about 1.2 MB, a
/// guiding text; a record near the reader's 64 MiB limit would take gigabytes
/// to render.
pub const MAX_GAME_BYTES: usize = 2 << 20;
/// The longest entity record read for a name, in bytes. Real names are a few
/// dozen bytes; a longer record, which only a damaged or hostile file holds,
/// reads as an empty name, and none is ever read whole.
pub const MAX_NAME_RECORD: usize = 4 << 10;

/// The names a table holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Players,
    Tournaments,
    /// Annotators, and the authors of guiding texts and analyses: players in
    /// 2CBH, a table of their own in the classic format.
    Annotators,
    /// Titles of guiding texts and analyses.
    Titles,
}

/// A database of one format.
pub trait Store: Sync {
    type Head: Head;
    /// Bytes of one header record.
    const HEAD_BYTES: usize;
    /// Whether annotators are players: then [`Kind::Annotators`] reads as
    /// [`Kind::Players`], and shares its table, ranks and identities.
    const ANNOTATORS_ARE_PLAYERS: bool;
    /// Whether a title's key is its record's number (the classic format), so
    /// that titles are found by reading the headers, rather than an entity id.
    const TITLES_BY_RECORD: bool;

    fn record_count(&self) -> u32;
    /// Reads the header records from `first` into `buf`, [`Self::HEAD_BYTES`]
    /// each, as many as it holds; how many it read.
    fn read_records(&self, first: u32, buf: &mut [u8]) -> Result<u32>;
    /// The header `id` from its bytes, as [`Store::read_records`] reads them.
    fn head(id: u32, bytes: &[u8]) -> Self::Head;
    fn record(&self, id: u32) -> Result<Self::Head>;
    /// Headers `first..=last`, clamped to the database and to a batch.
    fn records(&self, first: u32, last: u32) -> Result<Vec<Self::Head>>;
    /// How many ids a kind's table has; 0 for titles found by record.
    fn name_count(&self, kind: Kind) -> u64;
    fn player(&self, id: i64) -> Result<Option<Player>>;
    fn tournament(&self, id: i64) -> Result<Option<Tournament>>;
    /// The players from `ids.start` on, as [`Store::player`] reads them one by
    /// one, handed to `each` in id order and never past `ids.end`; how many.
    /// At least one of a range that is not empty: a format that can reads the
    /// records of many consecutive players into `buf` at once.
    fn read_players<E: From<Error>>(
        &self,
        ids: Range<i64>,
        _buf: &mut [u8],
        each: &mut impl FnMut(Option<Player>) -> std::result::Result<(), E>,
    ) -> std::result::Result<u64, E> {
        if ids.is_empty() {
            return Ok(0);
        }
        each(self.player(ids.start)?)?;
        Ok(1)
    }
    /// [`Store::read_players`] for tournaments, as [`Store::tournament`] reads
    /// them.
    fn read_tournaments<E: From<Error>>(
        &self,
        ids: Range<i64>,
        _buf: &mut [u8],
        each: &mut impl FnMut(Option<Tournament>) -> std::result::Result<(), E>,
    ) -> std::result::Result<u64, E> {
        if ids.is_empty() {
            return Ok(0);
        }
        each(self.tournament(ids.start)?)?;
        Ok(1)
    }
    fn annotator(&self, id: i64) -> Result<Option<String>>;
    /// The title whose key is `key` ([`Head::other`]).
    fn title(&self, key: i64) -> Result<Option<String>>;
    /// Game `r` as PGN, refusing a move or annotation record larger than
    /// [`MAX_GAME_BYTES`] before it is read.
    fn render(&self, r: &Self::Head, options: &Options) -> Result<Rendered>;
    /// The first `plies` plies of game `r`'s main line in SAN, as
    /// [`Store::render`] writes them, separated by single spaces (#81). The
    /// move record is read into `buf`, whose capacity bounds it as
    /// [`MAX_GAME_BYTES`] does. `None` when the game does not start from the
    /// standard position or its moves cannot be decoded; the line ends at a
    /// null move and before damage. Only a failed read is an error.
    fn main_line(&self, r: &Self::Head, plies: u8, buf: &mut Vec<u8>) -> Result<Option<String>>;
}

impl Store for v2::Database {
    type Head = v2::Record;
    const HEAD_BYTES: usize = v2::HEADER_RECORD_SIZE;
    const ANNOTATORS_ARE_PLAYERS: bool = true;
    const TITLES_BY_RECORD: bool = false;

    fn record_count(&self) -> u32 {
        v2::Database::record_count(self)
    }
    fn read_records(&self, first: u32, buf: &mut [u8]) -> Result<u32> {
        v2::Database::read_records(self, first, buf)
    }
    fn head(id: u32, bytes: &[u8]) -> v2::Record {
        v2::Record::from_bytes(id, bytes.try_into().expect("a whole header record"))
    }
    fn record(&self, id: u32) -> Result<v2::Record> {
        v2::Database::record(self, id)
    }
    fn records(&self, first: u32, last: u32) -> Result<Vec<v2::Record>> {
        v2::Database::records(self, first, last)
    }
    fn name_count(&self, kind: Kind) -> u64 {
        let typ = match kind {
            Kind::Players | Kind::Annotators => v2::PLAYER,
            Kind::Tournaments => v2::TOURNAMENT,
            Kind::Titles => v2::GAME_TAG,
        };
        u64::try_from(self.entities().stored_count(typ)).unwrap_or(u64::MAX)
    }
    fn player(&self, id: i64) -> Result<Option<Player>> {
        self.entities().player_within(id, MAX_NAME_RECORD)
    }
    fn tournament(&self, id: i64) -> Result<Option<Tournament>> {
        self.entities().tournament_within(id, MAX_NAME_RECORD)
    }
    fn read_players<E: From<Error>>(
        &self,
        ids: Range<i64>,
        buf: &mut [u8],
        each: &mut impl FnMut(Option<Player>) -> std::result::Result<(), E>,
    ) -> std::result::Result<u64, E> {
        self.entities().read_players_within(ids, buf, MAX_NAME_RECORD, each)
    }
    fn read_tournaments<E: From<Error>>(
        &self,
        ids: Range<i64>,
        buf: &mut [u8],
        each: &mut impl FnMut(Option<Tournament>) -> std::result::Result<(), E>,
    ) -> std::result::Result<u64, E> {
        self.entities().read_tournaments_within(ids, buf, MAX_NAME_RECORD, each)
    }
    fn annotator(&self, id: i64) -> Result<Option<String>> {
        Ok(self.player(id)?.map(|p| p.pgn()))
    }
    fn title(&self, key: i64) -> Result<Option<String>> {
        self.entities().title_within(key, MAX_NAME_RECORD)
    }
    fn render(&self, r: &v2::Record, options: &Options) -> Result<Rendered> {
        let data = self.moves_of_within(r, MAX_GAME_BYTES)?;
        let annotations = self.annotations_of_within(r, MAX_GAME_BYTES)?;
        pgn::game_from(self, r, &data.moves()?, annotations.as_ref(), options)
    }
    fn main_line(&self, r: &v2::Record, plies: u8, buf: &mut Vec<u8>) -> Result<Option<String>> {
        let data = match self.read_moves_into(r, MAX_GAME_BYTES, buf) {
            Ok(data) => data,
            Err(e) if failed_read(&e) => return Err(e),
            Err(_) => return Ok(None),
        };
        let Ok(moves) = data.prefix_moves() else { return Ok(None) };
        if moves.is_chess960() || !matches!(moves.start(), Ok(Start::Standard)) {
            return Ok(None);
        }
        // The walk checks the tree's shape as well as each move.
        let mut prefix = LinePrefix::new(plies);
        let walked = replay::walk(&moves, &mut prefix);
        Ok(prefix.finish(walked.is_ok()))
    }
}

/// A classic entity id, which is at most 24 bits; `None` for one that is not.
fn classic_id(id: i64) -> Option<u32> {
    u32::try_from(id).ok()
}

impl Store for cbh::Database {
    type Head = cbh::Record;
    const HEAD_BYTES: usize = cbh::RECORD_SIZE;
    const ANNOTATORS_ARE_PLAYERS: bool = false;
    const TITLES_BY_RECORD: bool = true;

    fn record_count(&self) -> u32 {
        cbh::Database::record_count(self)
    }
    fn read_records(&self, first: u32, buf: &mut [u8]) -> Result<u32> {
        cbh::Database::read_records(self, first, buf)
    }
    fn head(id: u32, bytes: &[u8]) -> cbh::Record {
        cbh::Record::from_bytes(id, bytes.try_into().expect("a whole header record"))
    }
    fn record(&self, id: u32) -> Result<cbh::Record> {
        cbh::Database::record(self, id)
    }
    fn records(&self, first: u32, last: u32) -> Result<Vec<cbh::Record>> {
        cbh::Database::records(self, first, last)
    }
    fn name_count(&self, kind: Kind) -> u64 {
        let [players, tournaments, annotators, _] = self.entities().counts();
        match kind {
            Kind::Players => players,
            Kind::Tournaments => tournaments,
            Kind::Annotators => annotators,
            Kind::Titles => 0,
        }
    }
    fn player(&self, id: i64) -> Result<Option<Player>> {
        classic_id(id).map_or(Ok(None), |id| self.entities().player(id))
    }
    fn tournament(&self, id: i64) -> Result<Option<Tournament>> {
        classic_id(id).map_or(Ok(None), |id| self.entities().tournament(id))
    }
    fn annotator(&self, id: i64) -> Result<Option<String>> {
        classic_id(id).map_or(Ok(None), |id| self.entities().annotator(id))
    }
    fn title(&self, key: i64) -> Result<Option<String>> {
        let Some(id) = classic_id(key).filter(|&id| id >= 1 && id <= self.record_count()) else { return Ok(None) };
        Ok(Some(self.text_title(&self.record(id)?, MAX_NAME_RECORD)?))
    }
    fn render(&self, r: &cbh::Record, options: &Options) -> Result<Rendered> {
        let data = self.moves_of_within(r, MAX_GAME_BYTES)?;
        let annotations = self.annotations_of_within(r, MAX_GAME_BYTES)?;
        pgn::classic_game_from(self, r, &data.moves()?, annotations.as_ref(), options)
    }
    fn main_line(&self, r: &cbh::Record, plies: u8, buf: &mut Vec<u8>) -> Result<Option<String>> {
        let data = match self.read_moves_into(r, MAX_GAME_BYTES, buf) {
            Ok(data) => data,
            Err(e) if failed_read(&e) => return Err(e),
            Err(_) => return Ok(None),
        };
        let Ok(moves) = data.moves() else { return Ok(None) };
        if moves.is_chess960() || !matches!(moves.start(), Ok(Start::Standard)) {
            return Ok(None);
        }
        // The compact encoding names a move by the position it is played in,
        // so the tree is walked; the main line comes first in it.
        let mut prefix = LinePrefix::new(plies);
        let walked = cbh::walk(&moves, &mut prefix);
        Ok(prefix.finish(walked.is_ok()))
    }
}

/// Whether reading a game's moves failed as a read, rather than finding a
/// record cut short. A record a file ends inside of stays unreadable while the
/// file stays as it is; a file that changed meanwhile is reported by the
/// generation check after the window's lines are read.
fn failed_read(e: &Error) -> bool {
    matches!(e, Error::Io(_, io) if io.kind() != std::io::ErrorKind::UnexpectedEof)
}

/// The start of a game's main line in SAN, read by a tree walk that ends as
/// soon as the prefix is complete, so a long game costs only its prefix.
struct LinePrefix {
    plies: u8,
    text: String,
    read: u8,
    /// A main-line move announced and not yet played: it counts once it is,
    /// as an illegal move is reported before it is found to be one.
    pending: Option<(Board, Move)>,
    /// The prefix is complete: at `plies`, at a null move, or at the main
    /// line's end.
    done: bool,
}

impl LinePrefix {
    fn new(plies: u8) -> Self {
        LinePrefix { plies, text: String::new(), read: 0, pending: None, done: false }
    }

    /// The line after a walk: the moves before any damage, but `None` when
    /// damage came before the first move, which is no line at all.
    fn finish(self, walked: bool) -> Option<String> {
        (walked || self.read > 0).then_some(self.text)
    }
}

impl TreeVisitor for LinePrefix {
    fn play(&mut self, before: &Board, mv: Option<Move>, main_line: bool) {
        if self.done {
            return;
        }
        match mv.filter(|_| main_line) {
            Some(mv) => self.pending = Some((before.clone(), mv)),
            None => self.done = true,
        }
    }

    fn played(&mut self, _after: &Board) {
        if let Some((before, mv)) = self.pending.take() {
            if !self.text.is_empty() {
                self.text.push(' ');
            }
            self.text.push_str(&pgn::san(&before, mv));
            self.read += 1;
            self.done = self.read >= self.plies;
        }
    }

    /// The main line ended: the walk is in its variations now.
    fn resume(&mut self) {
        self.done = true;
    }

    fn stopped(&self) -> bool {
        self.done
    }
}

impl Store for pgnfile::Database {
    type Head = pgnfile::Record;
    const HEAD_BYTES: usize = pgnfile::RECORD_SIZE;
    const ANNOTATORS_ARE_PLAYERS: bool = false;
    const TITLES_BY_RECORD: bool = false;

    fn record_count(&self) -> u32 {
        pgnfile::Database::record_count(self)
    }
    fn read_records(&self, first: u32, buf: &mut [u8]) -> Result<u32> {
        pgnfile::Database::read_records(self, first, buf)
    }
    fn head(id: u32, bytes: &[u8]) -> pgnfile::Record {
        pgnfile::Record::from_bytes(id, bytes.try_into().expect("a whole header record"))
    }
    fn record(&self, id: u32) -> Result<pgnfile::Record> {
        pgnfile::Database::record(self, id)
    }
    fn records(&self, first: u32, last: u32) -> Result<Vec<pgnfile::Record>> {
        pgnfile::Database::records(self, first, last)
    }
    fn name_count(&self, kind: Kind) -> u64 {
        u64::from(match kind {
            Kind::Players => self.players(),
            Kind::Tournaments => self.tournaments(),
            Kind::Annotators => self.annotators(),
            Kind::Titles => 0,
        })
    }
    fn player(&self, id: i64) -> Result<Option<Player>> {
        self.player_within(id, MAX_NAME_RECORD)
    }
    fn tournament(&self, id: i64) -> Result<Option<Tournament>> {
        self.tournament_within(id, MAX_NAME_RECORD)
    }
    fn annotator(&self, id: i64) -> Result<Option<String>> {
        self.annotator_within(id, MAX_NAME_RECORD)
    }
    fn title(&self, _: i64) -> Result<Option<String>> {
        Ok(None)
    }
    /// The game as the file writes it: its comments are all it has, so the
    /// preferred languages and the full form change nothing.
    fn render(&self, r: &pgnfile::Record, _: &Options) -> Result<Rendered> {
        Ok(Rendered { pgn: self.text(r, MAX_GAME_BYTES)?, annotations: pgn::AnnotationStatus::Complete })
    }
    /// The main line as the text writes it, played from the standard
    /// position ([`pgnfile::line`]), and read only until the prefix is
    /// complete.
    fn main_line(&self, r: &pgnfile::Record, plies: u8, buf: &mut Vec<u8>) -> Result<Option<String>> {
        if r.is_chess960() || r.is_other_variant() {
            return Ok(None);
        }
        match self.bytes_into(r, MAX_GAME_BYTES, buf) {
            Ok(()) => {}
            Err(e) if failed_read(&e) => return Err(e),
            Err(_) => return Ok(None),
        }
        let standard = Board::startpos().hash();
        let (mut text, mut read, mut started, mut set_up) = (String::new(), 0u8, false, false);
        let end = main_line(buf, &mut Lexer::new(), &mut |board, mv| {
            if !std::mem::replace(&mut started, true) && board.hash() != standard {
                set_up = true;
                return false;
            }
            match mv.filter(|_| read < plies) {
                Some(mv) => {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(&pgn::san(board, mv));
                    read += 1;
                    read < plies
                }
                None => false,
            }
        });
        Ok(match end {
            _ if set_up => None,
            // Damage before the first move is no line at all.
            LineEnd::BadStart => None,
            LineEnd::Unplayable(_) if read == 0 => None,
            _ => Some(text),
        })
    }
}

/// Runs `$body` with `$db` bound to the [`Store`] a borrowed
/// [`cbformat::view::Base`] holds: the database of any format is `view`'s,
/// and the bridge adds only the dispatch to its own trait (#66).
macro_rules! with_store {
    ($base:expr, $db:ident => $body:expr) => {
        match $base {
            ::cbformat::view::Base::TwoCbh($db) => $body,
            ::cbformat::view::Base::Cbh($db) => $body,
            ::cbformat::view::Base::Pgn($db) => $body,
        }
    };
}
pub(crate) use with_store;
