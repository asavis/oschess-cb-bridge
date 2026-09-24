//! `cbtool` on the command line; it never writes over the database it is reading.

use std::path::Path;
use std::process::Command;

use cbformat::fixture::{Builder, DbItems, TempDb, annotations, lid_header, quiet, text};
use cbformat::movetable::{self, Color, Piece};
use cbformat::v2::language;

/// A one-game database (1.e4) with an empty entity file.
fn fixture(name: &str) -> TempDb {
    fixture_with(name, 1, None)
}

/// `games` games, all 1.e4 and all sharing one move record and one annotation
/// record; with `player_name`, every game's white and black is one player of
/// that name.
fn fixture_with(name: &str, games: usize, player_name: Option<&[u8]>) -> TempDb {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[movetable::MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), movetable::END_OF_LINE]);
    let comment = b.annotations(&annotations(&[(0, vec![text(false, language::ENGLISH, "best by test")])]));
    for _ in 0..games {
        b.annotated_game(e4, comment);
    }
    if let Some(last) = player_name {
        let mut player = Vec::new();
        player.extend((last.len() as i32).to_le_bytes());
        player.extend(last);
        player.extend(0i32.to_le_bytes()); // no first name
        let mut lid = lid_header((4 + player.len()).max(1024) as i32, 1);
        lid.extend((player.len() as i32).to_le_bytes());
        lid.extend(&player);
        b.lid(lid);
    }
    b.write(&format!("cbtool-{name}"))
}

/// The contents of the database's files, and of those beside it that exist.
fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("db."))
        .collect();
    names.sort();
    names.into_iter().map(|f| (f.clone(), std::fs::read(dir.join(&f)).unwrap())).collect()
}

fn pgn(dir: &Path, out: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_cbtool"))
        .arg("pgn")
        .arg(dir.join("db.2cbh"))
        .arg("--out")
        .arg(out)
        .output()
        .unwrap()
}

#[test]
fn export_works() {
    let f = fixture("ok");
    let out = f.dir().join("games.pgn");
    std::fs::write(&out, "old contents").unwrap();
    let r = pgn(f.dir(), &out);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.contains("1. e4 {best by test} 1-0"), "{text}");
}

#[test]
fn export_refuses_to_overwrite_the_database() {
    let f = fixture("refuse");
    std::fs::write(f.dir().join("db.ini"), "[settings]").unwrap();
    let before = snapshot(f.dir());
    std::fs::hard_link(f.dir().join("db.2cbg"), f.dir().join("export.pgn")).unwrap();
    std::fs::hard_link(f.dir().join("db.ini"), f.dir().join("settings.pgn")).unwrap();
    let targets = [
        f.dir().join("db.2cbg"),
        f.dir().join("db.2cbh"),
        f.dir().join("db.2lid"),
        f.dir().join("db.2cba"),
        f.dir().join(".").join("db.2cbg"),
        f.dir().join("export.pgn"),
        f.dir().join("db.ini"),
        f.dir().join("settings.pgn"),
    ];
    for out in targets {
        let r = pgn(f.dir(), &out);
        assert!(!r.status.success(), "accepted {}", out.display());
        assert!(String::from_utf8_lossy(&r.stderr).contains("refusing"), "{}", String::from_utf8_lossy(&r.stderr));
        assert_eq!(snapshot(f.dir()), before, "input changed by --out {}", out.display());
    }
}

/// Games that share one large record must not make the export hold their
/// rendered text in memory: 512 games naming one player with a 64 KiB name
/// render to 64 MiB from a database of under 200 KiB.
#[cfg(unix)]
#[test]
fn export_memory_is_bounded_when_games_share_a_large_record() {
    let f = fixture_with("shared-large", 512, Some(&[b'A'; 64 << 10]));
    let limit_kib = 96 * 1024;
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("ulimit -v {limit_kib} && exec \"$0\" pgn \"$1\" > /dev/null"))
        .arg(env!("CARGO_BIN_EXE_cbtool"))
        .arg(f.dir().join("db.2cbh"))
        .env("CBTOOL_THREADS", "2")
        .status()
        .unwrap();
    assert!(status.success(), "export under a {limit_kib} KiB address-space limit: {status}");
}

