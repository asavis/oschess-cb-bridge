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
