//! The index of a PGN file after an edit of one span of it, made from the
//! index before the edit ([`update`]): the bridge appends a game, or replaces
//! or removes one, and the file is not read again from its start. The index
//! written is the one a build of the whole file writes ([`super::build`]),
//! byte for byte.
//!
//! The games before the one ahead of the edited span keep their records. The
//! text is read again from the start of the game ahead of the span, since the
//! edit may end that game elsewhere, until a game starts where a game of the
//! old file after the span started, moved by what the edit added or removed.
//! The lexer and the splitter start a game in the same state whatever came
//! before it (a tag ends the variations left open), so from there the text
//! is read as it was, and the old records follow, moved. Names take their ids
//! again in the order a build meets them, so the name table is a build's too.
//!
//! A text left inside a comment that is never closed, which a build reads
//! again from a game header inside the comment, is read again whole: that
//! reading depends on the whole file.

use std::cell::{Cell, RefCell};
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::lex::Lexer;
use super::scan::{Game, Splitter};
use super::{
    ANNOTATOR_AT, BLACK_AT, Building, CHUNK, Database, ENTRY_LEN_AT, ENTRY_SIZE, ENTRY_TEXT_AT, MAX_NAME, MAX_RESUMES,
    OFFSET_AT, RECORD_SIZE, Record, TOURNAMENT_AT, WHITE_AT, build, finish_index, io, partial_path, publish,
    start_index, too_many_names,
};
use crate::bytes::{Fields, array};
use crate::{Error, Result};

const BOM: &[u8] = b"\xef\xbb\xbf";

/// One span of a PGN file edited: the games it held and what it holds now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edit {
    /// The number of the first game the edit changes, from 1: the number
    /// after the last game for an append.
    pub first: u32,
    /// The games of the old file from `first` that the edit replaced or
    /// removed: 0 for an append, 1 for a replace or a removal.
    pub removed: u32,
    /// The bytes the edit added to the file; negative when it took away more.
    pub delta: i64,
}

/// The games of `text`, read as a build reads a file: a byte-order mark
/// first is no game's, and a comment left open is read again from a game
/// header it holds.
pub fn games(text: &[u8]) -> Vec<Game> {
    let mut games = Vec::new();
    let mut splitter = Splitter::new(|g: &Game| games.push(g.clone()));
    let from = if text.starts_with(BOM) { BOM.len() } else { 0 };
    let mut lexer = Lexer::at(from as u64);
    let mut rest = &text[from..];
    let mut resumes = 0;
    loop {
        lexer.feed(rest, &mut splitter);
        match lexer.finish(&mut splitter, resumes < MAX_RESUMES) {
            Some(at) => {
                resumes += 1;
                lexer.reset(at);
                rest = text.get(at as usize..).unwrap_or_default();
            }
            None => break,
        }
    }
    splitter.finish();
    drop(splitter);
    games
}

/// Writes to `index` the index of the PGN file `pgn` after `edit`, stamped
/// `stamp`, from `old`, the index of the file before it: the index a build
/// of the file as it is now writes. The index is written beside `index` first
/// and takes its place only when complete. Returns the number of games.
pub fn update(old: &Database, pgn: &Path, index: &Path, stamp: u64, edit: &Edit) -> Result<u32> {
    let partial = partial_path(index);
    match write_update(old, pgn, &partial, stamp, edit) {
        Ok(Some(games)) => publish(&partial, index, Ok(games)),
        // The edit cannot be told apart from the rest of the file.
        Ok(None) => {
            let _ = std::fs::remove_file(&partial);
            build(pgn, index, stamp, old.page, &mut |_| true)
        }
        Err(e) => publish(&partial, index, Err(e)),
    }
}

