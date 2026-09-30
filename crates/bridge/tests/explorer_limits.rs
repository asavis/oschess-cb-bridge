//! Index builds and index files at their limits, in a bridge process whose
//! address space is capped: nothing a database or an index file claims makes
//! the bridge allocate beyond its budget or abort. Sparse files need Unix.
#![cfg(unix)]

use std::path::Path;

use bridge::catalog::{Catalog, id_of};
use bridge::explorer::format::{BLOCK_ENTRY, DEEP_BLOCK_ENTRY, Header, MAX_PLY, MIN_DEEP_BITS};
use cbformat::fixture::{Builder, TempDb, annotations, lid_header, quiet};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};

mod common;
use common::{Limited, WAIT_LIMIT, poll, try_get};

const START: &str = "rnbqkbnr%2Fpppppppp%2F8%2F8%2F8%2F8%2FPPPPPPPP%2FRNBQKBNR%20w%20KQkq%20-%200%201";

/// The bridge under an address-space limit of `limit_kib` KiB, serving the
/// database at `path`, with a 16 MiB search budget and four workers.
fn limited(path: &Path, home: &Path, limit_kib: u64) -> Limited {
    let env = [("OSCHESS_BRIDGE_SEARCH_MIB", "16"), ("OSCHESS_BRIDGE_THREADS", "4")];
    Limited::start(path, home, limit_kib, &env)
}

/// The explorer's answer for the start position once the index is built,
/// which is answered `409` meanwhile; the bridge must keep running.
fn explore(bridge: &mut Limited) -> String {
    let path = format!("/v1/databases/{}/explorer?fen={START}", bridge.id);
    let mut last = String::new();
    let answer = poll(WAIT_LIMIT, || {
        let Some((status, out)) = try_get(bridge.port, &path) else {
            panic!("the bridge stopped answering; it ended: {:?}", bridge.ended());
        };
        if status == 200 {
            return Some(out);
        }
        assert_eq!(status, 409, "{out}");
        assert!(bridge.ended().is_none(), "the bridge ended while indexing");
        last = out;
        None
    });
    answer.unwrap_or_else(|| panic!("the index was not built: {last}"))
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
    let mut bridge = limited(&cbh, &db.dir().join("home"), 256 << 10);
    let out = explore(&mut bridge);
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
    let mut bridge = limited(&cbh, &db.dir().join("home"), 256 << 10);
    let out = explore(&mut bridge);
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
    let deep_offset = table_offset + u64::from(blocks) * BLOCK_ENTRY as u64;
    let h = Header {
        max_ply: MAX_PLY,
        prune_ply: MAX_PLY,
        first_record: 1,
        last_record: 10,
        generation,
        games: 10,
        keys: u64::from(blocks),
        blocks,
        table_offset,
        table_crc: 0,
        // A deep section of one empty block after the table, so that only the
        // table's claim is wrong.
        file_len: deep_offset + DEEP_BLOCK_ENTRY as u64,
        deep_bits: MIN_DEEP_BITS,
        deep_postings: 0,
        deep_offset,
        deep_table_offset: deep_offset,
        deep_table_crc: 0,
        build_id: 1,
    };
    std::fs::create_dir_all(home.join("index")).unwrap();
    let f = std::fs::File::create(home.join("index").join(format!("{}.idx", id_of(&cbh)))).unwrap();
    f.set_len(h.file_len).unwrap();
    std::os::unix::fs::FileExt::write_all_at(&f, &h.encode(), 0).unwrap();
    drop(f);
    let mut bridge = limited(&cbh, &home, 256 << 10);
    let out = explore(&mut bridge);
    assert!(out.contains(r#""games":10,"white":10"#), "{out}");
}
