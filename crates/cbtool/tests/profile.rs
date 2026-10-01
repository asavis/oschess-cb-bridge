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
    // The games of the line's start are listed (#148), as many as the
    // explorer counts; the line ends before its sixth ply, and no game is
    // long enough to be sampled.
    assert!(text.contains("total = explorer's games for 1 of 1; totals 2;"), "{text}");
    for case in ["line ply 6", "line ply 20", "sampled ply 40", "sampled ply 80"] {
        assert!(text.contains(&format!("{case}, first page")), "{case}\n{text}");
    }
    assert!(text.contains("no such position"), "{text}");
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
    let row = "from the bridge's start, no position asked, mode background, giving way 500 ms at most; stream 2 games";
    assert!(text.contains(row), "{text}");
    assert!(!text.contains("build to first answer"), "{text}");
    for phase in ["stream pass", "tree passes", "deep passes"] {
        assert!(text.contains(&format!("build: {phase}")), "{phase}\n{text}");
    }
    let labels = ["before the build", "partly during the build", "during the build", "after the build"];
    for row in text.lines().filter(timed) {
        assert!(labels.iter().any(|l| row.ends_with(l)), "{row}");
    }
    assert!(out.status.success(), "{text}");
}

/// The sorts and searches of a profile, with a database written just now.
fn timed(l: &&str) -> bool {
    (l.starts_with("sort ") || l.starts_with("search ")) && !l.contains(" suggested ")
}

/// A database written just now waits out the keeper's quiet period, a minute,
/// before its build starts (#149): the sorts and searches the profile times
/// meanwhile ran beside no build, and say so. The profile is stopped once it
/// has printed them, and the bridge it started ends with it.
#[test]
fn rows_taken_before_the_background_build_starts_say_so() {
    use cbformat::fixture::{Builder, quiet};
    use cbformat::movetable::{self, Color, Piece};
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    let mut b = Builder::new();
    let e4 = b.moves(1, &[movetable::MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), movetable::END_OF_LINE]);
    b.game(e4);
    b.game(e4);
    let db = b.write("cbtool-profile-background-recent");
    let mut child = Command::new(env!("CARGO_BIN_EXE_cbtool"))
        .arg("profile")
        .arg(db.dir().join("db.2cbh"))
        .arg("--index")
        .arg(db.dir().join("index"))
        .arg("--background")
        .env("OSCHESS_BRIDGE_BACKGROUND_MODE", "background")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The flow after the searches is the games' PGN.
    let mut lines = Vec::new();
    for line in BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok) {
        let pgn = line.starts_with("pgn ");
        lines.push(line);
        if pgn {
            break;
        }
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let text = lines.join("\n");
    assert!(lines.iter().any(|l| l.starts_with("pgn ")), "{text}");
    let rows: Vec<&str> = text.lines().filter(timed).collect();
    assert!(rows.iter().any(|r| r.starts_with("search ")), "{text}");
    for row in rows {
        assert!(row.ends_with(" before the build"), "{row}");
    }
}

/// Every bridge a profile starts keeps its indexes in the one `--index`
/// folder, and sweeps it as it starts of the partial files there, which only
/// the process writing them can tell from abandoned ones (#191). So one
/// bridge runs at a time: the one before has ended when the next starts, as
/// the profile's own record says (#239). Looks at `/proc` while it runs
/// check the record against the kernel: they can miss a bridge that lived
/// between two of them, so they do not count the bridges, but they never see
/// two at once when one ran at a time, nor more bridges than ran.
#[cfg(target_os = "linux")]
#[test]
fn one_bridge_runs_at_a_time() {
    let run = watched("cbtool-profile-one-bridge", usize::MAX);
    let text = &run.text;
    assert!(run.status.success(), "{text}");
    // The first bridge, the one after it, and one for each names flow.
    let bridges = recorded(text).unwrap_or_else(|why| panic!("{why}\n{text}"));
    assert!(bridges >= 4, "{bridges} bridges\n{text}");
    assert!(run.most <= 1, "{} bridges running at once\n{text}", run.most);
    assert!(run.seen.len() <= bridges, "{:?} seen, {bridges} recorded\n{text}", run.seen);
}

/// Why the record counts the bridges, and not looks at `/proc` (#239): a
/// watcher descheduled for a short bridge's life misses it, as one did under
/// load, seeing 3 of 4 bridges. One stalled once it has seen three misses
/// the rest every time, and the record still names them all, each ended
/// before the next started.
#[cfg(target_os = "linux")]
#[test]
fn the_record_names_the_bridges_a_stalled_watcher_misses() {
    let run = watched("cbtool-profile-stalled-watcher", 3);
    let text = &run.text;
    assert!(run.status.success(), "{text}");
    assert!(run.seen.len() < 4, "{:?} seen\n{text}", run.seen);
    let bridges = recorded(text).unwrap_or_else(|why| panic!("{why}\n{text}"));
    assert!(bridges >= 4, "{bridges} bridges\n{text}");
}