/// [`update`] into `out_path`; `None` when the file must be read whole.
fn write_update(old: &Database, pgn: &Path, out_path: &Path, stamp: u64, edit: &Edit) -> Result<Option<u32>> {
    let count = old.record_count();
    let after = edit.first.checked_add(edit.removed);
    if edit.first == 0 || after.is_none_or(|after| after > count.saturating_add(1)) {
        return Err(Error::Format(format!("an edit of games {} to {count} games", edit.first)));
    }
    let Some(names) = OldNames::read(old)? else { return Ok(None) };
    let mut out = start_index(out_path)?;
    let mut copy = Copy { names: &names, building: Building::new(old.page), games: 0, out: &mut out, out_path };
    // The games before the one ahead of the edited span, as they were.
    let kept = edit.first.saturating_sub(2);
    copy.records(old, 1, kept, 0)?;
    let from = if edit.first >= 2 { old.record(edit.first - 1)?.offset() } else { 0 };
    let Some(next) = read_window(old, pgn, from, edit, &mut copy)? else { return Ok(None) };
    copy.records(old, next, count, edit.delta)?;
    let total = std::fs::metadata(pgn).map_err(io(pgn))?.len();
    let Copy { building, games, .. } = copy;
    finish_index(&building, games, total, stamp, out, out_path)?;
    Ok(Some(games))
}

/// The name table of an index, read whole, by id.
struct OldNames {
    players: Vec<String>,
    tournaments: Vec<(String, String)>,
    annotators: Vec<String>,
}

impl OldNames {
    /// The names of `old`; `None` when an entry points outside the index,
    /// which only a damaged one does, so that the file is read whole.
    fn read(old: &Database) -> Result<Option<OldNames>> {
        let [players, tournaments, annotators] = old.counts.map(|c| c as usize);
        let entries = players + 2 * tournaments + annotators;
        let len = usize::try_from(old.index_len - old.names_at)
            .map_err(|_| Error::Format("a PGN index's names are larger than memory".into()))?;
        let table = old.index.file().read(old.names_at, len)?;
        let mut texts = Vec::with_capacity(entries);
        for n in 0..entries {
            let Some(e) = array::<{ ENTRY_SIZE as usize }>(&table, n * ENTRY_SIZE as usize) else { return Ok(None) };
            let (at, len) = (e.le_u64::<ENTRY_TEXT_AT>(), e.le_u32::<ENTRY_LEN_AT>() as usize);
            let text = at
                .checked_sub(old.names_at)
                .and_then(|at| usize::try_from(at).ok())
                .filter(|_| len <= MAX_NAME)
                .and_then(|at| table.get(at..at.checked_add(len)?));
            let Some(text) = text else { return Ok(None) };
            texts.push(String::from_utf8_lossy(text).into_owned());
        }
        let mut texts = texts.into_iter();
        let players = texts.by_ref().take(players).collect();
        let mut tournaments = Vec::with_capacity(tournaments);
        for _ in 0..tournaments.capacity() {
            tournaments.push((texts.next().unwrap_or_default(), texts.next().unwrap_or_default()));
        }
        Ok(Some(OldNames { players, tournaments, annotators: texts.collect() }))
    }
}

/// The new index as it is written: the records so far, with the names they
/// use given their new ids.
struct Copy<'a> {
    names: &'a OldNames,
    building: Building,
    games: u32,
    out: &'a mut BufWriter<File>,
    out_path: &'a Path,
}

