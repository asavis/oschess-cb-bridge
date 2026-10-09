//! A PGN file read as a database: its games found in one pass over the file
//! and kept in an index file beside the reader's other data, so that a game's
//! header is read at a position, as in the other formats, and the file is not
//! read again until it changes.
//!
//! [`build`] reads the PGN file and writes the index: a fixed-size [`Record`]
//! for each game, in the file's order, and the tables of the names the games
//! use. [`Database::open`] reads the index and serves each game's header,
//! names and text. A game's text is served as it is written in the file,
//! decoded as UTF-8, or in the computer's code page when it is not UTF-8.
//!
//! The index file (all numbers little-endian):
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 8 | `OSPGNIDX` |
//! | 8 | 4 | version, 3 |
//! | 12 | 4 | record size, 48 |
//! | 16 | 8 | the stamp the index was built for, which its user chooses |
//! | 24 | 8 | bytes of the PGN file read |
//! | 32 | 4 | games |
//! | 36 | 12 | players, tournaments and annotators |
//! | 48 | 8 | offset of the name table |
//! | 56 | 2 | the code page of text that is not UTF-8 |
//! | 64 | 48 × games | the records |
//!
//! The name table has an entry of 12 bytes for each name, the offset (8) and
//! length (4) of its UTF-8 text in the index file: every player, then each
//! tournament's title and place, then every annotator. The texts follow.

pub mod edit;
pub mod lex;
pub mod line;
pub mod scan;

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::bytes::Fields;
use crate::codepage::CodePage;
use crate::file::DbFile;
use crate::game::{Date, Eco, GameResult, Head, Player, RecordKind, TimeControl, Tournament};
use crate::recordfile::{RecordFile, over_limit};
use crate::{Error, Result};

use lex::Lexer;
use scan::{Game, ResultCode, Splitter, Tag, Variant};

/// Bytes of one [`Record`].
pub const RECORD_SIZE: usize = 48;
const HEADER_SIZE: u64 = 64;
const MAGIC: &[u8; 8] = b"OSPGNIDX";
/// The index format's version, which an index must carry to be opened.
/// Raise it whenever the index's layout changes, or the text it yields for
/// the same file does, as the decoding of code page text did in #120, and the
/// time control a record keeps did in #268.
pub const VERSION: u32 = 3;
/// Bytes of one entry of the name table: where its text starts in the index
/// file (8 bytes), then its length (4).
const ENTRY_SIZE: u64 = 12;
const ENTRY_TEXT_AT: usize = 0;
const ENTRY_LEN_AT: usize = 8;
/// The longest name kept, in bytes of UTF-8; a longer one is cut.
pub const MAX_NAME: usize = lex::MAX_TAG_VALUE;
/// The name text a build may hold in memory. A file with more distinct names
/// is refused: real files have a few megabytes of them.
pub const MAX_NAME_BYTES: usize = 256 << 20;
/// Bytes of the PGN file read at a time while building.
const CHUNK: usize = 1 << 20;
/// How many comments left open a build ends before a game header they hold,
/// each time reading the rest of the file again.
const MAX_RESUMES: u32 = 64;
/// The longest game text read where the caller sets no lower limit, as the
/// other readers bound a record.
pub const MAX_TEXT: usize = 64 << 20;

const FLAG_CHESS960: u8 = 1;
const FLAG_OTHER_VARIANT: u8 = 2;
const FLAG_SETUP: u8 = 4;

// Where each field of the index header starts, for the index's writer and
// its reader; the table above gives their sizes.
const MAGIC_AT: usize = 0;
const VERSION_AT: usize = 8;
const RECORD_SIZE_AT: usize = 12;
const STAMP_AT: usize = 16;
/// Bytes of the PGN file read: its length when built.
const TEXT_LEN_AT: usize = 24;
const GAMES_AT: usize = 32;
const PLAYERS_AT: usize = 36;
const TOURNAMENTS_AT: usize = 40;
const ANNOTATORS_AT: usize = 44;
const NAME_TABLE_AT: usize = 48;
const PAGE_AT: usize = 56;

