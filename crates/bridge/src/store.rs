//! The two database formats as the bridge reads them: [`Store`] over a
//! format's database and [`Head`] over its header records, for search, sort,
//! suggestions, the game list and the position index, and [`Any`] for a
//! database of either format.
//!
//! The formats differ where the bridge looks: 2CBH names an annotator as a
//! player and a guiding text's title as an entity, while the classic format
//! keeps annotators in a table of their own and a text's titles in the text's
//! own `.cbg` record.

use cbformat::pgn::{self, Options, Rendered};
use cbformat::replay::{self, TreeVisitor};
use cbformat::v2::{self, Date, Eco, GameResult, Player, RecordKind, Start, Tournament};
use cbformat::view::Base;
use cbformat::{Error, Result, cbh};
use chesscore::{Board, Move};

use crate::api::MAX_GAME_BYTES;
use crate::search::MAX_NAME_RECORD;

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

/// A header record: a game, a guiding text or an analysis. Entity ids are
/// those of the database's tables ([`Kind`]).
pub trait Head: Copy + Send + Sync {
    fn id(&self) -> u32;
    fn kind(&self) -> RecordKind;
    fn is_deleted(&self) -> bool;
    fn white(&self) -> i64;
    fn black(&self) -> i64;
    fn tournament(&self) -> i64;
    /// A game's annotator, among [`Kind::Annotators`].
    fn annotator(&self) -> i64;
    /// For a record that is not a game, its title's key ([`Store::title`])
    /// and its author, among [`Kind::Annotators`]; -1 where it has none.
    /// Guiding texts and analyses have header layouts of their own.
    fn other(&self) -> Option<(i64, i64)>;
    fn result(&self) -> GameResult;
    fn eco(&self) -> Eco;
    fn played_date(&self) -> Date;
    /// Round and sub-round; 0 or less when there is none.
    fn round(&self) -> (i32, i32);
    /// White's and black's ratings; 0 or less when unknown.
    fn elo(&self) -> (i32, i32);
    fn move_count(&self) -> i32;
    /// The record as stored.
    fn bytes(&self) -> &[u8];
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

impl Head for v2::Record {
    fn id(&self) -> u32 {
        v2::Record::id(self)
    }
    fn kind(&self) -> RecordKind {
        v2::Record::kind(self)
    }
    fn is_deleted(&self) -> bool {
        v2::Record::is_deleted(self)
    }
    fn white(&self) -> i64 {
        v2::Record::white(self)
    }
    fn black(&self) -> i64 {
        v2::Record::black(self)
    }
    fn tournament(&self) -> i64 {
        v2::Record::tournament(self)
    }
    fn annotator(&self) -> i64 {
        v2::Record::annotator(self)
    }
    fn other(&self) -> Option<(i64, i64)> {
        match v2::Record::kind(self) {
            RecordKind::Game => None,
            RecordKind::Text => Some((self.text_title(), self.text_author())),
            RecordKind::Analysis => Some((self.analysis_title(), self.analysis_author())),
            RecordKind::Unknown(_) => Some((-1, -1)),
        }
    }
    fn result(&self) -> GameResult {
        v2::Record::result(self)
    }
    fn eco(&self) -> Eco {
        v2::Record::eco(self)
    }
    fn played_date(&self) -> Date {
        v2::Record::played_date(self)
    }
    fn round(&self) -> (i32, i32) {
        (i32::from(v2::Record::round(self)), i32::from(self.subround()))
    }
    fn elo(&self) -> (i32, i32) {
        (i32::from(self.white_elo()), i32::from(self.black_elo()))
    }
    fn move_count(&self) -> i32 {
        i32::from(v2::Record::move_count(self))
    }
    fn bytes(&self) -> &[u8] {
        v2::Record::bytes(self)
    }
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
            Err(e @ Error::Io(..)) => return Err(e),
            Err(_) => return Ok(None),
        };
        let Ok(moves) = data.moves() else { return Ok(None) };
        if moves.is_chess960() || !matches!(moves.start(), Ok(Start::Standard)) {
            return Ok(None);
        }
        let mut board = Board::startpos();
        let mut line = SanLine::default();
        for word in moves.main_line().take(usize::from(plies)) {
            let before = board.clone();
            // A null move or damage ends the line.
            let Ok(Some(mv)) = replay::play(&mut board, word) else { break };
            line.push(&before, mv);
        }
        Ok(Some(line.text))
    }
}