impl Copy<'_> {
    /// Copies the records `first..=last` of `old`, their games moved by
    /// `delta` bytes.
    fn records(&mut self, old: &Database, first: u32, last: u32, delta: i64) -> Result<()> {
        let mut next = first;
        while next <= last {
            let batch = old.records(next, last)?;
            let Some(end) = batch.last().map(Record::id) else { break };
            for record in &batch {
                let b = self.moved(record, delta)?;
                self.write(&b)?;
            }
            next = end + 1;
        }
        Ok(())
    }

    /// `record`'s bytes with the new ids of its names, its game moved by
    /// `delta` bytes. The names are met in the order a build meets them.
    fn moved(&mut self, record: &Record, delta: i64) -> Result<[u8; RECORD_SIZE]> {
        let white = self.person(record, record.white(), false)?;
        let black = self.person(record, record.black(), false)?;
        let tournament = self.tournament(record)?;
        let annotator = self.person(record, record.annotator(), true)?;
        let offset = record
            .offset()
            .checked_add_signed(delta)
            .ok_or_else(|| Error::Format(format!("PGN index record {} moved before the file", record.id())))?;
        let mut bytes = record.b;
        bytes.put::<OFFSET_AT, 8>(offset.to_le_bytes());
        bytes.put::<WHITE_AT, 4>(white.to_le_bytes());
        bytes.put::<BLACK_AT, 4>(black.to_le_bytes());
        bytes.put::<TOURNAMENT_AT, 4>(tournament.to_le_bytes());
        bytes.put::<ANNOTATOR_AT, 4>(annotator.to_le_bytes());
        Ok(bytes)
    }

    /// The new id, stored one higher, of player or annotator `id` of the old
    /// index, which `record` names.
    fn person(&mut self, record: &Record, id: i64, annotator: bool) -> Result<u32> {
        if id < 0 {
            return Ok(0);
        }
        let old = if annotator { &self.names.annotators } else { &self.names.players };
        let text = old.get(id as usize).ok_or_else(|| damaged(record))?;
        if text.is_empty() {
            return Ok(0);
        }
        let b = &mut self.building;
        let table = if annotator { &mut b.annotators } else { &mut b.players };
        table.intern(text.clone(), text.len(), &mut b.held).ok_or_else(too_many_names)
    }

    /// The new id, stored one higher, of the tournament of `record`.
    fn tournament(&mut self, record: &Record) -> Result<u32> {
        let id = record.tournament();
        if id < 0 {
            return Ok(0);
        }
        let (event, site) = self.names.tournaments.get(id as usize).ok_or_else(|| damaged(record))?;
        if event.is_empty() && site.is_empty() {
            return Ok(0);
        }
        let b = &mut self.building;
        let bytes = event.len() + site.len();
        b.tournaments.intern((event.clone(), site.clone()), bytes, &mut b.held).ok_or_else(too_many_names)
    }

    /// The record of `game`, read from the new text, as a build makes it.
    fn read(&mut self, game: &Game) -> Result<()> {
        let b = self.building.record(game)?;
        self.write(&b)
    }

    fn write(&mut self, record: &[u8; RECORD_SIZE]) -> Result<()> {
        self.games = self
            .games
            .checked_add(1)
            .filter(|&g| g < u32::MAX)
            .ok_or_else(|| Error::Format("the file holds more games than a database can number".into()))?;
        self.out.write_all(record).map_err(io(self.out_path))
    }
}

fn damaged(record: &Record) -> Error {
    Error::Format(format!("PGN index record {} names a name its table lacks", record.id()))
}