// Where each field of a [`Record`] starts, for the index's writer and its
// reader: where the game's text starts in the PGN file (8 bytes) and its
// length (4); the white and black players, the tournament and the annotator
// (4 each, ids in their tables stored one higher, 0 for none); the date (4);
// the round, sub-round, white's and black's ratings, the ECO code and the
// move count (2 each); the result, the flags and the time control's bits (1
// each, #268). The rest is zero.
const OFFSET_AT: usize = 0;
const LEN_AT: usize = 8;
const WHITE_AT: usize = 12;
const BLACK_AT: usize = 16;
const TOURNAMENT_AT: usize = 20;
const ANNOTATOR_AT: usize = 24;
const DATE_AT: usize = 28;
const ROUND_AT: usize = 32;
const SUBROUND_AT: usize = 34;
const WHITE_ELO_AT: usize = 36;
const BLACK_ELO_AT: usize = 38;
const ECO_AT: usize = 40;
const MOVES_AT: usize = 42;
const RESULT_AT: usize = 44;
const FLAGS_AT: usize = 45;
const TIME_CONTROL_AT: usize = 46;

/// The header of one game, as the index keeps it.
#[derive(Clone, Copy)]
pub struct Record {
    id: u32,
    b: [u8; RECORD_SIZE],
}

/// A PGN file holds games only, none deleted.
impl Head for Record {
    fn id(&self) -> u32 {
        Record::id(self)
    }
    fn kind(&self) -> RecordKind {
        RecordKind::Game
    }
    fn is_deleted(&self) -> bool {
        false
    }
    fn white(&self) -> i64 {
        Record::white(self)
    }
    fn black(&self) -> i64 {
        Record::black(self)
    }
    fn tournament(&self) -> i64 {
        Record::tournament(self)
    }
    fn annotator(&self) -> i64 {
        Record::annotator(self)
    }
    fn other(&self) -> Option<(i64, i64)> {
        None
    }
    fn result(&self) -> GameResult {
        Record::result(self)
    }
    fn eco(&self) -> Eco {
        Record::eco(self)
    }
    fn played_date(&self) -> Date {
        Record::played_date(self)
    }
    fn round(&self) -> (i32, i32) {
        let (round, sub) = Record::round(self);
        (i32::from(round), i32::from(sub))
    }
    fn elo(&self) -> (i32, i32) {
        let (white, black) = Record::elo(self);
        (i32::from(white), i32::from(black))
    }
    fn move_count(&self) -> i32 {
        i32::from(Record::move_count(self))
    }
    fn bytes(&self) -> &[u8] {
        Record::bytes(self)
    }
    fn time_control(&self) -> Option<TimeControl> {
        Some(Record::time_control(self))
    }
}

