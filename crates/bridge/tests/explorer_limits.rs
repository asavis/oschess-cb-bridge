//! Index builds and index files at their limits, in a bridge process whose
//! address space is capped: nothing a database or an index file claims makes
//! the bridge allocate beyond its budget or abort. Sparse files need Unix.
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};

use bridge::catalog::{Catalog, id_of};
use bridge::explorer::format::{BLOCK_ENTRY, Header, MAX_PLY, PRUNE_PLY};
use cbformat::fixture::{Builder, TempDb, annotations, lid_header, quiet};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};

const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";
const START: &str = "rnbqkbnr%2Fpppppppp%2F8%2F8%2F8%2F8%2FPPPPPPPP%2FRNBQKBNR%20w%20KQkq%20-%200%201";

fn get(port: u16, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let raw = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let status = out.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    (status, out)
}

/// The bridge as a separate process under an address-space limit of
/// `limit_kib`, with a 16 MiB search budget and four workers; killed when
/// dropped.
struct Limited {
    child: std::process::Child,
    port: u16,
    id: String,
}

impl Limited {
    /// Starts the bridge on a port found free, again on another port when a
    /// bridge of another test took that one first (see `serves`).
    fn start(path: &Path, home: &Path, limit_kib: u64) -> Limited {
        std::fs::create_dir_all(home).unwrap();
        std::fs::write(home.join("token"), TOKEN).unwrap();
        for _ in 0..10 {
            let port = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
            std::fs::write(home.join("bridge.toml"), format!("port = {port}\n")).unwrap();
            let child = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("ulimit -v {limit_kib} && exec \"$0\" --database \"$1\""))
                .arg(env!("CARGO_BIN_EXE_oschess-bridge"))
                .arg(path)
                .env("OSCHESS_BRIDGE_HOME", home)
                .env("OSCHESS_BRIDGE_SEARCH_MIB", "16")
                .env("OSCHESS_BRIDGE_THREADS", "4")
                .env("MALLOC_ARENA_MAX", "2")
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let mut limited = Limited { child, port, id: id_of(path) };
            if limited.serves() {
                return limited;
            }
        }
        panic!("the bridge did not start");
    }

    /// Whether this bridge runs and serves its database, waiting for it to
    /// start: a bridge that could not bind its port ends, and the port may
    /// then be another test's bridge.
    fn serves(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if self.child.try_wait().unwrap().is_some() {
                return false;
            }
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                let listed = get(self.port, "/v1/databases").1.contains(&format!(r#""id":"{}""#, self.id));
                return listed && self.child.try_wait().unwrap().is_none();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// The explorer's answer for the start position once the index is built.
    fn explore(&mut self) -> String {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let (status, out) = get(self.port, &format!("/v1/databases/{}/explorer?fen={START}", self.id));
            if status == 200 {
                return out;
            }
            assert_eq!(status, 409, "{out}");
            assert!(self.child.try_wait().unwrap().is_none(), "the bridge ended while indexing");
            assert!(Instant::now() < deadline, "the index was not built: {out}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Limited {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn e4(b: &mut Builder) -> i64 {
    b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE])
}

/// Annotation records that span 240 MiB of a sparse `.2cba`, which a batch
/// read of the database (at most 256 MiB) would take whole: the index never
/// reads annotations, so the build stays within a 256 MiB address space, well
/// clear of what the bridge itself takes (about 90 MB).
#[test]
fn a_huge_annotation_file_is_never_read() {
    let mut b = Builder::new();
    let moves = e4(&mut b);
    let ann = b.annotations(&annotations(&[]));
    for _ in 0..50 {
        b.annotated_game(moves, ann);
    }
    let db: TempDb = b.write("explorer-limits-annotations");
    let cba = db.dir().join("db.2cba");
    std::fs::OpenOptions::new().write(true).open(&cba).unwrap().set_len(240 << 20).unwrap();
    // Game 2's annotation record claims to lie near the end of the file.
    let cbh = db.dir().join("db.2cbh");
    let mut headers = std::fs::read(&cbh).unwrap();
    headers[2 * 192 + 0x10..2 * 192 + 0x18].copy_from_slice(&((240i64 << 20) - 64).to_le_bytes());
    std::fs::write(&cbh, headers).unwrap();
    let mut bridge = Limited::start(&cbh, &db.dir().join("home"), 256 << 10);
    let out = bridge.explore();
    assert!(out.contains(r#""games":50,"white":50"#), "{out}");
}

/// A move record of 5 MiB, over the 2 MiB the index reads: its game is left
/// out, and the others are indexed.
#[test]
fn a_move_record_over_the_limit_leaves_its_game_out() {
    let mut b = Builder::new();
    let moves = e4(&mut b);
    let mut huge = vec![MOVES, quiet(Color::White, Piece::Pawn, "d2", "d4"), END_OF_LINE];
    huge.resize(5 << 19, 0);
    let big = b.moves(1, &huge);
    for g in 0..21 {
        b.game(if g == 10 { big } else { moves });
    }
    let db = b.write("explorer-limits-moves");
    let cbh = db.dir().join("db.2cbh");
    let mut bridge = Limited::start(&cbh, &db.dir().join("home"), 256 << 10);
    let out = bridge.explore();
    assert!(out.contains(r#""games":20,"white":20"#), "{out}");
    assert!(!out.contains(r#""uci":"d2d4""#), "{out}");
}

/// An index file whose CRC-valid header claims a table of 33,554,432 blocks,
/// in a sparse file of the length it names: refused before its 940 MB table
/// would be allocated, then rebuilt, within a 256 MiB address space.
#[test]
fn an_index_file_claiming_a_huge_table_is_rebuilt() {
    let mut b = Builder::new();
    let moves = e4(&mut b);
    for _ in 0..10 {
        b.game(moves);
    }
    b.lid(lid_header(1024, 0));
    let db = b.write("explorer-limits-table");
    let cbh = db.dir().join("db.2cbh");
    let home = db.dir().join("home");
    let generation = Catalog::new([cbh.clone()]).entries()[0].generation().unwrap();
    let blocks: u32 = 1 << 25;
    let table_offset = 1u64 << 30;
    let h = Header {
        max_ply: MAX_PLY,
        prune_ply: PRUNE_PLY,
        first_record: 1,
        last_record: 10,
        generation,
        games: 10,
        keys: u64::from(blocks),
        blocks,
        table_offset,
        table_crc: 0,
        file_len: table_offset + u64::from(blocks) * BLOCK_ENTRY as u64,
    };
    std::fs::create_dir_all(home.join("index")).unwrap();
    let f = std::fs::File::create(home.join("index").join(format!("{}.idx", id_of(&cbh)))).unwrap();
    f.set_len(h.file_len).unwrap();
    std::os::unix::fs::FileExt::write_all_at(&f, &h.encode(), 0).unwrap();
    drop(f);
    let mut bridge = Limited::start(&cbh, &home, 256 << 10);
    let out = bridge.explore();
    assert!(out.contains(r#""games":10,"white":10"#), "{out}");
}