/// Reads the edited file `pgn` from `from`, where the game ahead of the
/// edited span starts (or the file), writing the record of each game to
/// `copy`, until a game starts where an old game after the span started,
/// moved by the edit: the number of that old game, from which the old
/// records follow, or the number after the last when no game does. `None`
/// when the text is left inside a comment that is never closed.
fn read_window(old: &Database, pgn: &Path, from: u64, edit: &Edit, copy: &mut Copy) -> Result<Option<u32>> {
    let count = old.record_count();
    // The old game after the span looked for next, and where it starts now.
    let next = Cell::new(edit.first + edit.removed);
    let start = |n: u32| -> Result<Option<u64>> {
        if n > count {
            return Ok(None);
        }
        Ok(old.record(n)?.offset().checked_add_signed(edit.delta))
    };
    let found: Cell<Option<u32>> = Cell::new(None);
    let failed: RefCell<Option<Error>> = RefCell::new(None);
    let mut splitter = Splitter::new(|game: &Game| {
        if found.get().is_some() || failed.borrow().is_some() {
            return;
        }
        // Old games that now start before this one were taken into a game
        // read here.
        let read = loop {
            match start(next.get()) {
                Ok(Some(at)) if at < game.start => next.set(next.get() + 1),
                Ok(Some(at)) if at == game.start => {
                    found.set(Some(next.get()));
                    return;
                }
                Ok(_) => break copy.read(game),
                Err(e) => break Err(e),
            }
        };
        if let Err(e) = read {
            *failed.borrow_mut() = Some(e);
        }
    });
    let mut file = File::open(pgn).map_err(io(pgn))?;
    file.seek(SeekFrom::Start(from)).map_err(io(pgn))?;
    let mut lexer = Lexer::at(from);
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::Io(pgn.to_path_buf(), e)),
        };
        let mut bytes = &buf[..n];
        // A byte-order mark is no game's, as a build reads it.
        if lexer.position() == 0 && bytes.starts_with(BOM) {
            lexer.reset(BOM.len() as u64);
            bytes = &bytes[BOM.len()..];
        }
        lexer.feed(bytes, &mut splitter);
        if found.get().is_some() || failed.borrow().is_some() {
            break;
        }
    }
    if found.get().is_none() && failed.borrow().is_none() {
        if lexer.finish(&mut splitter, true).is_some() {
            return Ok(None);
        }
        splitter.finish();
    }
    drop(splitter);
    if let Some(e) = failed.into_inner() {
        return Err(e);
    }
    Ok(Some(found.get().unwrap_or(count + 1)))
}

