//! Writes into PGN databases through the HTTP API (`docs/api.md`, "Writing
//! games"): appends, replaces and removals written as the file is written,
//! byte for byte, the header index made for the new file without reading the
//! file again, and the refusals, which leave the file as it was.

use std::path::{Path, PathBuf};

use bridge::catalog::id_of;
use cbformat::codepage::CodePage;
use cbformat::pgnfile;

mod common;
use common::{
    ORIGIN, Reply, TOKEN, TestBridge, WAIT_LIMIT, app_of, classic_fixture, fixture, get_reply, member, object_with,
    objects, poll, send, string_member,
};

/// A folder of the test's own, removed with it.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("bridge-write-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn path(&self, file: &str) -> PathBuf {
        self.0.join(file)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `method path` with `body`, as the page sends a write: with the token, an
/// allowed `Origin`, and `If-Match` naming `generation` when given.
fn write(port: u16, method: &str, path: &str, generation: Option<&str>, body: &str) -> Reply {
    let precondition = generation.map(|g| format!("If-Match: \"{g}\"\r\n")).unwrap_or_default();
    send(
        port,
        &format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nOrigin: {ORIGIN}\r\nContent-Type: application/x-chess-pgn\r\nContent-Length: {}\r\n{precondition}Connection: close\r\n\r\n{body}",
            body.len()
        ),
    )
}