impl Record {
    /// The record numbered `id` from its bytes, as [`Database::read_records`]
    /// reads them.
    pub fn from_bytes(id: u32, b: &[u8; RECORD_SIZE]) -> Record {
        Record { id, b: *b }
    }
    pub fn id(&self) -> u32 {
        self.id
    }
    pub fn bytes(&self) -> &[u8] {
        &self.b
    }
    /// Where the game's text starts in the PGN file, and its length.
    pub fn offset(&self) -> u64 {
        self.b.le_u64::<OFFSET_AT>()
    }
    pub fn len(&self) -> u32 {
        self.b.le_u32::<LEN_AT>()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// A name's id in its table, stored one higher so that 0 is none: -1.
    fn name_id<const AT: usize>(&self) -> i64 {
        i64::from(self.b.le_u32::<AT>()) - 1
    }
    pub fn white(&self) -> i64 {
        self.name_id::<WHITE_AT>()
    }
    pub fn black(&self) -> i64 {
        self.name_id::<BLACK_AT>()
    }
    pub fn tournament(&self) -> i64 {
        self.name_id::<TOURNAMENT_AT>()
    }
    pub fn annotator(&self) -> i64 {
        self.name_id::<ANNOTATOR_AT>()
    }
    pub fn played_date(&self) -> Date {
        Date(self.b.le_i32::<DATE_AT>())
    }
    /// The time control its `TimeControl` tag gives ([`TimeControl::of_pgn`]).
    pub fn time_control(&self) -> TimeControl {
        TimeControl::of_kind(self.b[TIME_CONTROL_AT])
    }
    /// Round and sub-round; 0 when unknown.
    pub fn round(&self) -> (i16, i16) {
        (self.b.le_i16::<ROUND_AT>(), self.b.le_i16::<SUBROUND_AT>())
    }
    /// White's and black's ratings; 0 when unknown.
    pub fn elo(&self) -> (i16, i16) {
        (self.b.le_i16::<WHITE_ELO_AT>(), self.b.le_i16::<BLACK_ELO_AT>())
    }
    pub fn eco(&self) -> Eco {
        Eco::from_field(self.b.le_u16::<ECO_AT>())
    }
    /// Full moves of the main line, counted as written.
    pub fn move_count(&self) -> i16 {
        self.b.le_i16::<MOVES_AT>()
    }
    pub fn result(&self) -> GameResult {
        GameResult::from_field(self.b[RESULT_AT])
    }
    /// Whether the game is Chess960.
    pub fn is_chess960(&self) -> bool {
        self.b[FLAGS_AT] & FLAG_CHESS960 != 0
    }
    /// Whether the game is of a variant other than chess and Chess960.
    pub fn is_other_variant(&self) -> bool {
        self.b[FLAGS_AT] & FLAG_OTHER_VARIANT != 0
    }
    /// Whether the game starts from a position of its own (a `FEN` tag).
    pub fn has_setup(&self) -> bool {
        self.b[FLAGS_AT] & FLAG_SETUP != 0
    }
}

/// The distinct names of a build, each given an id in the order first seen.
#[derive(Default)]
struct Names<K> {
    ids: HashMap<K, u32>,
    list: Vec<K>,
}

impl<K: Clone + Eq + std::hash::Hash> Names<K> {
    /// The id of `key` stored one higher, 0 being none; `None` when the build
    /// cannot hold another name.
    fn intern(&mut self, key: K, bytes: usize, held: &mut usize) -> Option<u32> {
        if let Some(&id) = self.ids.get(&key) {
            return Some(id + 1);
        }
        let id = u32::try_from(self.list.len()).ok().filter(|&id| id < u32::MAX - 1)?;
        *held = held.checked_add(bytes).filter(|&h| h <= MAX_NAME_BYTES)?;
        self.ids.insert(key.clone(), id);
        self.list.push(key);
        Some(id + 1)
    }
}

/// `text` without surrounding blanks, cut to [`MAX_NAME`] bytes; empty when
/// PGN's way of saying unknown.
fn name(text: String) -> String {
    let text = text.trim();
    if scan::is_unknown(text) {
        return String::new();
    }
    let mut end = text.len().min(MAX_NAME);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// What a build keeps besides the records.
struct Building {
    page: CodePage,
    players: Names<String>,
    tournaments: Names<(String, String)>,
    annotators: Names<String>,
    held: usize,
}

impl Building {
    fn new(page: CodePage) -> Building {
        Building {
            page,
            players: Names::default(),
            tournaments: Names::default(),
            annotators: Names::default(),
            held: 0,
        }
    }

    /// A tag's value as a name, in the game's encoding: UTF-8 when all of the
    /// game is, as [`Database::text`] reads it, else the code page.
    fn text(&self, game: &Game, tag: Tag) -> String {
        let decode = |v: &[u8]| if game.utf8 { String::from_utf8_lossy(v).into_owned() } else { self.page.decode(v) };
        game.tags.get(tag).map(|v| name(decode(v))).unwrap_or_default()
    }

    fn person(&mut self, game: &Game, tag: Tag, annotator: bool) -> Result<u32> {
        let text = self.text(game, tag);
        if text.is_empty() {
            return Ok(0);
        }
        let bytes = text.len();
        let table = if annotator { &mut self.annotators } else { &mut self.players };
        table.intern(text, bytes, &mut self.held).ok_or_else(too_many_names)
    }

    fn record(&mut self, game: &Game) -> Result<[u8; RECORD_SIZE]> {
        let mut b = [0u8; RECORD_SIZE];
        let length = u32::try_from(game.end - game.start).unwrap_or(u32::MAX);
        b.put::<OFFSET_AT, 8>(game.start.to_le_bytes());
        b.put::<LEN_AT, 4>(length.to_le_bytes());
        let white = self.person(game, Tag::White, false)?;
        let black = self.person(game, Tag::Black, false)?;
        let (event, site) = (self.text(game, Tag::Event), self.text(game, Tag::Site));
        let tournament = if event.is_empty() && site.is_empty() {
            0
        } else {
            let bytes = event.len() + site.len();
            self.tournaments.intern((event, site), bytes, &mut self.held).ok_or_else(too_many_names)?
        };
        let annotator = self.person(game, Tag::Annotator, true)?;
        b.put::<WHITE_AT, 4>(white.to_le_bytes());
        b.put::<BLACK_AT, 4>(black.to_le_bytes());
        b.put::<TOURNAMENT_AT, 4>(tournament.to_le_bytes());
        b.put::<ANNOTATOR_AT, 4>(annotator.to_le_bytes());
        let tag = |t: Tag| game.tags.get(t).unwrap_or_default();
        b.put::<DATE_AT, 4>(scan::date(tag(Tag::Date)).0.to_le_bytes());
        let (round, sub) = scan::round(tag(Tag::Round));
        b.put::<ROUND_AT, 2>(round.to_le_bytes());
        b.put::<SUBROUND_AT, 2>(sub.to_le_bytes());
        b.put::<WHITE_ELO_AT, 2>(scan::elo(tag(Tag::WhiteElo)).to_le_bytes());
        b.put::<BLACK_ELO_AT, 2>(scan::elo(tag(Tag::BlackElo)).to_le_bytes());
        let variant = scan::variant(tag(Tag::Variant));
        let eco = if variant == Variant::Chess960 { scan::CHESS960_ECO } else { scan::eco(tag(Tag::Eco)) };
        b.put::<ECO_AT, 2>(eco.to_le_bytes());
        // Full moves are the move numbers the plies cover, as ChessBase counts
        // them: a game set up with black to move starts with half a move.
        let black_first = tag(Tag::Fen).split(|&c| c == b' ').filter(|f| !f.is_empty()).nth(1) == Some(b"b");
        let moves = if black_first && game.plies > 0 { game.plies / 2 + 1 } else { game.plies.div_ceil(2) };
        b.put::<MOVES_AT, 2>((moves.min(i16::MAX as u32) as u16).to_le_bytes());
        let result = ResultCode::of_tag(tag(Tag::Result)).or(game.termination).unwrap_or(ResultCode::UNKNOWN);
        b.put::<RESULT_AT, 1>([result.0]);
        let mut flags = match variant {
            Variant::Standard => 0,
            Variant::Chess960 => FLAG_CHESS960,
            Variant::Other => FLAG_OTHER_VARIANT,
        };
        if !tag(Tag::Fen).trim_ascii().is_empty() {
            flags |= FLAG_SETUP;
        }
        b.put::<FLAGS_AT, 1>([flags]);
        b.put::<TIME_CONTROL_AT, 1>([TimeControl::of_pgn(tag(Tag::TimeControl)).bits()]);
        Ok(b)
    }
}

fn too_many_names() -> Error {
    Error::Format(format!("the file names more than {MAX_NAME_BYTES} bytes of players, events and annotators"))
}

fn io(path: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    move |e| Error::Io(path.to_path_buf(), e)
}

/// The path a build writes before it is complete: `index` with `.partial`
/// added.
pub fn partial_path(index: &Path) -> PathBuf {
    let mut s = index.as_os_str().to_owned();
    s.push(".partial");
    PathBuf::from(s)
}

/// Reads the PGN file `pgn` and writes its index to `index`, stamped `stamp`,
/// decoding text that is not UTF-8 in `page`. The index is written beside
/// `index` first and takes its place only when complete. `read` is told the
/// bytes read so far after each part of the file, and the build stops with an
/// error when it answers `false`. Returns the number of games.
pub fn build(pgn: &Path, index: &Path, stamp: u64, page: CodePage, read: &mut dyn FnMut(u64) -> bool) -> Result<u32> {
    let partial = partial_path(index);
    publish(&partial, index, write_index(pgn, &partial, stamp, page, read))
}

/// Puts the index `written` to `partial` in the place of `index`, or removes
/// what was written when it failed.
fn publish<T>(partial: &Path, index: &Path, written: Result<T>) -> Result<T> {
    match written {
        Ok(done) => {
            std::fs::rename(partial, index).map_err(io(index))?;
            Ok(done)
        }
        Err(e) => {
            let _ = std::fs::remove_file(partial);
            Err(e)
        }
    }
}

/// Writes the index of `pgn` to `out_path` in three steps: the records as
/// the games are read, the name table after them, and the header, which
/// counts both, over the zeros written first in its place.
fn write_index(
    pgn: &Path,
    out_path: &Path,
    stamp: u64,
    page: CodePage,
    read: &mut dyn FnMut(u64) -> bool,
) -> Result<u32> {
    let file = File::open(pgn).map_err(io(pgn))?;
    let mut out = start_index(out_path)?;
    let mut building = Building::new(page);
    let (games, total) = read_games(pgn, file, &mut building, &mut out, out_path, read)?;
    finish_index(&building, games, total, stamp, out, out_path)?;
    Ok(games)
}

/// An index file begun at `out_path`: zeros in the place of its header.
fn start_index(out_path: &Path) -> Result<BufWriter<File>> {
    let mut out = BufWriter::with_capacity(CHUNK, File::create(out_path).map_err(io(out_path))?);
    out.write_all(&[0; HEADER_SIZE as usize]).map_err(io(out_path))?;
    Ok(out)
}

/// Ends the index `out`, the file `out_path`, whose `games` records are
/// written: the name table of `building` after them, then the header, over
/// the zeros written first in its place, for the PGN file of `total` bytes
/// and `stamp`.
fn finish_index(
    building: &Building,
    games: u32,
    total: u64,
    stamp: u64,
    mut out: BufWriter<File>,
    out_path: &Path,
) -> Result<()> {
    let names_at = HEADER_SIZE + u64::from(games) * RECORD_SIZE as u64;
    write_names(building, names_at, &mut out, out_path)?;
    let counts = [
        building.players.list.len() as u32,
        building.tournaments.list.len() as u32,
        building.annotators.list.len() as u32,
    ];
    let header = header(stamp, total, games, counts, names_at, building.page);
    let mut file = out.into_inner().map_err(|e| Error::Io(out_path.to_path_buf(), e.into_error()))?;
    file.seek(SeekFrom::Start(0)).map_err(io(out_path))?;
    file.write_all(&header).map_err(io(out_path))?;
    file.sync_all().map_err(io(out_path))?;
    Ok(())
}

/// Reads the games of `file`, the PGN file `pgn`, keeping their names in
/// `building` and writing each one's record to `out`, the file `out_path`,
/// as the game ends. The number of games, and the length of the text read.
fn read_games(
    pgn: &Path,
    mut file: File,
    building: &mut Building,
    out: &mut impl Write,
    out_path: &Path,
    read: &mut dyn FnMut(u64) -> bool,
) -> Result<(u32, u64)> {
    let mut games: u32 = 0;
    let failed: RefCell<Option<Error>> = RefCell::new(None);
    let mut splitter = Splitter::new(|game: &Game| {
        if failed.borrow().is_some() {
            return;
        }
        let written = building.record(game).and_then(|record| {
            games = games
                .checked_add(1)
                .filter(|&g| g < u32::MAX)
                .ok_or_else(|| Error::Format("the file holds more games than a database can number".into()))?;
            out.write_all(&record).map_err(io(out_path))
        });
        if let Err(e) = written {
            *failed.borrow_mut() = Some(e);
        }
    });
    let mut lexer = Lexer::new();
    let mut buf = vec![0u8; CHUNK];
    // Bytes read in all, for `read`, and the length of the text.
    let (mut read_so_far, mut total) = (0u64, 0u64);
    let mut resumes = 0;
    loop {
        loop {
            let n = match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(Error::Io(pgn.to_path_buf(), e)),
            };
            let mut bytes = &buf[..n];
            // A byte-order mark is no game's.
            if lexer.position() == 0 && bytes.starts_with(b"\xef\xbb\xbf") {
                lexer.reset(3);
                bytes = &bytes[3..];
            }
            read_so_far += n as u64;
            lexer.feed(bytes, &mut splitter);
            if let Some(e) = failed.borrow_mut().take() {
                return Err(e);
            }
            if !read(read_so_far) {
                return Err(Error::Format("the index build was stopped".into()));
            }
        }
        total = total.max(lexer.position());
        // A comment left open to the end ends before the first game header it
        // holds, and the file is read again from there, a bounded number of
        // times.
        match lexer.finish(&mut splitter, resumes < MAX_RESUMES) {
            Some(at) => {
                resumes += 1;
                lexer.reset(at);
                file.seek(SeekFrom::Start(at)).map_err(io(pgn))?;
            }
            None => break,
        }
    }
    splitter.finish();
    drop(splitter);
    match failed.into_inner() {
        Some(e) => Err(e),
        None => Ok((games, total)),
    }
}

/// Writes the name table to `out`, where it starts at `names_at` of the
/// index: an entry for every player, each tournament's title and place and
/// every annotator, then their texts in the same order.
fn write_names(building: &Building, names_at: u64, out: &mut impl Write, out_path: &Path) -> Result<()> {
    let texts: Vec<&str> = building
        .players
        .list
        .iter()
        .map(String::as_str)
        .chain(building.tournaments.list.iter().flat_map(|(t, p)| [t.as_str(), p.as_str()]))
        .chain(building.annotators.list.iter().map(String::as_str))
        .collect();
    let mut at = names_at + texts.len() as u64 * ENTRY_SIZE;
    for text in &texts {
        let mut entry = [0u8; ENTRY_SIZE as usize];
        entry.put::<ENTRY_TEXT_AT, 8>(at.to_le_bytes());
        entry.put::<ENTRY_LEN_AT, 4>((text.len() as u32).to_le_bytes());
        out.write_all(&entry).map_err(io(out_path))?;
        at += text.len() as u64;
    }
    for text in &texts {
        out.write_all(text.as_bytes()).map_err(io(out_path))?;
    }
    Ok(())
}

/// The index header: the build's `stamp`, the `total` bytes of the PGN file
/// read, its `games`, the `counts` of players, tournaments and annotators,
/// where the name table starts and the code `page`.
fn header(
    stamp: u64,
    total: u64,
    games: u32,
    counts: [u32; 3],
    names_at: u64,
    page: CodePage,
) -> [u8; HEADER_SIZE as usize] {
    let mut h = [0u8; HEADER_SIZE as usize];
    h.put::<MAGIC_AT, 8>(*MAGIC);
    h.put::<VERSION_AT, 4>(VERSION.to_le_bytes());
    h.put::<RECORD_SIZE_AT, 4>((RECORD_SIZE as u32).to_le_bytes());
    h.put::<STAMP_AT, 8>(stamp.to_le_bytes());
    h.put::<TEXT_LEN_AT, 8>(total.to_le_bytes());
    h.put::<GAMES_AT, 4>(games.to_le_bytes());
    h.put::<PLAYERS_AT, 4>(counts[0].to_le_bytes());
    h.put::<TOURNAMENTS_AT, 4>(counts[1].to_le_bytes());
    h.put::<ANNOTATORS_AT, 4>(counts[2].to_le_bytes());
    h.put::<NAME_TABLE_AT, 8>(names_at.to_le_bytes());
    h.put::<PAGE_AT, 2>(page.number().to_le_bytes());
    h
}

/// A PGN file and its index.
pub struct Database {
    path: PathBuf,
    text: DbFile,
    /// The index: its records, and the name table after them.
    index: RecordFile<RECORD_SIZE>,
    index_len: u64,
    /// The length of the PGN file the index was built from.
    text_len: u64,
    /// Players, tournaments and annotators.
    counts: [u32; 3],
    names_at: u64,
    page: CodePage,
}

/// The kinds of names, in the order of the name table.
#[derive(Clone, Copy)]
enum NameKind {
    Players,
    Tournaments,
    Annotators,
}

impl Database {
    /// The PGN file `pgn` with its index `index`, which must have been built
    /// for `stamp` and `page` from the whole of the file as it is now.
    pub fn open(pgn: &Path, index: &Path, stamp: u64, page: CodePage) -> Result<Database> {
        let text = DbFile::open(pgn.to_path_buf())?;
        // The index is the bridge's own, which it sweeps and rebuilds on a
        // schedule of its own: kept open, never opened again at its path.
        let index_file = DbFile::open_kept(index.to_path_buf())?;
        let index_len = index_file.size()?;
        let bad = |what: &str| Error::Format(format!("PGN index {}: {what}", index.display()));
        if index_len < HEADER_SIZE {
            return Err(bad("too short"));
        }
        let mut h = [0u8; HEADER_SIZE as usize];
        index_file.read_into(0, &mut h)?;
        if h.field::<MAGIC_AT, 8>() != MAGIC
            || h.le_u32::<VERSION_AT>() != VERSION
            || h.le_u32::<RECORD_SIZE_AT>() != RECORD_SIZE as u32
        {
            return Err(bad("not an index of this version"));
        }
        if h.le_u64::<STAMP_AT>() != stamp
            || h.le_u64::<TEXT_LEN_AT>() != text.size()?
            || h.le_u16::<PAGE_AT>() != page.number()
        {
            return Err(bad("built for another state of the file"));
        }
        let games = h.le_u32::<GAMES_AT>();
        let counts = [h.le_u32::<PLAYERS_AT>(), h.le_u32::<TOURNAMENTS_AT>(), h.le_u32::<ANNOTATORS_AT>()];
        let names_at = h.le_u64::<NAME_TABLE_AT>();
        let entries = u64::from(counts[0]) + 2 * u64::from(counts[1]) + u64::from(counts[2]);
        if names_at != HEADER_SIZE + u64::from(games) * RECORD_SIZE as u64
            || names_at.checked_add(entries * ENTRY_SIZE).is_none_or(|end| end > index_len)
        {
            return Err(bad("sizes that do not fit the file"));
        }
        let index = RecordFile::new(index_file, HEADER_SIZE, games);
        let text_len = h.le_u64::<TEXT_LEN_AT>();
        Ok(Database { path: pgn.to_path_buf(), text, index, index_len, text_len, counts, names_at, page })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record_count(&self) -> u32 {
        self.index.count()
    }

    /// The code page of games that are not UTF-8.
    pub fn code_page(&self) -> CodePage {
        self.page
    }

    /// The record of 1-based game `id`.
    pub fn record(&self, id: u32) -> Result<Record> {
        Ok(Record { id, b: self.index.record(id)? })
    }

    /// Reads records from `first` into `buf`, as many as it holds up to the
    /// last, in one read; how many it read, none when `first` is 0 or past
    /// the end. It allocates nothing.
    pub fn read_records(&self, first: u32, buf: &mut [u8]) -> Result<u32> {
        self.index.read_records(first, buf)
    }

    /// Records `first..=last`, clamped to the database and to
    /// [`crate::game::MAX_BATCH_RECORDS`] records, in one read.
    pub fn records(&self, first: u32, last: u32) -> Result<Vec<Record>> {
        self.index.records(first, last, Record::from_bytes)
    }

    /// `first..=last` clamped as [`Database::records`] clamps it: within the
    /// database and at most [`crate::game::MAX_BATCH_RECORDS`] long, with
    /// `first` at least 1.
    pub(crate) fn clamp(&self, first: u32, last: u32) -> (u32, u32) {
        self.index.clamp(first, last)
    }

    pub fn players(&self) -> u32 {
        self.counts[0]
    }

    pub fn tournaments(&self) -> u32 {
        self.counts[1]
    }

    pub fn annotators(&self) -> u32 {
        self.counts[2]
    }

    /// Entry `n` of the name table's texts, read to at most `limit` bytes; a
    /// longer text, which only a damaged index holds, is empty.
    fn name(&self, n: u64, limit: usize) -> Result<String> {
        let mut e = [0u8; ENTRY_SIZE as usize];
        self.index.file().read_into(self.names_at + n * ENTRY_SIZE, &mut e)?;
        let (at, len) = (e.le_u64::<ENTRY_TEXT_AT>(), e.le_u32::<ENTRY_LEN_AT>() as usize);
        if len > limit || at.checked_add(len as u64).is_none_or(|end| end > self.index_len) {
            return Ok(String::new());
        }
        Ok(String::from_utf8_lossy(&self.index.file().read(at, len)?).into_owned())
    }

    /// The table position of name `id` of `kind`; `None` for an id the table
    /// does not have.
    fn slot(&self, kind: NameKind, id: i64) -> Option<u64> {
        let (before, count) = match kind {
            NameKind::Players => (0, self.counts[0]),
            NameKind::Tournaments => (u64::from(self.counts[0]), self.counts[1]),
            NameKind::Annotators => (u64::from(self.counts[0]) + 2 * u64::from(self.counts[1]), self.counts[2]),
        };
        let id = u64::try_from(id).ok().filter(|&id| id < u64::from(count))?;
        Some(match kind {
            NameKind::Tournaments => before + 2 * id,
            _ => before + id,
        })
    }

    /// Player `id` as written, `Last, First`, split at its first comma.
    pub fn player(&self, id: i64) -> Result<Option<Player>> {
        self.player_within(id, MAX_NAME)
    }

    /// [`Database::player`] from a name of at most `limit` bytes.
    pub fn player_within(&self, id: i64, limit: usize) -> Result<Option<Player>> {
        let Some(slot) = self.slot(NameKind::Players, id) else { return Ok(None) };
        let text = self.name(slot, limit)?;
        Ok(Some(match text.split_once(',') {
            Some((last, first)) => Player { last: last.trim().to_string(), first: first.trim().to_string() },
            None => Player { last: text, first: String::new() },
        }))
    }

    /// Tournament `id`: its event as its title and its site as its place.
    pub fn tournament(&self, id: i64) -> Result<Option<Tournament>> {
        self.tournament_within(id, MAX_NAME)
    }

    pub fn tournament_within(&self, id: i64, limit: usize) -> Result<Option<Tournament>> {
        let Some(slot) = self.slot(NameKind::Tournaments, id) else { return Ok(None) };
        Ok(Some(Tournament {
            title: self.name(slot, limit)?,
            place: self.name(slot + 1, limit)?,
            start: Date(0),
            kind: 0,
        }))
    }

    pub fn annotator(&self, id: i64) -> Result<Option<String>> {
        self.annotator_within(id, MAX_NAME)
    }

    pub fn annotator_within(&self, id: i64, limit: usize) -> Result<Option<String>> {
        let Some(slot) = self.slot(NameKind::Annotators, id) else { return Ok(None) };
        self.name(slot, limit).map(Some)
    }

    /// Fills `buf` with the PGN file's bytes from `offset`, where
    /// [`Record::offset`] places a game: the texts of consecutive games are
    /// read at once so.
    pub fn read_span(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.text.read_into(offset, buf)
    }

    /// The game's bytes as the file holds them, refused before anything is
    /// allocated when longer than `limit`.
    pub fn bytes(&self, r: &Record, limit: usize) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.bytes_into(r, limit, &mut buf)?;
        Ok(buf)
    }

