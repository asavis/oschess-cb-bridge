//! Classic databases written by the fixture's builder, read back whole.

use cbformat::cbh::Database;
use cbformat::fixture_cbh::{Builder, Tok, encode, move_record};
use cbformat::v2::{GameResult, RecordKind};
use chesscore::Board;

use Tok::{End as E, Mv as M};

fn game(moves: &[&str]) -> Vec<u8> {
    let mut toks: Vec<Tok<'_>> = moves.iter().map(|m| M(m)).collect();
    toks.push(E);
    move_record(0, None, None, &encode(&Board::startpos(), &toks, 0, false))
}

/// `games` games of 1 to 4 plies of 1.e4 e5 2.Nf3 Nc6.
fn builder(games: usize) -> Builder {
    let line = ["e2e4", "e7e5", "g1f3", "b8c6"];
    let mut b = Builder::new();
    for g in 0..games {
        b.game(&game(&line[..1 + g % 4]))[0x1b] = (g % 3) as u8;
    }
    b
}

#[test]
fn records_entities_and_moves() {
    let mut b = builder(3);
    // Last names: Windows-1252 "Müller", and UTF-8 "Łódź".
    b.player_name(0, &[0x4d, 0xfc, 0x6c, 0x6c, 0x65, 0x72]).player_name(1, "Łódź".as_bytes());
    let f = b.write("records");
    let db = Database::open(f.dir().join("db.cbh")).unwrap();
    assert_eq!(db.record_count(), 3);
    let r = db.record(2).unwrap();
    assert_eq!(r.kind(), RecordKind::Game);
    assert_eq!(r.result(), GameResult::Draw);
    let e = db.entities();
    assert_eq!(e.player(r.white()).unwrap().unwrap().last, "Müller");
    assert_eq!(e.player(r.black()).unwrap().unwrap().last, "Łódź");
    assert_eq!(e.tournament(r.tournament()).unwrap().unwrap().title, "Paris");
    assert_eq!(e.player(99).unwrap(), None);
    assert!(db.record(4).is_err() && db.record(0).is_err());
    let data = db.moves_of(&r).unwrap();
    let mut n = 0;
    cbformat::cbh::walk(&data.moves().unwrap(), &mut Count(&mut n)).unwrap();
    assert_eq!(n, 2);
}

struct Count<'a>(&'a mut u32);

impl cbformat::replay::TreeVisitor for Count<'_> {
    fn play(&mut self, _: &Board, _: Option<chesscore::Move>, _: bool) {
        *self.0 += 1;
    }
}

#[test]
fn batches_read_what_single_reads_read() {
    let f = builder(50).write("batches");
    let db = Database::open(f.base()).unwrap();
    for size in [1, 7, 50, 64] {
        let mut first = 1;
        while first <= 50 {
            let batch = db.batch(first, first + size - 1).unwrap();
            assert_eq!(batch.ids(), first..=(first + size - 1).min(50));
            for id in batch.ids() {
                let r = batch.record(id).unwrap();
                assert_eq!(r.bytes(), db.record(id).unwrap().bytes());
                assert_eq!(batch.moves_of(&r).unwrap().bytes(), db.moves_of(&r).unwrap().bytes(), "game {id}");
            }
            first = batch.ids().end() + 1;
        }
    }
    let recs = db.records(10, 20).unwrap();
    assert_eq!(recs.iter().map(|r| r.id()).collect::<Vec<_>>(), (10..=20).collect::<Vec<_>>());
    assert!(db.records(51, 60).unwrap().is_empty());
}