impl Head for cbh::Record {
    fn id(&self) -> u32 {
        cbh::Record::id(self)
    }
    fn kind(&self) -> RecordKind {
        cbh::Record::kind(self)
    }
    fn is_deleted(&self) -> bool {
        cbh::Record::is_deleted(self)
    }
    fn white(&self) -> i64 {
        i64::from(cbh::Record::white(self))
    }
    fn black(&self) -> i64 {
        i64::from(cbh::Record::black(self))
    }
    fn tournament(&self) -> i64 {
        i64::from(cbh::Record::tournament(self))
    }
    fn annotator(&self) -> i64 {
        i64::from(cbh::Record::annotator(self))
    }
    /// A guiding text's title is in its `.cbg` record: its key is the text's
    /// number. The format has no analyses.
    fn other(&self) -> Option<(i64, i64)> {
        match cbh::Record::kind(self) {
            RecordKind::Game => None,
            RecordKind::Text => Some((i64::from(self.id()), i64::from(cbh::Record::annotator(self)))),
            _ => Some((-1, -1)),
        }
    }
    fn result(&self) -> GameResult {
        cbh::Record::result(self)
    }
    fn eco(&self) -> Eco {
        cbh::Record::eco(self)
    }
    fn played_date(&self) -> Date {
        cbh::Record::played_date(self)
    }
    fn round(&self) -> (i32, i32) {
        (i32::from(cbh::Record::round(self)), i32::from(self.subround()))
    }
    fn elo(&self) -> (i32, i32) {
        (i32::from(self.white_elo()), i32::from(self.black_elo()))
    }
    fn move_count(&self) -> i32 {
        i32::from(cbh::Record::move_count(self))
    }
    fn bytes(&self) -> &[u8] {
        cbh::Record::bytes(self)
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
            Err(e @ Error::Io(..)) => return Err(e),
            Err(_) => return Ok(None),
        };
        let Ok(moves) = data.moves() else { return Ok(None) };
        if moves.is_chess960() || !matches!(moves.start(), Ok(Start::Standard)) {
            return Ok(None);
        }
        // The compact encoding names a move by the position it is played in,
        // so the tree is walked; the main line comes first in it. Damage ends
        // the walk after the moves before it.
        let mut main = ClassicLine { plies, line: SanLine::default(), pending: None, done: false };
        let _ = cbh::walk(&moves, &mut main);
        Ok(Some(main.line.text))
    }
}

/// A main line's SAN, one space between moves.
#[derive(Default)]
struct SanLine {
    text: String,
    plies: u8,
}

impl SanLine {
    fn push(&mut self, before: &Board, mv: Move) {
        if !self.text.is_empty() {
            self.text.push(' ');
        }
        self.text.push_str(&pgn::san(before, mv));
        self.plies += 1;
    }
}

/// The main line of a classic game's tree walk, as far as `plies`.
struct ClassicLine {
    plies: u8,
    line: SanLine,
    /// A main-line move announced and not yet played: it counts once it is,
    /// as an illegal move is reported before it is found to be one.
    pending: Option<(Board, Move)>,
    /// The line ended: at `plies`, a null move, or the first move off it.
    done: bool,
}

impl TreeVisitor for ClassicLine {
    fn play(&mut self, before: &Board, mv: Option<Move>, main_line: bool) {
        if self.done {
            return;
        }
        match mv.filter(|_| main_line && self.line.plies < self.plies) {
            Some(mv) => self.pending = Some((before.clone(), mv)),
            None => self.done = true,
        }
    }

    fn played(&mut self, _after: &Board) {
        if let Some((before, mv)) = self.pending.take() {
            self.line.push(&before, mv);
        }
    }
}

/// A database of either format, borrowed.
#[derive(Clone, Copy)]
pub enum Any<'a> {
    TwoCbh(&'a v2::Database),
    Cbh(&'a cbh::Database),
}

impl Any<'_> {
    /// Number of records, including deleted games, texts and analyses.
    pub fn record_count(self) -> u32 {
        match self {
            Any::TwoCbh(db) => db.record_count(),
            Any::Cbh(db) => db.record_count(),
        }
    }
}

impl<'a> From<&'a v2::Database> for Any<'a> {
    fn from(db: &'a v2::Database) -> Self {
        Any::TwoCbh(db)
    }
}

impl<'a> From<&'a cbh::Database> for Any<'a> {
    fn from(db: &'a cbh::Database) -> Self {
        Any::Cbh(db)
    }
}

impl<'a> From<&'a Base> for Any<'a> {
    fn from(db: &'a Base) -> Self {
        match db {
            Base::TwoCbh(db) => Any::TwoCbh(db),
            Base::Cbh(db) => Any::Cbh(db),
        }
    }
}

impl<'a, T> From<&'a std::sync::Arc<T>> for Any<'a>
where
    &'a T: Into<Any<'a>>,
{
    fn from(db: &'a std::sync::Arc<T>) -> Self {
        (&**db).into()
    }
}

/// Runs `$body` with `$db` bound to the [`Store`] an [`Any`] holds.
macro_rules! with_store {
    ($any:expr, $db:ident => $body:expr) => {
        match ::std::convert::Into::<$crate::store::Any<'_>>::into($any) {
            $crate::store::Any::TwoCbh($db) => $body,
            $crate::store::Any::Cbh($db) => $body,
        }
    };
}
pub(crate) use with_store;