    /// [`Database::bytes`] into `buf`, which a caller reading many games
    /// reuses.
    pub fn bytes_into(&self, r: &Record, limit: usize, buf: &mut Vec<u8>) -> Result<()> {
        let len = r.len() as usize;
        if len > limit {
            return Err(Error::Format(format!("the game is {}", over_limit(len, limit))));
        }
        buf.clear();
        buf.resize(len, 0);
        self.text.read_into(r.offset(), buf)
    }

    /// The game's text as a reader is shown it: [`Database::text`], with the
    /// layout marks of a Chessable course in its comments read as brackets
    /// (`@@StartBracket@@39@@EndBracket@@` is `(39)`, #318). A write keeps
    /// the text as written.
    pub fn reading(&self, r: &Record, limit: usize) -> Result<String> {
        let text = self.text(r, limit)?;
        Ok(match crate::signs::read_course_marks(&text) {
            Cow::Borrowed(_) => text,
            Cow::Owned(read) => read,
        })
    }

    /// The game's text as written, refused before it is read when longer
    /// than `limit` bytes: UTF-8 when it is valid UTF-8, else in the code
    /// page, with every line ending in `\n`, and a final one.
    pub fn text(&self, r: &Record, limit: usize) -> Result<String> {
        let bytes = self.bytes(r, limit)?;
        let text = self.page.utf8_or(&bytes);
        let mut out = String::with_capacity(text.len() + 1);
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\r' {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            } else {
                out.push(c);
            }
        }
        if !out.ends_with('\n') {
            out.push('\n');
        }
        Ok(out)
    }
}
