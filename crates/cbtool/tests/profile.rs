//! `cbtool profile` prints timings and counts only (#83): a flow that fails
//! shows its status and the bridge's code, never the database's name or a
//! path, and the command then exits with status 1.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use cbformat::fixture::{Builder, quiet};
use cbformat::movetable::{self, Color, Piece};

#[test]
fn failures_name_no_database_and_no_path() {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[movetable::MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), movetable::END_OF_LINE]);
    b.game(e4);
    b.game(e4);
    let db = b.write("cbtool-profile-private");
    // The database under a private name, in a folder of that name.
    let dir = db.dir().join("PRIVATE_SENTINEL_FOLDER");
    std::fs::create_dir_all(&dir).unwrap();
    for ext in cbformat::v2::EXTENSIONS {
        let from = db.dir().join(format!("db{ext}"));
        if from.exists() {
            std::fs::copy(&from, dir.join(format!("PRIVATE_SENTINEL{ext}"))).unwrap();
        }
    }
    // An index folder the bridge cannot write, so that its build fails, and an
    // engine that does not exist.
    let index = db.dir().join("PRIVATE_SENTINEL_INDEX");
    std::fs::create_dir_all(&index).unwrap();
    std::fs::set_permissions(&index, std::fs::Permissions::from_mode(0o555)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_cbtool"))
        .arg("profile")
        .arg(dir.join("PRIVATE_SENTINEL.2cbh"))
        .arg("--index")
        .arg(&index)
        .arg("--engine")
        .arg(dir.join("PRIVATE_SENTINEL_ENGINE"))
        .output()
        .unwrap();
    std::fs::set_permissions(&index, std::fs::Permissions::from_mode(0o755)).unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("FAILED"), "{text}");
    assert!(text.contains("engine"), "{text}");
    assert!(!text.contains("PRIVATE_SENTINEL"), "{text}");
    assert!(!text.contains(db.dir().to_str().unwrap()), "{text}");
}