/// Headers and move records read into the caller's buffers are the ones read
/// on their own; a record over the limit is refused before it is read.
#[test]
fn buffered_reads_read_what_single_reads_read() {
    let mut b = builder(30);
    b.text(&[(0, b"Openings")]);
    for g in 0..9 {
        b.game(&game(&["d2d4", "d7d5", "c2c4"][..1 + g % 3]));
    }
    let f = b.write("buffered");
    let db = Database::open(f.base()).unwrap();
    assert_eq!(db.record_count(), 40);
    let mut buf = vec![0u8; 7 * cbformat::cbh::RECORD_SIZE + 3];
    let mut first = 1;
    while first <= 40 {
        let n = db.read_records(first, &mut buf).unwrap();
        assert_eq!(n, 7.min(41 - first));
        let records: Vec<_> = buf[..n as usize * 46]
            .as_chunks::<46>()
            .0
            .iter()
            .enumerate()
            .map(|(i, b)| cbformat::cbh::Record::from_bytes(first + i as u32, b))
            .collect();
        for r in &records {
            assert_eq!(r.bytes(), db.record(r.id()).unwrap().bytes());
        }
        let next = db.record(first + n).ok();
        let mut moves = Vec::with_capacity(1 << 10);
        let window = db.read_move_window(&records, next.as_ref(), &mut moves).unwrap().expect("a window");
        for r in &records {
            let whole = db.moves_of(r).unwrap();
            assert_eq!(db.moves_in(window, &moves, r).unwrap().unwrap().bytes(), whole.bytes());
            let mut one = Vec::with_capacity(64);
            assert_eq!(db.read_moves_into(r, 64, &mut one).unwrap().bytes(), whole.bytes());
            assert_eq!(db.moves_of_within(r, 64).unwrap().bytes(), whole.bytes());
        }
        first += n;
    }
    assert_eq!(db.read_records(0, &mut buf).unwrap(), 0);
    assert_eq!(db.read_records(41, &mut buf).unwrap(), 0);
    // A window larger than the buffer is not read; a record over the limit
    // or the buffer is refused.
    let records = db.records(1, 40).unwrap();
    assert!(db.read_move_window(&records, None, &mut Vec::with_capacity(8)).unwrap().is_none());
    let r = db.record(4).unwrap();
    let size = db.moves_of(&r).unwrap().bytes().len();
    assert!(db.moves_of_within(&r, size - 1).is_err());
    assert!(db.read_moves_into(&r, size - 1, &mut Vec::with_capacity(64)).is_err());
    assert!(db.read_moves_into(&r, 64, &mut Vec::with_capacity(size - 1)).is_err());
}

/// A guiding text's title is in its `.cbg` record: the first that is not blank.
#[test]
fn guiding_text_titles() {
    let mut b = builder(1);
    b.text(&[(0, b""), (1, b"Er\xf6ffnungen"), (0, b"Openings")]);
    b.text(&[]);
    let f = b.write("titles");
    let db = Database::open(f.base()).unwrap();
    let title = |id: u32| db.text_title(&db.record(id).unwrap(), 1 << 10).unwrap();
    assert_eq!(db.record(2).unwrap().kind(), RecordKind::Text);
    assert_eq!((title(1), title(2), title(3)), (String::new(), "Eröffnungen".to_string(), String::new()));
    // Read up to the limit only: a title past it reads as none.
    assert_eq!(db.text_title(&db.record(2).unwrap(), 12).unwrap(), "");
}

/// Damaged files give errors when opened or read, never panics.
#[test]
fn damaged_files_are_refused() {
    let f = builder(4).write("damaged");
    let path = |ext: &str| f.dir().join(format!("db{ext}"));
    let original = |ext: &str| std::fs::read(path(ext)).unwrap();
    let with = |ext: &str, bytes: Vec<u8>, check: &dyn Fn(cbformat::Result<Database>)| {
        let keep = original(ext);
        std::fs::write(path(ext), bytes).unwrap();
        check(Database::open(f.base()));
        std::fs::write(path(ext), keep).unwrap();
    };
    let refused = |r: cbformat::Result<Database>| assert!(r.is_err());
    // A header file that is not whole records, or names another record size.
    let mut cbh = original(".cbh");
    cbh.push(0);
    with(".cbh", cbh, &refused);
    let mut cbh = original(".cbh");
    cbh[4] = 47;
    with(".cbh", cbh, &refused);
    // An entity file with a bad magic number, a tiny record, or missing.
    let mut cbp = original(".cbp");
    cbp[8] ^= 1;
    with(".cbp", cbp, &refused);
    let mut cbp = original(".cbp");
    cbp[12] = 10;
    with(".cbp", cbp, &refused);
    with(".cbp", Vec::new(), &refused);
    // Move offsets and sizes that point outside the file: errors on reading.
    let games = |bytes: Vec<u8>| {
        with(".cbh", bytes, &|r| {
            let db = r.unwrap();
            let rec = db.record(1).unwrap();
            assert!(db.moves_of(&rec).is_err());
            assert!(db.batch(1, 4).unwrap().moves_of(&rec).is_err());
        })
    };
    let mut far = original(".cbh");
    far[47..51].copy_from_slice(&0x00ff_ffffu32.to_be_bytes());
    games(far);
    let mut low = original(".cbh");
    low[47..51].copy_from_slice(&2u32.to_be_bytes());
    games(low);
    let mut cbg = original(".cbg");
    cbg[27..30].copy_from_slice(&[0, 0, 0]); // the first game's size field: 0
    with(".cbg", cbg, &|r| {
        let db = r.unwrap();
        let err = db.moves_of(&db.record(1).unwrap()).err().expect("a size of 0 is refused");
        assert!(err.to_string().contains("smaller than"), "{err}");
    });
    let mut cbg = original(".cbg");
    cbg[26..30].copy_from_slice(&[0, 0xff, 0xff, 0xff]); // size past the end
    with(".cbg", cbg, &|r| {
        let db = r.unwrap();
        assert!(db.moves_of(&db.record(1).unwrap()).is_err());
    });
}