/// How many bridges the profile's record in `text` names, when it names them
/// one after another: `bridge 1 started`, `bridge 1 ended`, `bridge 2
/// started` and so on, each ended before the next started.
fn recorded(text: &str) -> Result<usize, String> {
    let record: Vec<&str> = text.lines().filter(|l| l.starts_with("bridge ")).collect();
    for (i, line) in record.iter().enumerate() {
        let expected = format!("bridge {} {}", i / 2 + 1, if i % 2 == 0 { "started" } else { "ended" });
        if *line != expected {
            return Err(format!("record line {} is {line:?}, not {expected:?}", i + 1));
        }
    }
    if record.len() % 2 == 1 {
        return Err(format!("bridge {} never ended", record.len() / 2 + 1));
    }
    Ok(record.len() / 2)
}

/// The record's check counts bridges named one after another among the rows,
/// and fails a bridge that started before the one before it ended, one that
/// never ended, and one out of turn.
#[test]
fn a_record_names_its_bridges_one_after_another() {
    let rows = "flow case\nopening   bridge process start\n";
    assert_eq!(recorded(rows), Ok(0));
    let two = "bridge 1 started\nsort x\nbridge 1 ended\nbridge 2 started\nbridge 2 ended\n";
    assert_eq!(recorded(&format!("{rows}{two}")), Ok(2));
    assert!(recorded("bridge 1 started\nbridge 2 started\nbridge 1 ended\nbridge 2 ended\n").is_err());
    assert!(recorded("bridge 1 started\nbridge 1 ended\nbridge 2 started\n").is_err());
    assert!(recorded("bridge 1 started\nbridge 1 ended\nbridge 3 started\nbridge 3 ended\n").is_err());
    assert!(recorded("bridge 1 ended\nbridge 1 started\n").is_err());
}

/// A profile's run, watched from `/proc` ([`watched`]).
#[cfg(target_os = "linux")]
struct Watched {
    /// Its output: the rows and the record of its bridges.
    text: String,
    status: std::process::ExitStatus,
    /// The bridges the looks saw, by process id.
    seen: std::collections::HashSet<u32>,
    /// The most bridges one look saw running.
    most: usize,
}

/// `cbtool profile` on a database of two games written as `name`, its
/// bridges looked at in `/proc` every millisecond until it ends. Once the
/// looks have seen `stall_at` bridges, they stop until it ends, as a watcher
/// descheduled that long would.
#[cfg(target_os = "linux")]
fn watched(name: &str, stall_at: usize) -> Watched {
    use cbformat::fixture::{Builder, quiet};
    use cbformat::movetable::{self, Color, Piece};
    use std::process::Stdio;

    let mut b = Builder::new();
    let e4 = b.moves(1, &[movetable::MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), movetable::END_OF_LINE]);
    b.game(e4);
    b.game(e4);
    let db = b.write(name);
    let out = db.dir().join("profile.txt");
    let mut profile = Command::new(env!("CARGO_BIN_EXE_cbtool"))
        .arg("profile")
        .arg(db.dir().join("db.2cbh"))
        .arg("--index")
        .arg(db.dir().join("index"))
        .stdout(std::fs::File::create(&out).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let (mut seen, mut most) = (std::collections::HashSet::new(), 0);
    let status = loop {
        if seen.len() >= stall_at {
            break profile.wait().unwrap();
        }
        if let Some(status) = profile.try_wait().unwrap() {
            break status;
        }
        let running = children(profile.id());
        most = most.max(running.len());
        seen.extend(running);
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    Watched { text: std::fs::read_to_string(&out).unwrap(), status, seen, most }
}

/// The processes running now whose parent is `parent`. The kernel lists
/// each thread's children in `/proc/<pid>/task/<tid>/children`, so a look
/// reads a file per thread of `parent` rather than every process's `stat`:
/// looks that took long on a loaded machine missed a bridge that started and
/// ended between two of them (#217).
#[cfg(target_os = "linux")]
fn children(parent: u32) -> Vec<u32> {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{parent}/task")) else { return Vec::new() };
    let listed = tasks.flatten().filter_map(|t| std::fs::read_to_string(t.path().join("children")).ok());
    let running = |pid: &u32| {
        // `pid (name) state …`, where the name may hold spaces and
        // parentheses; an ended child waits as a zombie until it is reaped.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        stat.rfind(')').is_some_and(|end| stat[end + 1..].split_whitespace().next().is_some_and(|state| state != "Z"))
    };
    listed
        .flat_map(|l| l.split_whitespace().filter_map(|p| p.parse().ok()).collect::<Vec<u32>>())
        .filter(running)
        .collect()
}