/// The row of database `id` in `GET /v1/databases`, once it is ready.
fn ready_row(port: u16, id: &str) -> String {
    let row = poll(WAIT_LIMIT, || {
        let list = get_reply(port, "/v1/databases");
        let row = object_with(&list.body, &format!(r#""id":"{id}""#))?.to_string();
        (string_member(&row, "state") == "ready").then_some(row)
    });
    row.unwrap_or_else(|| panic!("database {id} was not ready within {WAIT_LIMIT:?}"))
}

/// The row of database `id` now, whatever its state.
fn row_now(port: u16, id: &str) -> String {
    let list = get_reply(port, "/v1/databases");
    object_with(&list.body, &format!(r#""id":"{id}""#)).unwrap_or_else(|| panic!("{}", list.body)).to_string()
}

/// `text` as a file of `layout` holds it: its line ends `eol`, in UTF-8 or
/// in Windows-1252.
fn file_text(text: &str, eol: &str, utf8: bool) -> Vec<u8> {
    let text = text.replace('\n', eol);
    if utf8 { text.into_bytes() } else { CodePage::WESTERN.encode(&text).unwrap() }
}

const GAME_1: &str = "[Event \"Club\"]\n[White \"Ståhlberg, Gideon\"]\n[Black \"Morphy, Paul\"]\n[Result \"1-0\"]\n\n1. e4 e5 2. Nf3 1-0";
const GAME_2: &str = "[Event \"Club\"]\n[White \"Tal, Mikhail\"]\n[Black \"Ståhlberg, Gideon\"]\n[Result \"0-1\"]\n\n1. d4 {Ståhlberg's move} d5 0-1";
const GAME_3: &str =
    "[Event \"Open\"]\n[White \"Morphy, Paul\"]\n[Black \"Anderssen, Adolf\"]\n[Result \"*\"]\n\n1. c4 *";
const NEW: &str = "[Event \"Blitz\"]\n[White \"Ståhlberg, Gideon\"]\n[Black \"Réti, Richard\"]\n[Result \"1/2-1/2\"]\n\n1. e4 c5 2. Nf3 d6 1/2-1/2";
const REPLACEMENT: &str = "[Event \"Club\"]\n[White \"Tal, Mikhail\"]\n[Black \"Ståhlberg, Gideon\"]\n[Result \"1-0\"]\n\n1. d4 d5\n2. c4 {Ståhlberg's error} 1-0";

/// Checks the answer to a write and what it left: the status, the new
/// generation in the body and as the `ETag`, the file's bytes, the database
/// ready at once at that generation with `records` games, and its header
/// index the one a build of the file writes. The new generation.
fn check_written(
    bridge: &TestBridge,
    path: &Path,
    answer: &Reply,
    status: u16,
    expected: &[u8],
    records: u32,
) -> String {
    let id = id_of(path);
    assert_eq!(answer.status, status, "{}", answer.body);
    let generation = string_member(&answer.body, "generation").to_string();
    assert_eq!(answer.header("etag"), Some(format!("\"{generation}\"").as_str()));
    assert_eq!(answer.header("access-control-expose-headers"), Some("Retry-After, ETag"));
    assert_eq!(std::fs::read(path).unwrap(), expected, "{}", String::from_utf8_lossy(expected));
    // Ready at once: the write made the header index, so the file is not
    // read again.
    let row = row_now(bridge.port, &id);
    assert_eq!(string_member(&row, "state"), "ready", "{row}");
    assert_eq!(string_member(&row, "generation"), generation, "{row}");
    assert_eq!(member(&row, "records"), records.to_string(), "{row}");
    assert_eq!(member(&row, "writable"), "true", "{row}");
    let stamp = u64::from_str_radix(&generation, 16).unwrap();
    let built = bridge.dir().join("built.head");
    pgnfile::build(path, &built, stamp, CodePage::WESTERN, &mut |_| true).unwrap();
    let index = bridge.dir().join("pgn").join(format!("{id}.head"));
    assert_eq!(std::fs::read(&index).unwrap(), std::fs::read(&built).unwrap(), "the index of {}", path.display());
    std::fs::remove_file(&built).unwrap();
    generation
}

/// An append, a replace and a removal in UTF-8 and Windows-1252 files with LF
/// and CRLF line ends: each written in the file's encoding and line ends, an
/// append after every earlier byte, the others in the place of their game,
/// and the database ready at the new generation at once.
#[test]
fn writes_follow_the_file() {
    for (name, eol, utf8) in
        [("utf8-lf", "\n", true), ("utf8-crlf", "\r\n", true), ("ansi-lf", "\n", false), ("ansi-crlf", "\r\n", false)]
    {
        let scratch = Scratch::new(name);
        let path = scratch.path("games.pgn");
        let text = |t: &str| file_text(t, eol, utf8);
        // The file's last game ends its line, with no empty line after it.
        let original = [text(GAME_1), text(&format!("\n\n{GAME_2}\n\n{GAME_3}\n"))].concat();
        std::fs::write(&path, &original).unwrap();
        let bridge = TestBridge::new(app_of([path.clone()]));
        let id = id_of(&path);
        let games = format!("/v1/databases/{id}/games");
        let row = ready_row(bridge.port, &id);
        assert_eq!(member(&row, "writable"), "true", "{row}");
        let generation = string_member(&row, "generation").to_string();

        // An append: an empty line after the file's last line, then the game
        // and an empty line. The body's blank lines around the game are not
        // the game's.
        let answer = write(bridge.port, "POST", &games, Some(&generation), &format!("\n{NEW}\n\n"));
        let appended = [original.clone(), text(&format!("\n{NEW}\n\n"))].concat();
        let generation = check_written(&bridge, &path, &answer, 201, &appended, 4);
        assert_eq!(member(&answer.body, "number"), "4", "{}", answer.body);
        let served = get_reply(bridge.port, &format!("{games}/4"));
        assert_eq!(member(&served.body, "pgn"), bridge::json::string(&format!("{NEW}\n")), "{name}");

        // A replace: the game's own bytes, the empty lines around it kept.
        let answer = write(bridge.port, "PUT", &format!("{games}/2"), Some(&generation), REPLACEMENT);
        let replaced = [text(&format!("{GAME_1}\n\n{REPLACEMENT}\n\n{GAME_3}\n\n{NEW}\n\n"))].concat();
        let generation = check_written(&bridge, &path, &answer, 200, &replaced, 4);
        assert_eq!(member(&answer.body, "number"), "2", "{}", answer.body);
        let served = get_reply(bridge.port, &format!("{games}/2"));
        assert_eq!(member(&served.body, "pgn"), bridge::json::string(&format!("{REPLACEMENT}\n")), "{name}");

        // A removal: the game and the empty lines after it, to the next game;
        // the games after it move up.
        let answer = write(bridge.port, "DELETE", &format!("{games}/1"), Some(&generation), "");
        let removed = text(&format!("{REPLACEMENT}\n\n{GAME_3}\n\n{NEW}\n\n"));
        let generation = check_written(&bridge, &path, &answer, 200, &removed, 3);
        assert!(!answer.body.contains("number"), "{}", answer.body);
        let answer = write(bridge.port, "DELETE", &format!("{games}/3"), Some(&generation), "");
        let removed = text(&format!("{REPLACEMENT}\n\n{GAME_3}\n\n"));
        check_written(&bridge, &path, &answer, 200, &removed, 2);
        let served = get_reply(bridge.port, &format!("{games}/1"));
        assert_eq!(member(&served.body, "pgn"), bridge::json::string(&format!("{REPLACEMENT}\n")), "{name}");
        drop(bridge);
    }
}

/// A file without a final line end gets one, then an empty line, before an
/// appended game; an empty file gets the game alone, in CRLF, as ChessBase
/// writes on Windows, and in UTF-8.
#[test]
fn an_append_ends_the_file_first() {
    let scratch = Scratch::new("ends");
    let (open, empty, bom) = (scratch.path("open.pgn"), scratch.path("empty.pgn"), scratch.path("bom.pgn"));
    std::fs::write(&open, b"[Event \"a\"]\n\n1. e4 *").unwrap();
    std::fs::write(&empty, b"").unwrap();
    std::fs::write(&bom, b"\xef\xbb\xbf").unwrap();
    let bridge = TestBridge::new(app_of([open.clone(), empty.clone(), bom.clone()]));
    for (path, before, after) in [
        (&open, &b"[Event \"a\"]\n\n1. e4 *"[..], "\n\n[Event \"b\"]\n\n1. d4 *\n\n"),
        (&empty, &b""[..], "[Event \"b\"]\r\n\r\n1. d4 *\r\n\r\n"),
        (&bom, &b"\xef\xbb\xbf"[..], "[Event \"b\"]\r\n\r\n1. d4 *\r\n\r\n"),
    ] {
        let id = id_of(path);
        let generation = string_member(&ready_row(bridge.port, &id), "generation").to_string();
        let records = if before.len() > 3 { 2 } else { 1 };
        let answer = write(
            bridge.port,
            "POST",
            &format!("/v1/databases/{id}/games"),
            Some(&generation),
            "[Event \"b\"]\n\n1. d4 *",
        );
        check_written(&bridge, path, &answer, 201, &[before, after.as_bytes()].concat(), records);
    }
}

/// Every refusal leaves the file as it was: a write without `If-Match` or
/// with a stale one, a body that is not one playable game, a game number the
/// file does not have, a character the file's code page cannot hold, a
/// ChessBase database, a read-only file, a body where none is taken or one
/// too large.
#[test]
fn refusals_leave_the_file_as_it_was() {
    let scratch = Scratch::new("refusals");
    let (utf8, ansi) = (scratch.path("utf8.pgn"), scratch.path("ansi.pgn"));
    let (utf8_text, ansi_text) = (file_text(GAME_1, "\n", true), file_text(GAME_1, "\r\n", false));
    std::fs::write(&utf8, &utf8_text).unwrap();
    std::fs::write(&ansi, &ansi_text).unwrap();
    let (two_db, classic_db) = (fixture("write-refusals-2cbh", &[]), classic_fixture("write-refusals-cbh", &[]));
    let (two, classic) = (two_db.dir().join("db.2cbh"), classic_db.dir().join("db.cbh"));
    let missing = scratch.path("gone.pgn");
    let bridge = TestBridge::new(app_of([utf8.clone(), ansi.clone(), two.clone(), classic.clone(), missing]));
    let port = bridge.port;
    let (utf8_id, ansi_id) = (id_of(&utf8), id_of(&ansi));
    let generation = string_member(&ready_row(port, &utf8_id), "generation").to_string();
    let ansi_generation = string_member(&ready_row(port, &ansi_id), "generation").to_string();
    let list = get_reply(port, "/v1/databases");
    let rows = objects(&list.body, "databases");
    assert_eq!(rows.len(), 5, "{}", list.body);
    for row in &rows {
        let pgn = string_member(row, "format") == "pgn" && string_member(row, "state") == "ready";
        assert_eq!(member(row, "writable"), pgn.to_string(), "{row}");
    }
    let code = |r: &Reply| string_member(member(&r.body, "error"), "code").to_string();
    let games = |id: &str| format!("/v1/databases/{id}/games");

    // ChessBase's own formats are read-only.
    for path in [&two, &classic] {
        let id = id_of(path);
        let g = string_member(&ready_row(port, &id), "generation").to_string();
        for (method, at, body) in [("POST", games(&id), GAME_1), ("PUT", format!("{}/1", games(&id)), GAME_1)] {
            let r = write(port, method, &at, Some(&g), body);
            assert_eq!((r.status, code(&r)), (409, "read_only".into()), "{method} {}", path.display());
        }
        let r = write(port, "DELETE", &format!("{}/1", games(&id)), Some(&g), "");
        assert_eq!((r.status, code(&r)), (409, "read_only".into()));
    }

    let r = write(port, "POST", &games(&utf8_id), None, GAME_1);
    assert_eq!((r.status, code(&r)), (428, "precondition_required".into()), "{}", r.body);
    let r = write(port, "DELETE", &format!("{}/1", games(&utf8_id)), None, "");
    assert_eq!(r.status, 428);
    let r = write(port, "POST", &games(&utf8_id), Some("0000000000000000"), GAME_1);
    assert_eq!((r.status, code(&r)), (409, "generation_changed".into()), "{}", r.body);
    let r = write(port, "PUT", &format!("{}/1", games(&utf8_id)), Some("0000000000000000"), GAME_1);
    assert_eq!((r.status, code(&r)), (409, "generation_changed".into()));

    for body in [
        "",
        "  \n",
        "{only a comment}",
        "1. e4 e5 1-0\n\n1. d4 d5 0-1",
        "[Event \"a\"]\n[Event \"b\"]",
        "1. e4 e5 2. Ke3 *",
        // What the reader would pass over: an empty FEN, a token longer
        // than it keeps, bytes that make no PGN element.
        "[FEN \"\"]\n1. e4 *",
        "[Event \"truncated\"]\n1. e4 0000000000000000GARBAGE *",
        "1. e4 ½ *",
    ] {
        for (method, at) in [("POST", games(&utf8_id)), ("PUT", format!("{}/1", games(&utf8_id)))] {
            let r = write(port, method, &at, Some(&generation), body);
            assert_eq!((r.status, code(&r)), (400, "bad_request".into()), "{method} {body:?}: {}", r.body);
            assert_eq!(string_member(member(&r.body, "error"), "parameter"), "body");
        }
    }
    for number in ["2", "0", "x", "4294967296"] {
        let at = format!("{}/{number}", games(&utf8_id));
        assert_eq!(write(port, "PUT", &at, Some(&generation), GAME_1).status, 404, "{number}");
        assert_eq!(write(port, "DELETE", &at, Some(&generation), "").status, 404, "{number}");
    }

    // The ANSI file cannot hold Cyrillic; the UTF-8 file can.
    let cyrillic = "[Event \"Club\"]\n[White \"Таль, Михаил\"]\n\n1. e4 *";
    let r = write(port, "POST", &games(&ansi_id), Some(&ansi_generation), cyrillic);
    assert_eq!((r.status, code(&r)), (422, "unencodable".into()), "{}", r.body);
    assert_eq!(string_member(member(&r.body, "error"), "character"), "Т");

    // A body only where a game is written, and at most 4 MiB.
    let r = write(port, "DELETE", &format!("{}/1", games(&utf8_id)), Some(&generation), GAME_1);
    assert_eq!((r.status, code(&r)), (413, "body_not_allowed".into()), "{}", r.body);
    let head = format!(
        "POST {} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nIf-Match: \"{generation}\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        games(&utf8_id),
        (4 << 20) + 1
    );
    let r = send(port, &head);
    assert_eq!((r.status, code(&r)), (413, "body_too_large".into()), "{}", r.body);

    // A file with the read-only attribute is read-only.
    let mut permissions = std::fs::metadata(&utf8).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&utf8, permissions.clone()).unwrap();
    let row = ready_row(port, &utf8_id);
    assert_eq!(member(&row, "writable"), "false", "{row}");
    let r = write(port, "POST", &games(&utf8_id), Some(&generation), GAME_1);
    assert_eq!((r.status, code(&r)), (409, "read_only".into()), "{}", r.body);
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(&utf8, permissions).unwrap();

    assert_eq!(std::fs::read(&utf8).unwrap(), utf8_text);
    assert_eq!(std::fs::read(&ansi).unwrap(), ansi_text);
    // Then a write with the generation read goes through.
    let r = write(port, "POST", &games(&utf8_id), Some(&generation), GAME_1);
    assert_eq!(r.status, 201, "{}", r.body);
}

/// The text of game `n` of database `id`, as `GET` serves it.
fn served(port: u16, id: &str, n: u32) -> String {
    let r = get_reply(port, &format!("/v1/databases/{id}/games/{n}"));
    assert_eq!(r.status, 200, "{}", r.body);
    member(&r.body, "pgn").to_string()
}

/// A save leaves the games next to it as they were, and adds or removes
/// exactly one game. One the reader would join to a neighbour, because the
/// neighbour or the game lacks its result or holds tags alone, is refused
/// with `422 games_would_join`, and so is a removal that would join the games
/// on either side; a game whose comment is not closed is `400`; a game on a
/// line shared with the next is kept apart from it by a line end.
#[test]
fn neighbours_stay_as_they_were() {
    let scratch = Scratch::new("neighbours");
    let cases = [
        // A game without tags would continue a last game without a result.
        ("no-result", "1. e4\n"),
        // A replacement without a result would take the next game, which has
        // no tags.
        ("tagless-next", "[Event \"first\"]\n1. e4 *\n\n1. d4 *\n\n"),
        // Two games on one line.
        ("one-line", "[Event \"a\"] 1. e4 * [Event \"b\"] 1. d4 *\n"),
        // Removing the middle game would join the other two.
        ("join-on-delete", "1. e4\n\n[Event \"b\"]\n1. d4 *\n\n1. c4 *\n"),
        // A game of tags alone takes the next game's tags unless one repeats.
        ("tags-alone", "[Event \"a\"]\n\n[Event \"b\"]\n1. e4 *\n"),
    ];
    let paths: Vec<PathBuf> = cases.iter().map(|(name, _)| scratch.path(&format!("{name}.pgn"))).collect();
    for ((_, text), path) in cases.iter().zip(&paths) {
        std::fs::write(path, text).unwrap();
    }
    let bridge = TestBridge::new(app_of(paths.clone()));
    let port = bridge.port;
    let code = |r: &Reply| string_member(member(&r.body, "error"), "code").to_string();
    // Refused: the file stays as it was, and so does its list.
    let refused = |path: &Path, method: &str, at: &str, body: &str, status: u16, expected: &str| {
        let id = id_of(path);
        let before = std::fs::read(path).unwrap();
        let row = ready_row(port, &id);
        let generation = string_member(&row, "generation").to_string();
        let r = write(port, method, &format!("/v1/databases/{id}/games{at}"), Some(&generation), body);
        assert_eq!((r.status, code(&r)), (status, expected.to_string()), "{method} {body:?}: {}", r.body);
        assert_eq!(std::fs::read(path).unwrap(), before, "{method} {body:?}");
        assert_eq!(row_now(port, &id), row);
    };
    // Written: the answer and the games then served.
    let written = |path: &Path, method: &str, at: &str, body: &str, status: u16, games: &[&str]| {
        let id = id_of(path);
        let generation = string_member(&ready_row(port, &id), "generation").to_string();
        let r = write(port, method, &format!("/v1/databases/{id}/games{at}"), Some(&generation), body);
        assert_eq!(r.status, status, "{method} {body:?}: {}", r.body);
        let row = row_now(port, &id);
        assert_eq!(member(&row, "records"), games.len().to_string(), "{method} {body:?}: {row}");
        for (n, game) in games.iter().enumerate() {
            assert_eq!(
                served(port, &id, n as u32 + 1),
                bridge::json::string(&format!("{game}\n")),
                "{method} {body:?}"
            );
        }
    };
    let joins = "games_would_join";

    let path = &paths[0];
    refused(path, "POST", "", "1. d4 *", 422, joins);
    written(path, "POST", "", "[Event \"b\"]\n1. d4 *", 201, &["1. e4", "[Event \"b\"]\n1. d4 *"]);

    let path = &paths[1];
    refused(path, "PUT", "/1", "[Event \"x\"]\n1. c4", 422, joins);
    refused(path, "PUT", "/1", "[Event \"new\"]\n1. c4 * {open", 400, "bad_request");
    written(path, "PUT", "/1", "[Event \"x\"]\n1. c4 *", 200, &["[Event \"x\"]\n1. c4 *", "1. d4 *"]);

    // A comment to the end of the line would take the next game: a line end
    // is put after the replacement.
    let path = &paths[2];
    written(
        path,
        "PUT",
        "/1",
        "[Event \"x\"] 1. c4 * ; a note",
        200,
        &["[Event \"x\"] 1. c4 * ; a note", "[Event \"b\"] 1. d4 *"],
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "[Event \"x\"] 1. c4 * ; a note\n [Event \"b\"] 1. d4 *\n");

    let path = &paths[3];
    refused(path, "DELETE", "/2", "", 422, joins);
    written(path, "DELETE", "/3", "", 200, &["1. e4", "[Event \"b\"]\n1. d4 *"]);

    let path = &paths[4];
    refused(path, "PUT", "/2", "[White \"x\"]\n1. d4 *", 422, joins);
    written(path, "PUT", "/2", "[Event \"c\"]\n1. d4 *", 200, &["[Event \"a\"]", "[Event \"c\"]\n1. d4 *"]);
}

/// A file another program changed since the client read it is a conflict
/// at once: the write answers `409 generation_changed` from the file's
/// metadata, before the changed file is read for its index, and writes
/// nothing.
#[test]
fn a_file_changed_elsewhere_is_a_conflict() {
    let scratch = Scratch::new("changed");
    let path = scratch.path("games.pgn");
    std::fs::write(&path, format!("{GAME_1}\n\n")).unwrap();
    let bridge = TestBridge::new(app_of([path.clone()]));
    let id = id_of(&path);
    let generation = string_member(&ready_row(bridge.port, &id), "generation").to_string();
    // ChessBase saves a game into the file.
    let changed = format!("{GAME_1}\n\n{GAME_3}\n\n");
    std::fs::write(&path, &changed).unwrap();
    let games = format!("/v1/databases/{id}/games");
    for (method, at, body) in
        [("POST", games.clone(), NEW), ("PUT", format!("{games}/1"), NEW), ("DELETE", format!("{games}/1"), "")]
    {
        let r = write(bridge.port, method, &at, Some(&generation), body);
        assert_eq!(r.status, 409, "{method}: {}", r.body);
        assert!(r.body.contains(r#""code":"generation_changed""#), "{method}: {}", r.body);
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), changed);
    // With the generation read again, the write goes through.
    let generation = string_member(&ready_row(bridge.port, &id), "generation").to_string();
    assert_eq!(write(bridge.port, "POST", &games, Some(&generation), NEW).status, 201);
}

/// A replace makes its temporary file new: a link already of that name,
/// which the bridge did not make, is refused with `500 write_failed`, and
/// neither the file it links to, nor the link, nor the PGN file changes.
#[cfg(unix)]
#[test]
fn a_link_in_the_place_of_the_temporary_file_is_left_alone() {
    let scratch = Scratch::new("temp-link");
    let path = scratch.path("games.pgn");
    let before = format!("{GAME_1}\n\n{GAME_2}\n\n");
    std::fs::write(&path, &before).unwrap();
    let bridge = TestBridge::new(app_of([path.clone()]));
    let id = id_of(&path);
    let generation = string_member(&ready_row(bridge.port, &id), "generation").to_string();
    let other = scratch.path("unrelated.txt");
    std::fs::write(&other, "not a PGN file").unwrap();
    let temp = scratch.path("games.pgn.oschess-tmp");
    std::os::unix::fs::symlink(&other, &temp).unwrap();
    let r = write(bridge.port, "PUT", &format!("/v1/databases/{id}/games/1"), Some(&generation), NEW);
    assert_eq!(r.status, 500, "{}", r.body);
    assert!(r.body.contains(r#""code":"write_failed""#), "{}", r.body);
    assert_eq!(std::fs::read_to_string(&other).unwrap(), "not a PGN file");
    assert!(std::fs::symlink_metadata(&temp).unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    // Nor does a start remove it: it is no file the bridge made.
    drop(bridge);
    let bridge = TestBridge::new(app_of([path.clone()]));
    assert!(std::fs::symlink_metadata(&temp).is_ok());
    drop(bridge);
}

/// A PGN path that links to the file is written where it links: the link
/// stays a link to the new file, and a temporary file left beside the file
/// it links to goes at start.
#[cfg(unix)]
#[test]
fn a_linked_pgn_file_is_written_where_it_links() {
    let scratch = Scratch::new("pgn-link");
    let real = scratch.path("real.pgn");
    let link = scratch.path("link.pgn");
    std::fs::write(&real, format!("{GAME_1}\n\n{GAME_2}\n\n")).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    std::fs::write(scratch.path("real.pgn.oschess-tmp"), "a crash's leftover").unwrap();
    let bridge = TestBridge::new(app_of([link.clone()]));
    assert!(!scratch.path("real.pgn.oschess-tmp").exists(), "the leftover beside the linked file goes");
    let id = id_of(&link);
    let generation = string_member(&ready_row(bridge.port, &id), "generation").to_string();
    let r = write(bridge.port, "PUT", &format!("/v1/databases/{id}/games/2"), Some(&generation), REPLACEMENT);
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link stays");
    assert_eq!(std::fs::read_to_string(&real).unwrap(), format!("{GAME_1}\n\n{REPLACEMENT}\n\n"));
    let row = row_now(bridge.port, &id);
    assert_eq!(
        (string_member(&row, "state"), string_member(&row, "generation")),
        ("ready", string_member(&r.body, "generation"))
    );
}

/// The temporary file a replace or a removal writes beside the PGN file, left
/// by a bridge that stopped before it was renamed, goes when the file is next
/// listed; the PGN file stays as it is.
#[test]
fn a_leftover_temporary_file_goes_at_start() {
    let scratch = Scratch::new("leftover");
    let path = scratch.path("games.pgn");
    let temp = scratch.path("games.pgn.oschess-tmp");
    std::fs::write(&path, GAME_1).unwrap();
    std::fs::write(&temp, "half a file").unwrap();
    let other = scratch.path("other.pgn.oschess-tmp");
    std::fs::write(&other, "not the bridge's to remove").unwrap();
    let bridge = TestBridge::new(app_of([path.clone()]));
    assert!(!temp.exists(), "the leftover is removed");
    assert!(other.exists(), "a file beside a PGN file the bridge does not list stays");
    assert_eq!(std::fs::read(&path).unwrap(), GAME_1.as_bytes());
    ready_row(bridge.port, &id_of(&path));
}

/// A file another program holds without sharing writes and deletes, as
/// ChessBase holds a database it has open, refuses every write with
/// `409 file_busy`, and stays as it was.
#[cfg(windows)]
#[test]
fn a_file_held_elsewhere_is_busy() {
    use std::os::windows::fs::OpenOptionsExt;
    let scratch = Scratch::new("busy");
    let path = scratch.path("games.pgn");
    let before = file_text(&format!("{GAME_1}\n\n{GAME_2}\n"), "\r\n", true);
    std::fs::write(&path, &before).unwrap();
    let bridge = TestBridge::new(app_of([path.clone()]));
    let id = id_of(&path);
    let generation = string_member(&ready_row(bridge.port, &id), "generation").to_string();
    // FILE_SHARE_READ alone: no other program may write it or delete it.
    let held = std::fs::OpenOptions::new().read(true).share_mode(1).open(&path).unwrap();
    let games = format!("/v1/databases/{id}/games");
    for (method, at, body) in
        [("POST", games.clone(), NEW), ("PUT", format!("{games}/1"), NEW), ("DELETE", format!("{games}/2"), "")]
    {
        let r = write(bridge.port, method, &at, Some(&generation), body);
        assert_eq!(r.status, 409, "{method}: {}", r.body);
        assert!(r.body.contains(r#""code":"file_busy""#), "{method}: {}", r.body);
    }
    drop(held);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(!scratch.path("games.pgn.oschess-tmp").exists());
    let r = write(bridge.port, "DELETE", &format!("{games}/2"), Some(&generation), "");
    assert_eq!(r.status, 200, "{}", r.body);
}
