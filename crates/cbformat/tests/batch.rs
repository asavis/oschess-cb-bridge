//! Batched reads return exactly what single reads return.

use std::path::PathBuf;

use cbformat::movetable::{self, Captured, Color, MoveWord, Piece};
use cbformat::v2::{Database, Token, checksum};

struct Fixture {
    dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn word(color: Color, piece: Piece, from: &str, to: &str) -> u16 {
    let sq = |s: &str| {
        let b = s.as_bytes();
        (b[1] - b'1') * 8 + (b[0] - b'a')
    };
    let want =
        MoveWord::Normal { color, piece, from: sq(from), to: sq(to), captured: Captured::Nothing, promotion: None };
    (1..0xb12d).find(|&w| movetable::decode(w) == Some(want)).unwrap()
}

fn frame(content: &[u8]) -> Vec<u8> {
    let mut r = vec![0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11];
    r.extend((content.len() as i32).to_le_bytes());
    r.extend(0i32.to_le_bytes());
    r.extend(checksum(content).to_be_bytes());
    r.extend(1u16.to_le_bytes());
    r.extend(content);
    r.extend((content.len() as i64 + 34).to_le_bytes());
    r
}

/// `games` games of 0 to 6 plies of 1.e4 e5 2.Nf3 Nc6 3.Bb5 a6, their move
/// records back to back except game `moved`'s, which is appended at the end.
fn fixture(games: u32, moved: u32) -> Fixture {
    use Color::{Black as B, White as W};
    use Piece::{Bishop, Knight, Pawn};
    let line = [
        word(W, Pawn, "e2", "e4"),
        word(B, Pawn, "e7", "e5"),
        word(W, Knight, "g1", "f3"),
        word(B, Knight, "b8", "c6"),
        word(W, Bishop, "f1", "b5"),
        word(B, Pawn, "a7", "a6"),
    ];
    let dir = std::env::temp_dir().join(format!("cbformat-batch-{}-{games}-{moved}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cbh = vec![0u8; 192];
    cbh[0x0a..0x0c].copy_from_slice(&192i16.to_le_bytes());
    cbh[0x0d] = 5;
    let mut cbg = vec![0u8; 12];
    let mut late = Vec::new();
    let mut offsets = vec![0i64; games as usize + 1];
    for g in 1..=games {
        let plies = (g % 7) as usize;
        let mut words = vec![movetable::MOVES];
        words.extend(&line[..plies]);
        words.push(movetable::END_OF_LINE);
        let content: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        if g == moved {
            late = frame(&content);
        } else {
            offsets[g as usize] = cbg.len() as i64;
            cbg.extend(frame(&content));
        }
    }
    if moved > 0 {
        offsets[moved as usize] = cbg.len() as i64;
        cbg.extend(late);
    }
    let total = cbg.len() as i64;
    cbg[..8].copy_from_slice(&total.to_le_bytes());
    for (g, offset) in offsets.iter().enumerate().skip(1) {
        let mut rec = [0u8; 192];
        rec[0] = 1;
        rec[2] = 1;
        rec[3] = 1;
        rec[8..16].copy_from_slice(&offset.to_le_bytes());
        rec[0x58] = (g % 3) as u8;
        cbh.extend(rec);
    }
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
    Fixture { dir }
}

fn tokens(db: &Database, id: u32) -> Vec<Token> {
    db.moves_of(&db.record(id).unwrap()).unwrap().moves().unwrap().tokens().collect()
}

#[test]
fn batches_read_what_single_reads_read() {
    let f = fixture(300, 10);
    let db = Database::open(f.dir.join("db")).unwrap();
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
            first = batch.ids().end() + 1;
        }
    }
}

#[test]
fn a_batch_past_the_end_or_empty_reads_nothing() {
    let f = fixture(20, 0);
    let db = Database::open(f.dir.join("db")).unwrap();
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
