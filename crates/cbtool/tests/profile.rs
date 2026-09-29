//! `cbtool profile` prints timings and counts only (#83): a flow that fails
//! shows its status and the bridge's code, never the database's name or a
//! path, and the command then exits with status 1. Every index it has built
//! is in its `--index` folder (#118).

use std::process::Command;

use cbformat::fixture::pgn_file;

#[cfg(unix)]
#[test]
fn failures_name_no_database_and_no_path() {
    use std::os::unix::fs::PermissionsExt;

    use cbformat::fixture::{Builder, quiet};
    use cbformat::movetable::{self, Color, Piece};

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

/// A PGN file's header index is built in the `--index` folder, as the
/// position index is, and never in the bridge's data folder: a second run
/// would find it there, and its first answers would not be cold.
#[test]
fn no_index_is_built_in_the_data_folder() {
    let pgn = pgn_file("cbtool-profile-home", b"[Event \"x\"]\n\n1. e4 e5 2. Nf3 Nc6 1-0\n");
    let (home, index) = (pgn.dir().join("home"), pgn.dir().join("index"));
    std::fs::create_dir_all(&home).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_cbtool"))
        .arg("profile")
        .arg(pgn.dir().join("db.pgn"))
        .arg("--index")
        .arg(&index)
        .env("OSCHESS_BRIDGE_HOME", &home)
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let written: Vec<_> = std::fs::read_dir(&home).unwrap().map(|e| e.unwrap().path()).collect();
    assert!(written.is_empty(), "{written:?} in the data folder\n{text}");
    assert!(index.join("pgn").is_dir(), "{text}");
}

/// Every notable game the lookups name is checked against its `/games` row
/// (#144), and the count says how many are that row whole, then `year`. The
/// build of the position index is timed phase by phase (#147).
#[test]
fn notable_games_are_counted_as_whole_rows() {
    use cbformat::fixture::{Builder, quiet};
    use cbformat::movetable::{self, Color, Piece};

    let mut b = Builder::new();
    let e4 = quiet(Color::White, Piece::Pawn, "e2", "e4");
    let e5 = quiet(Color::Black, Piece::Pawn, "e7", "e5");
    let open = b.moves(1, &[movetable::MOVES, e4, e5, movetable::END_OF_LINE]);
    b.game(open)[0xbc..0xc0].copy_from_slice(&((1951i32 << 9) | (7 << 5) | 1).to_le_bytes());
    let short = b.moves(1, &[movetable::MOVES, e4, movetable::END_OF_LINE]);
    b.game(short);
    let db = b.write("cbtool-profile-rows");
    let out = Command::new(env!("CARGO_BIN_EXE_cbtool"))
        .arg("profile")
        .arg(db.dir().join("db.2cbh"))
        .arg("--index")
        .arg(db.dir().join("index"))
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    // The lookups: from the start and after 1. e4, both games; after 1... e5,
    // the first, and no move from there.
    assert!(text.contains("topGames entries with every row field: 5 of 5"), "{text}");
    for phase in ["stream pass", "tree passes", "deep passes", "index file end", "renames"] {
        assert!(text.contains(&format!("build: {phase}")), "{phase}\n{text}");
    }
    assert!(text.contains("passes 1, replay"), "{text}");
    assert!(out.status.success(), "{text}");
}

/// With `--background`, the bridge builds the position index unasked (#149):
/// the flows wait for it rather than start it, time it from the bridge's
/// start, and say of each sort and search whether the build still ran.
#[test]
fn the_background_build_is_timed_from_the_bridges_start() {
    use cbformat::fixture::{Builder, quiet};
    use cbformat::movetable::{self, Color, Piece};

    let mut b = Builder::new();
    let e4 = b.moves(1, &[movetable::MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), movetable::END_OF_LINE]);
    b.game(e4);
    b.game(e4);
    let db = b.write("cbtool-profile-background");
    // Written a while ago, so quiet from the start.
    let a_while_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
    for entry in std::fs::read_dir(db.dir()).unwrap() {
        let file = std::fs::File::options().write(true).open(entry.unwrap().path()).unwrap();
        file.set_modified(a_while_ago).unwrap();
    }
    let out = Command::new(env!("CARGO_BIN_EXE_cbtool"))
        .arg("profile")
        .arg(db.dir().join("db.2cbh"))
        .arg("--index")
        .arg(db.dir().join("index"))
        .arg("--background")
        .env("OSCHESS_BRIDGE_BACKGROUND_MODE", "background")
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("background build to ready"), "{text}");
    assert!(text.contains("from the bridge's start, no position asked, mode background; stream 2 games"), "{text}");
    assert!(!text.contains("build to first answer"), "{text}");
    for phase in ["stream pass", "tree passes", "deep passes"] {
        assert!(text.contains(&format!("build: {phase}")), "{phase}\n{text}");
    }
    let timed = |l: &&str| (l.starts_with("sort ") || l.starts_with("search ")) && !l.contains(" suggested ");
    for row in text.lines().filter(timed) {
        assert!(row.ends_with("during the build") || row.ends_with("after the build"), "{row}");
    }
    assert!(out.status.success(), "{text}");
}
