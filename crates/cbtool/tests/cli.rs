//! `cbtool` never writes over the database it is reading.

use std::path::Path;
use std::process::Command;

use cbformat::fixture::{Builder, TempDb, lid_header, quiet};
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
