//! `cbtool` never writes over the database it is reading.

use std::path::Path;
use std::process::Command;

use cbformat::fixture::{Builder, DbItems, TempDb, lid_header, quiet};
use cbformat::movetable::{self, Color, Piece};

/// A one-game database (1.e4) with an empty entity file.
fn fixture(name: &str) -> TempDb {
    fixture_with(name, 1, None)
}

/// `games` games, all 1.e4 and all sharing one move record; with
/// `player_name`, every game's white and black is one player of that name.
fn fixture_with(name: &str, games: usize, player_name: Option<&[u8]>) -> TempDb {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[movetable::MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), movetable::END_OF_LINE]);
    for _ in 0..games {
        b.game(e4);
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
    let db = b.write(&format!("cbtool-{name}"));
    std::fs::write(db.dir().join("db.2cba"), b"annotations").unwrap();
    db
}

fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    ["db.2cbh", "db.2cbg", "db.2lid", "db.2cba"]
        .iter()
        .map(|f| (f.to_string(), std::fs::read(dir.join(f)).unwrap()))
        .collect()
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
    assert!(text.contains("1. e4 1-0"), "{text}");
}

#[test]
fn export_refuses_to_overwrite_the_database() {
    let f = fixture("refuse");
    let before = snapshot(f.dir());
    std::fs::hard_link(f.dir().join("db.2cbg"), f.dir().join("export.pgn")).unwrap();
    let targets = [
        f.dir().join("db.2cbg"),
        f.dir().join("db.2cbh"),
        f.dir().join("db.2lid"),
        f.dir().join("db.2cba"),
        f.dir().join(".").join("db.2cbg"),
        f.dir().join("export.pgn"),
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