/// `cbtool databases` reads the list ChessBase keeps, reports what is on this
/// computer, and ignores OneDrive conflict copies.
#[test]
fn databases_lists_the_window_and_the_state_of_each_entry() {
    let f = fixture_with("databases", 3, None);
    let here = f.dir().join("db.2cbh");
    let mut list = DbItems::new();
    list.section("2cbg")
        .database(here.to_str().unwrap(), "Здесь", [0, 28, 3, 1, 1037620, 1037559])
        .database(f.dir().join("gone.2cbh").to_str().unwrap(), "Gone", [0, 28, 7, 1, 1037620, 1037559])
        .section("Databases")
        .database("sub/games.pgn", "", [0, 3, 9, 5, 1037616, 1037616]);
    std::fs::write(f.dir().join("DBItems.cbini"), list.bytes()).unwrap();
    std::fs::write(f.dir().join("DBItems-OTHER.cbini"), DbItems::new().bytes()).unwrap();
    let r = Command::new(env!("CARGO_BIN_EXE_cbtool")).arg("databases").arg(f.dir()).output().unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let out = String::from_utf8(r.stdout).unwrap();
    let rows: Vec<Vec<&str>> = out
        .lines()
        .filter(|l| l.trim_start().starts_with(char::is_numeric))
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(rows.len(), 3, "{out}");
    assert_eq!(rows[0], ["1", "2cbh", "present", "3", "3", "Здесь"]);
    assert_eq!(rows[1], ["2", "2cbh", "missing", "7", "-", "Gone"]);
    assert_eq!(rows[2], ["3", "pgn", "missing", "9", "-", "games"]);
    assert!(out.contains("ignored: 1 OneDrive conflict copy"), "{out}");
    assert!(!out.contains(f.dir().to_str().unwrap()), "stored paths are not printed: {out}");
}

/// A database whose header is here but a companion file is not is reported
/// with the companion's state and never opened: its record count stays `-`.
/// Zero-block files stand for placeholders, which needs a Unix file system.
#[cfg(unix)]
#[test]
fn databases_never_opens_a_database_with_an_offline_companion() {
    let f = fixture_with("databases-offline", 3, None);
    let mut list = DbItems::new();
    list.section("2cbg").database(f.dir().join("db.2cbh").to_str().unwrap(), "Db", [0, 28, 3, 1, 1037620, 1037559]);
    std::fs::write(f.dir().join("DBItems.cbini"), list.bytes()).unwrap();
    let row = || {
        let r = Command::new(env!("CARGO_BIN_EXE_cbtool")).arg("databases").arg(f.dir()).output().unwrap();
        assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
        let out = String::from_utf8(r.stdout).unwrap();
        let row: Vec<String> = out
            .lines()
            .find(|l| l.trim_start().starts_with('1'))
            .unwrap_or_else(|| panic!("{out}"))
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        row
    };
    assert_eq!(row(), ["1", "2cbh", "present", "3", "3", "Db"]);
    // Replace a file by one of the same length with no blocks on disk.
    let hollow = |name: &str| {
        let p = f.dir().join(name);
        let len = std::fs::metadata(&p).unwrap().len();
        std::fs::remove_file(&p).unwrap();
        std::fs::File::create(&p).unwrap().set_len(len.max(1)).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(std::fs::metadata(&p).unwrap().blocks(), 0, "{name} is not sparse on this file system");
    };
    let saved_cba = std::fs::read(f.dir().join("db.2cba")).unwrap();
    hollow("db.2cba");
    assert_eq!(row(), ["1", "2cbh", "cloud-only?", "3", "-", "Db"]);
    std::fs::write(f.dir().join("db.2cba"), &saved_cba).unwrap();
    hollow("db.2lid");
    assert_eq!(row(), ["1", "2cbh", "cloud-only?", "3", "-", "Db"]);
    std::fs::remove_file(f.dir().join("db.2cbg")).unwrap();
    assert_eq!(row(), ["1", "2cbh", "missing", "3", "-", "Db"]);
}