#[cfg(test)]
mod tests {
    use super::super::Database;
    use super::*;
    use crate::codepage::CodePage;

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("cbformat-pgn-edit-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The bytes of the index a build writes for `text`.
    fn built(dir: &Path, text: &[u8], stamp: u64) -> Vec<u8> {
        let (pgn, index) = (dir.join("built.pgn"), dir.join("built.head"));
        std::fs::write(&pgn, text).unwrap();
        build(&pgn, &index, stamp, CodePage::WESTERN, &mut |_| true).unwrap();
        std::fs::read(&index).unwrap()
    }

    /// Edits `before` into `after`, replacing the bytes `span` of it, and
    /// checks that the index [`update`] writes from the index of `before` is
    /// the one a build writes for `after`, as the edit names the games.
    fn check(name: &str, before: &[u8], span: std::ops::Range<usize>, with: &[u8], first: u32, removed: u32) {
        let scratch = Scratch::new(name);
        let dir = &scratch.0;
        let (pgn, index) = (dir.join("db.pgn"), dir.join("db.head"));
        std::fs::write(&pgn, before).unwrap();
        build(&pgn, &index, 1, CodePage::WESTERN, &mut |_| true).unwrap();
        let old = Database::open(&pgn, &index, 1, CodePage::WESTERN).unwrap();
        let mut after = before[..span.start].to_vec();
        after.extend_from_slice(with);
        after.extend_from_slice(&before[span.end..]);
        let delta = with.len() as i64 - span.len() as i64;
        // The edit replaces the file, as the bridge does.
        let edited = dir.join("db.pgn.new");
        std::fs::write(&edited, &after).unwrap();
        std::fs::rename(&edited, &pgn).unwrap();
        let games = update(&old, &pgn, &index, 2, &Edit { first, removed, delta }).unwrap();
        let expected = built(dir, &after, 2);
        let text = String::from_utf8_lossy(&after);
        assert_eq!(std::fs::read(&index).unwrap(), expected, "{name}: {text}");
        let reopened = Database::open(&pgn, &index, 2, CodePage::WESTERN).unwrap();
        assert_eq!(reopened.record_count(), games, "{name}");
    }

    fn game(event: &str, white: &str, moves: &str) -> String {
        format!("[Event \"{event}\"]\n[White \"{white}\"]\n[Black \"Black, {event}\"]\n\n{moves}")
    }

    /// Appends, replacements and removals at the start, inside and at the end
    /// of a file, with names new and shared, give a build's index.
    #[test]
    fn an_edited_index_is_a_built_one() {
        let games = [
            game("One", "Morphy, Paul", "1. e4 e5 1-0"),
            game("Two", "Anderssen, Adolf", "1. d4 d5 0-1"),
            game("One", "Morphy, Paul", "1. c4 {a comment} c5 1/2-1/2 {after}"),
            game("Four", "Tal, Mikhail", "1. Nf3 *"),
        ];
        let file = games.join("\n\n") + "\n\n";
        let starts: Vec<usize> = games
            .iter()
            .scan(0, |at, g| {
                let start = *at;
                *at += g.len() + 2;
                Some(start)
            })
            .collect();
        let new = game("New", "Carlsen, Magnus", "1. e4 c5 2. Nf3 1-0");
        // An append, as the bridge writes it.
        let appended = format!("{new}\n\n");
        check("append", file.as_bytes(), file.len()..file.len(), appended.as_bytes(), 5, 0);
        let empty = "";
        check("append-to-nothing", empty.as_bytes(), 0..0, appended.as_bytes(), 1, 0);
        for (n, &start) in starts.iter().enumerate() {
            let span = start..start + games[n].len();
            let number = n as u32 + 1;
            check(&format!("replace-{number}"), file.as_bytes(), span.clone(), new.as_bytes(), number, 1);
            let shorter = game("One", "Morphy, Paul", "1. e4 1-0");
            check(&format!("replace-short-{number}"), file.as_bytes(), span, shorter.as_bytes(), number, 1);
            let removed = start..starts.get(n + 1).copied().unwrap_or(file.len());
            check(&format!("remove-{number}"), file.as_bytes(), removed, b"", number, 1);
        }
    }

    /// Edits whose games read otherwise beside their neighbours: a game
    /// without a result takes the movetext after it, so that old games end
    /// inside the new ones, and a byte-order mark comes first.
    #[test]
    fn an_edit_that_joins_games_gives_a_built_index() {
        // Without its result, game 1 takes the movetext of game 2 when the
        // game between them goes.
        let file = "[Event \"a\"]\n1. e4\n\n[Event \"b\"]\n1. d4 1-0\n\n1. c4 0-1\n\n[Event \"d\"]\n1. f4 *\n";
        let b = file.find("[Event \"b\"]").unwrap();
        let c = file.find("1. c4").unwrap();
        check("remove-joins", file.as_bytes(), b..c, b"", 2, 1);
        check("replace-joins", file.as_bytes(), b..c - 2, b"1. g3", 2, 1);
        let bom = format!("\u{feff}{file}");
        check("bom-remove-first", bom.as_bytes(), 3..b + 3, b"", 1, 1);
        check("bom-replace-first", bom.as_bytes(), 3..3 + "[Event \"a\"]\n1. e4".len(), b"[Event \"z\"] 1. h4 *", 1, 1);
    }

    /// A comment left open by the edit, which a build reads again from a game
    /// header inside it, reads the file whole, and still gives a build's
    /// index.
    #[test]
    fn a_comment_left_open_reads_the_file_whole() {
        let file = "[Event \"a\"]\n1. e4 1-0\n\n[Event \"b\"]\n1. d4 1-0\n\n[Event \"c\"]\n1. c4 0-1\n";
        let b = file.find("[Event \"b\"]").unwrap();
        let end = b + "[Event \"b\"]\n1. d4 1-0".len();
        check("open-comment", file.as_bytes(), b..end, b"[Event \"b\"]\n1. d4 {left open", 2, 1);
    }

    #[test]
    fn games_of_a_text() {
        assert_eq!(games(b"").len(), 0);
        assert_eq!(games(b"{just a comment}\n").len(), 0);
        let one = games("\u{feff}[Event \"a\"]\n1. e4 *\n".as_bytes());
        assert_eq!((one.len(), one[0].start, one[0].plies), (1, 3, 1));
        assert_eq!(games(b"1. e4 1-0 1. d4 0-1").len(), 2);
        assert_eq!(games(b"[Event \"a\"]\n[Event \"b\"]").len(), 2);
    }
}
