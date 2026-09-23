//! Batched reads return exactly what single reads return.

use cbformat::fixture::{Builder, TempDb, quiet};
use cbformat::movetable::{self, Color, Piece};
use cbformat::v2::{Database, MAX_BATCH_RECORDS, Token};

/// `games` games of 0 to 6 plies of 1.e4 e5 2.Nf3 Nc6 3.Bb5 a6, their move
/// records back to back except game `moved`'s, which is appended at the end.
fn fixture(games: u32, moved: u32) -> TempDb {
    use Color::{Black as B, White as W};
    use Piece::{Bishop, Knight, Pawn};
    let line = [
        quiet(W, Pawn, "e2", "e4"),
        quiet(B, Pawn, "e7", "e5"),
        quiet(W, Knight, "g1", "f3"),
        quiet(B, Knight, "b8", "c6"),
        quiet(W, Bishop, "f1", "b5"),
        quiet(B, Pawn, "a7", "a6"),
    ];
    let words = |g: u32| {
        let mut w = vec![movetable::MOVES];
        w.extend(&line[..(g % 7) as usize]);
        w.push(movetable::END_OF_LINE);
        w
    };
    let mut b = Builder::new();
    let mut offsets = vec![0i64; games as usize + 1];
    for g in (1..=games).filter(|&g| g != moved).chain((moved > 0).then_some(moved)) {
        offsets[g as usize] = b.moves(1, &words(g));
    }
    for g in 1..=games {
        b.game(offsets[g as usize])[0x58] = (g % 3) as u8;
    }
    b.write(&format!("batch-{games}-{moved}"))
}

fn tokens(db: &Database, id: u32) -> Vec<Token> {
    db.moves_of(&db.record(id).unwrap()).unwrap().moves().unwrap().tokens().collect()
}

#[test]
fn batches_read_what_single_reads_read() {
    let f = fixture(300, 10);
    let db = Database::open(f.base()).unwrap();
    assert_eq!(db.record_count(), 300);
    for size in [1, 7, 64, 300, 1000] {
        let mut first = 1;
        while first <= 300 {
            let batch = db.batch(first, first + size - 1).unwrap();
            assert_eq!(*batch.ids().start(), first);
            for id in batch.ids() {
                let r = batch.record(id).unwrap();
                assert_eq!(r.bytes(), db.record(id).unwrap().bytes(), "record {id}, batch size {size}");
                let data = batch.moves_of(&r).unwrap();
                let got: Vec<Token> = data.moves().unwrap().tokens().collect();
                assert_eq!(got, tokens(&db, id), "moves of {id}, batch size {size}");
                assert_eq!(got.len(), (id % 7) as usize + 1);
            }
            assert_eq!(batch.ids(), first..=(first + size - 1).min(300), "batch size {size}");
            first = batch.ids().end() + 1;
        }
    }
}

#[test]
fn batch_ids_are_the_ids_asked_for() {
    let f = fixture(4, 0);
    let db = Database::open(f.base()).unwrap();
    // The batch reads one record past its end to bound its moves; that record
    // is not one of its ids.
    assert_eq!(db.batch(1, 1).unwrap().ids(), 1..=1);
    let ids: Vec<u32> = [(1, 2), (3, 4)].iter().flat_map(|&(a, b)| db.batch(a, b).unwrap().ids()).collect();
    assert_eq!(ids, [1, 2, 3, 4]);
    let batch = db.batch(1, 2).unwrap();
    assert_eq!(batch.record(3).unwrap().bytes(), db.record(3).unwrap().bytes());
}

#[test]
fn record_runs_and_batches_are_bounded() {
    let games = MAX_BATCH_RECORDS + 10;
    let f = fixture(games, 0);
    let db = Database::open(f.base()).unwrap();
    let batch = db.batch(1, u32::MAX).unwrap();
    assert_eq!(batch.ids(), 1..=MAX_BATCH_RECORDS);
    let last = batch.record(MAX_BATCH_RECORDS).unwrap();
    assert_eq!(
        tokens(&db, MAX_BATCH_RECORDS),
        batch.moves_of(&last).unwrap().moves().unwrap().tokens().collect::<Vec<_>>()
    );
    let records = db.records(1, u32::MAX).unwrap();
    assert_eq!(records.len(), MAX_BATCH_RECORDS as usize);
    let tail = db.records(MAX_BATCH_RECORDS + 1, u32::MAX).unwrap();
    assert_eq!(tail.iter().map(|r| r.id()).collect::<Vec<_>>(), (MAX_BATCH_RECORDS + 1..=games).collect::<Vec<_>>());
    for r in records.iter().chain(&tail).step_by(997) {
        assert_eq!(r.bytes(), db.record(r.id()).unwrap().bytes(), "record {}", r.id());
    }
    assert!(db.records(games + 1, u32::MAX).unwrap().is_empty());
    assert!(db.records(5, 4).unwrap().is_empty());
}

