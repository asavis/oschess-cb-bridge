//! `cbtool` never writes over the database it is reading.

use std::path::{Path, PathBuf};
use std::process::Command;

use cbformat::movetable::{self, Captured, Color, MoveWord, Piece};
use cbformat::v2::checksum;

struct Fixture {
    dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn e2e4() -> u16 {
    let want = MoveWord::Normal {
        color: Color::White,
        piece: Piece::Pawn,
        from: 12,
        to: 28,
        captured: Captured::Nothing,
        promotion: None,
    };
    (1..0xb12d).find(|&w| movetable::decode(w) == Some(want)).unwrap()
}

/// A one-game database (1.e4) with an empty entity file.
fn fixture(name: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!("cbtool-cli-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cbh = vec![0u8; 384];
    cbh[0x0a..0x0c].copy_from_slice(&192i16.to_le_bytes());
    cbh[0x0d] = 5;
    cbh[192] = 1;
    cbh[194] = 1;
    cbh[195] = 1;
    cbh[192 + 8..192 + 16].copy_from_slice(&12i64.to_le_bytes());
    cbh[192 + 0x58] = 2;
    let content: Vec<u8> =
        [movetable::MOVES, e2e4(), movetable::END_OF_LINE].iter().flat_map(|w| w.to_le_bytes()).collect();
    let mut rec = vec![0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11];
    rec.extend((content.len() as i32).to_le_bytes());
    rec.extend(0i32.to_le_bytes());
    rec.extend(checksum(&content).to_be_bytes());
    rec.extend(1u16.to_le_bytes());
    rec.extend(&content);
    rec.extend((content.len() as i64 + 34).to_le_bytes());
    let mut cbg = Vec::new();
    cbg.extend((12 + rec.len() as i64).to_le_bytes());
    cbg.extend(12i16.to_le_bytes());
    cbg.extend([0, 5]);
    cbg.extend(rec);
    let mut lid = Vec::new();
    lid.extend(184i32.to_be_bytes());
    lid.extend(1i32.to_be_bytes());
    lid.extend(1024i32.to_be_bytes());
    lid.extend(0i64.to_be_bytes());
    lid.extend((-1i64).to_be_bytes());
    lid.resize(184, 0);
    std::fs::write(dir.join("db.2cbh"), cbh).unwrap();
    std::fs::write(dir.join("db.2cbg"), cbg).unwrap();
    std::fs::write(dir.join("db.2lid"), lid).unwrap();
    std::fs::write(dir.join("db.2cba"), b"annotations").unwrap();
    Fixture { dir }
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
    let out = f.dir.join("games.pgn");
    std::fs::write(&out, "old contents").unwrap();
    let r = pgn(&f.dir, &out);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.contains("1. e4 1-0"), "{text}");
}

#[test]
fn export_refuses_to_overwrite_the_database() {
    let f = fixture("refuse");
    let before = snapshot(&f.dir);
    std::fs::hard_link(f.dir.join("db.2cbg"), f.dir.join("export.pgn")).unwrap();
    let targets = [
        f.dir.join("db.2cbg"),
        f.dir.join("db.2cbh"),
        f.dir.join("db.2lid"),
        f.dir.join("db.2cba"),
        f.dir.join(".").join("db.2cbg"),
        f.dir.join("export.pgn"),
    ];
    for out in targets {
        let r = pgn(&f.dir, &out);
        assert!(!r.status.success(), "accepted {}", out.display());
        assert!(String::from_utf8_lossy(&r.stderr).contains("refusing"), "{}", String::from_utf8_lossy(&r.stderr));
        assert_eq!(snapshot(&f.dir), before, "input changed by --out {}", out.display());
    }
}