/// A move file over 4 GiB is read through the 64-bit offsets of `.cbj`: the
/// game's record is moved past 4 GiB in a sparse copy of `.cbg`, where only
/// `.cbj` can reach it.
#[cfg(unix)]
#[test]
fn a_move_file_over_4_gib_is_read_through_cbj() {
    use std::os::unix::fs::FileExt;
    let f = builder(1).write("wide");
    let path = |ext: &str| f.dir().join(format!("db{ext}"));
    let cbg = std::fs::read(path(".cbg")).unwrap();
    let rec = &cbg[26..];
    let far: u64 = (1 << 32) + 26;
    let file = std::fs::File::create(path(".cbg")).unwrap();
    file.set_len(far + rec.len() as u64).unwrap();
    file.write_all_at(&cbg[..26], 0).unwrap();
    file.write_all_at(rec, far).unwrap();
    drop(file);
    let cbj = |moves: u64| {
        let mut b = Vec::new();
        for v in [11i32, 120, 1] {
            b.extend(v.to_le_bytes());
        }
        b.resize(32 + 120, 0);
        b[32 + 0x1e..32 + 0x26].copy_from_slice(&moves.to_be_bytes());
        b
    };

    let err = Database::open(f.base()).err().expect("no .cbj");
    assert!(err.to_string().contains(".cbj"), "{err}");

    std::fs::write(path(".cbj"), cbj(far)).unwrap();
    let db = Database::open(f.base()).unwrap();
    let r = db.record(1).unwrap();
    assert_eq!(r.moves_offset(), 26, "the .cbh offset keeps only the low 32 bits");
    let mut n = 0;
    cbformat::cbh::walk(&db.moves_of(&r).unwrap().moves().unwrap(), &mut Count(&mut n)).unwrap();
    assert_eq!(n, 1);
    assert_eq!(db.batch(1, 1).unwrap().moves_of(&r).unwrap().bytes(), rec);
    // A window cannot be placed by the 32-bit offsets; a read on its own can.
    let mut buf = Vec::with_capacity(1 << 10);
    assert!(db.read_move_window(&[r], None, &mut buf).unwrap().is_none());
    assert_eq!(db.read_moves_into(&r, 1 << 10, &mut buf).unwrap().bytes(), rec);
    assert!(cbformat::pgn::classic_game(&db, 1).unwrap().contains("\n1. e4 0-1"));

    // Offsets that do not agree with `.cbh` are damage.
    std::fs::write(path(".cbj"), cbj(far + 1)).unwrap();
    let db = Database::open(f.base()).unwrap();
    assert!(db.moves_of(&db.record(1).unwrap()).is_err());
    // A record too short for the offsets refuses the database.
    let mut short = cbj(far);
    short[4..8].copy_from_slice(&20i32.to_le_bytes());
    std::fs::write(path(".cbj"), short).unwrap();
    assert!(Database::open(f.base()).is_err());
}