#[test]
fn a_header_file_truncated_after_opening_is_an_error() {
    // Each test's fixture has its own game count, which names its directory.
    let f = fixture(21, 0);
    let db = Database::open(f.base()).unwrap();
    assert_eq!(db.records(1, 21).unwrap().len(), 21);
    std::fs::OpenOptions::new().write(true).open(f.dir().join("db.2cbh")).unwrap().set_len(2 * 192).unwrap();
    assert!(db.records(1, 21).is_err());
    assert!(db.batch(1, 21).is_err());
    assert!(db.record(2).is_err());
    assert!(db.record(1).is_ok());
}

#[test]
fn a_batch_past_the_end_or_empty_reads_nothing() {
    let f = fixture(20, 0);
    let db = Database::open(f.base()).unwrap();
    assert_eq!(db.batch(15, 100).unwrap().ids(), 15..=20);
    assert!(db.batch(21, 30).unwrap().ids().is_empty());
    assert!(db.batch(1, 0).unwrap().ids().is_empty());
    // Outside its ids, a batch reads the record on its own.
    let empty = db.batch(1, 0).unwrap();
    let r = empty.record(5).unwrap();
    assert_eq!(r.bytes(), db.record(5).unwrap().bytes());
    assert!(empty.moves_of(&r).is_ok());
    assert!(empty.record(21).is_err());
}

/// The last id `u32::MAX` is read without overflow. The header file is sparse:
/// 824 GB long and a few bytes on disk, which needs a Unix file system.
#[cfg(unix)]
#[test]
fn the_largest_record_id_is_read_without_overflow() {
    let f = fixture(22, 0);
    let file = std::fs::OpenOptions::new().write(true).open(f.dir().join("db.2cbh")).unwrap();
    file.set_len((u64::from(u32::MAX) + 1) * 192).unwrap();
    let db = Database::open(f.base()).unwrap();
    assert_eq!(db.record_count(), u32::MAX);
    let ids: Vec<u32> = db.records(u32::MAX - 2, u32::MAX).unwrap().iter().map(|r| r.id()).collect();
    assert_eq!(ids, [u32::MAX - 2, u32::MAX - 1, u32::MAX]);
    assert_eq!(db.records(u32::MAX, u32::MAX).unwrap().len(), 1);
    let batch = db.batch(u32::MAX, u32::MAX).unwrap();
    assert_eq!(batch.ids(), u32::MAX..=u32::MAX);
    assert_eq!(batch.record(u32::MAX).unwrap().bytes(), db.record(u32::MAX).unwrap().bytes());
}

/// A move record 4 GiB past a batch's span is read on its own. On a 32-bit
/// target its distance from the span does not fit in `usize`, and must not
/// wrap onto a frame inside the span. The move file is sparse, which needs a
/// Unix file system.
#[cfg(unix)]
#[test]
fn a_move_record_4_gib_past_the_span_is_read_on_its_own() {
    use cbformat::fixture::{bytes, framed};
    use std::os::unix::fs::FileExt;
    let f = fixture(23, 0);
    let far = (1i64 << 32) + 12;
    let null = movetable::NULL_MOVE;
    let content = bytes(&[movetable::MOVES, null, null, movetable::END_OF_LINE]);
    let cbg = std::fs::OpenOptions::new().write(true).open(f.dir().join("db.2cbg")).unwrap();
    cbg.write_all_at(&framed(1, &content), far as u64).unwrap();
    let cbh = std::fs::OpenOptions::new().write(true).open(f.dir().join("db.2cbh")).unwrap();
    cbh.write_all_at(&far.to_le_bytes(), 3 * 192 + 8).unwrap();
    let db = Database::open(f.base()).unwrap();
    let r = db.record(3).unwrap();
    let alone: Vec<Token> = db.moves_of(&r).unwrap().moves().unwrap().tokens().collect();
    assert_eq!(alone, [Token::Move(null), Token::Move(null), Token::EndOfLine]);
    let batch = db.batch(1, 1).unwrap();
    let got: Vec<Token> = batch.moves_of(&r).unwrap().moves().unwrap().tokens().collect();
    assert_eq!(got, alone);
}