/// A companion that is not a regular file is reported and never opened:
/// opening a pipe would block until a writer appears. The listing must finish
/// promptly with the database unreadable.
#[cfg(unix)]
#[test]
fn databases_never_opens_a_pipe_or_a_directory() {
    let f = fixture_with("databases-fifo", 3, None);
    let mut list = DbItems::new();
    list.section("2cbg").database(f.dir().join("db.2cbh").to_str().unwrap(), "Db", [0, 28, 3, 1, 1037620, 1037559]);
    std::fs::write(f.dir().join("DBItems.cbini"), list.bytes()).unwrap();
    let row = || {
        let mut child = Command::new(env!("CARGO_BIN_EXE_cbtool"))
            .arg("databases")
            .arg(f.dir())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        while child.try_wait().unwrap().is_none() {
            if started.elapsed() > std::time::Duration::from_secs(10) {
                child.kill().unwrap();
                panic!("cbtool databases did not finish within 10 s");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let mut out = String::new();
        std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut out).unwrap();
        out.lines()
            .find(|l| l.trim_start().starts_with('1'))
            .unwrap_or_else(|| panic!("{out}"))
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let cbg = f.dir().join("db.2cbg");
    std::fs::remove_file(&cbg).unwrap();
    let made = Command::new("mkfifo").arg(&cbg).status().unwrap();
    assert!(made.success(), "mkfifo failed");
    assert_eq!(row(), ["1", "2cbh", "unreadable", "3", "-", "Db"]);
    std::fs::remove_file(&cbg).unwrap();
    std::fs::create_dir(&cbg).unwrap();
    assert_eq!(row(), ["1", "2cbh", "unreadable", "3", "-", "Db"]);
}

/// A classic database is verified with the same report as a 2CBH one.
#[test]
fn verify_and_info_read_classic_databases() {
    use cbformat::fixture_cbh::{Builder, Tok, encode, move_record};
    let mut b = Builder::new();
    for moves in [&["e2e4", "e7e5"][..], &["d2d4", "--", "c2c4"][..]] {
        let mut toks: Vec<Tok<'_>> = moves.iter().map(|m| Tok::Mv(m)).collect();
        toks.push(Tok::End);
        b.game(&move_record(0, None, None, &encode(&chesscore::Board::startpos(), &toks, 0, false)));
    }
    let f = b.write("cli-classic");
    let run = |cmd: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_cbtool")).arg(cmd).arg(f.dir().join("db.cbh")).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    let v = run("verify");
    for line in ["games              2", "null moves         1", "all plies          5", "failures           0"] {
        assert!(v.contains(line), "{v}");
    }
    assert!(run("info").contains("players        2"));
}

/// A classic database exports and verifies its annotations like a 2CBH one: a
/// comment on a move the game has is written, one on no move fails both, and
/// the export never writes over one of the database's own files.
#[test]
fn classic_annotations_export_and_verify() {
    use cbformat::fixture_cbh::{Builder, Tok, annotation_record, encode, move_record};
    let e4 = move_record(0, None, None, &encode(&chesscore::Board::startpos(), &[Tok::Mv("e2e4"), Tok::End], 0, false));
    let mut b = Builder::new();
    b.game(&e4);
    b.annotations(&annotation_record(1, &[(0, 0x02, b"\x00\x2afine"), (0, 0x03, &[1])]));
    b.game(&e4);
    b.annotations(&annotation_record(2, &[(1, 0x02, b"\x00\x2aon no move")]));
    b.game(&e4);
    let f = b.write("cli-classic-annotations");
    let db = f.dir().join("db.cbh");
    let verify = Command::new(env!("CARGO_BIN_EXE_cbtool")).arg("verify").arg(&db).output().unwrap();
    let text = String::from_utf8_lossy(&verify.stdout);
    assert_eq!(verify.status.code(), Some(1), "{text}");
    assert!(text.contains("annotated          2") && text.contains("incomplete       0"), "{text}");
    assert!(text.contains("failures           1") && text.contains("game 2: annotations:"), "{text}");

    let out = f.dir().join("games.pgn");
    let export =
        Command::new(env!("CARGO_BIN_EXE_cbtool")).arg("pgn").arg(&db).arg("--out").arg(&out).output().unwrap();
    assert!(!export.status.success());
    assert!(String::from_utf8_lossy(&export.stderr).contains("game 2:"), "{}", String::from_utf8_lossy(&export.stderr));
    let pgn = std::fs::read_to_string(&out).unwrap();
    assert!(pgn.contains("[White \"Morphy\"]") && pgn.contains("[Event \"Paris\"]"), "{pgn}");
    assert!(pgn.contains("1. e4 $1 {fine} 1-0") && pgn.contains("\n1. e4 1-0"), "{pgn}");

    let one = Command::new(env!("CARGO_BIN_EXE_cbtool")).arg("pgn").arg(f.base()).arg("3").output().unwrap();
    assert!(one.status.success(), "{}", String::from_utf8_lossy(&one.stderr));
    assert!(String::from_utf8_lossy(&one.stdout).ends_with("\n1. e4 1-0\n\n"));

    for file in ["db.cbh", "db.cbg", "db.cba", "db.cbp"] {
        let refused = Command::new(env!("CARGO_BIN_EXE_cbtool"))
            .arg("pgn")
            .arg(&db)
            .arg("--out")
            .arg(f.dir().join(file))
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&refused.stderr).contains("refusing"), "{file}");
    }
}

/// Every file of a classic database and those beside it are refused as the
/// export's output, by name and through a hard link, and none changes: the
/// media manifest `.cbm`, a search booster, the settings among them.
#[test]
fn classic_export_refuses_every_companion() {
    use cbformat::fixture_cbh::{Builder, Tok, encode, move_record};
    let e4 = move_record(0, None, None, &encode(&chesscore::Board::startpos(), &[Tok::Mv("e2e4"), Tok::End], 0, false));
    let mut b = Builder::new();
    b.game(&e4);
    let f = b.write("cli-classic-companions");
    for extra in ["db.cbm", "db.cit", "db.cbgi", "db.flags", "db.ini", "db.cko"] {
        std::fs::write(f.dir().join(extra), extra.as_bytes()).unwrap();
    }
    let before = snapshot(f.dir());
    std::fs::hard_link(f.dir().join("db.cbm"), f.dir().join("media.pgn")).unwrap();
    std::fs::hard_link(f.dir().join("db.cba"), f.dir().join("notes.pgn")).unwrap();
    let names = ["db.cbm", "db.cit", "db.cbgi", "db.flags", "db.ini", "db.cko", "db.cbj", "media.pgn", "notes.pgn"];
    for name in names {
        let out = f.dir().join(name);
        let existed = out.exists();
        let r = Command::new(env!("CARGO_BIN_EXE_cbtool"))
            .arg("pgn")
            .arg(f.dir().join("db.cbh"))
            .arg("--out")
            .arg(&out)
            .output()
            .unwrap();
        if existed {
            assert!(!r.status.success(), "accepted {name}");
            assert!(String::from_utf8_lossy(&r.stderr).contains("refusing"), "{name}");
        } else {
            // A companion that does not exist yet is nothing to overwrite.
            assert!(r.status.success(), "{name}: {}", String::from_utf8_lossy(&r.stderr));
            std::fs::remove_file(&out).unwrap();
        }
        assert_eq!(snapshot(f.dir()), before, "input changed by --out {name}");
    }
}

/// An annotation record whose head claims a gigabyte, in a sparse `.cba`
/// that long, is refused before it is read: `verify` and `pgn` report the game
/// within a 256 MiB address space instead of aborting.
#[cfg(unix)]
#[test]
fn a_huge_annotation_record_is_refused_before_it_is_read() {
    use cbformat::fixture_cbh::{Builder, Tok, encode, move_record};
    let e4 = move_record(0, None, None, &encode(&chesscore::Board::startpos(), &[Tok::Mv("e2e4"), Tok::End], 0, false));
    let claimed: u32 = 0x4000_0000;
    let mut head = vec![0, 0, 1, 1, 0, 0x0e, 0x0e, 0, 0, 1];
    head.extend(claimed.to_be_bytes());
    let mut b = Builder::new();
    b.game(&e4);
    b.annotations(&head);
    b.game(&e4);
    let f = b.write("cli-classic-huge-annotations");
    std::fs::File::options()
        .write(true)
        .open(f.dir().join("db.cba"))
        .unwrap()
        .set_len(26 + u64::from(claimed))
        .unwrap();
    let run = |args: &[&str]| {
        Command::new("sh")
            .arg("-c")
            .arg("ulimit -v 262144 && exec \"$0\" \"$@\"")
            .arg(env!("CARGO_BIN_EXE_cbtool"))
            .args(args)
            .env("CBTOOL_THREADS", "2")
            .output()
            .unwrap()
    };
    let db = f.dir().join("db.cbh");
    let db = db.to_str().unwrap();
    let verify = run(&["verify", db]);
    let text = String::from_utf8_lossy(&verify.stdout);
    assert_eq!(verify.status.code(), Some(1), "{text}{}", String::from_utf8_lossy(&verify.stderr));
    assert!(text.contains("failures           1") && text.contains("game 1: annotations:"), "{text}");
    assert!(text.contains("over the limit"), "{text}");
    let pgn = run(&["pgn", db]);
    let err = String::from_utf8_lossy(&pgn.stderr);
    assert_eq!(pgn.status.code(), Some(1), "{err}");
    assert!(err.contains("game 1:") && err.contains("over the limit"), "{err}");
    assert!(String::from_utf8_lossy(&pgn.stdout).contains("\n1. e4 1-0"), "game 2 is exported");
}

/// The review's hostile classic record, a million nested variations in one
/// 2 MB game, is a verify failure within a 256 MiB address space, not an
/// allocation abort.
#[cfg(unix)]
#[test]
fn verify_bounds_the_memory_of_hostile_nesting() {
    use cbformat::fixture_cbh::{Builder, move_record};
    let stream: Vec<u8> = (0..1_000_000u32).flat_map(|n| [(0xdc + n) as u8, (0xaa + n) as u8]).collect();
    let mut b = Builder::new();
    b.game(&move_record(0, None, None, &stream));
    let f = b.write("cli-hostile-nesting");
    let out = Command::new("sh")
        .arg("-c")
        .arg("ulimit -v 262144 && exec \"$0\" verify \"$1\"")
        .arg(env!("CARGO_BIN_EXE_cbtool"))
        .arg(f.dir().join("db.cbh"))
        .env("CBTOOL_THREADS", "1")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("failures           1") && text.contains("nested deeper than 1024"), "{text}");
}

/// An annotation on a move the game does not have fails `verify` and the
/// export of that game, instead of vanishing from the PGN.
#[test]
fn annotations_on_no_move_fail_verify_and_export() {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[movetable::MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), movetable::END_OF_LINE]);
    let good = b.annotations(&annotations(&[(0, vec![text(false, language::ENGLISH, "fine")])]));
    let bad = b.annotations(&annotations(&[(1, vec![text(false, language::ENGLISH, "on no move")])]));
    b.annotated_game(e4, good);
    b.annotated_game(e4, bad);
    let f = b.write("cbtool-no-move");
    let verify =
        Command::new(env!("CARGO_BIN_EXE_cbtool")).arg("verify").arg(f.dir().join("db.2cbh")).output().unwrap();
    let text = String::from_utf8_lossy(&verify.stdout);
    assert_eq!(verify.status.code(), Some(1), "{text}");
    assert!(text.contains("annotated          2") && text.contains("failures           1"), "{text}");
    assert!(text.contains("game 2: annotations:") && text.contains("position 1"), "{text}");
    let out = f.dir().join("games.pgn");
    let export = pgn(f.dir(), &out);
    assert!(!export.status.success());
    assert!(String::from_utf8_lossy(&export.stderr).contains("game 2:"), "{}", String::from_utf8_lossy(&export.stderr));
    assert!(std::fs::read_to_string(&out).unwrap().contains("1. e4 {fine} 1-0"));
}
