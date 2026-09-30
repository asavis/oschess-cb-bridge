//! A build waits for memory that searches hold and never evicts it. A test
//! binary of its own, since the search budget is shared by the whole process.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use bridge::explorer;
use bridge::explorer::runs::Progress;
use bridge::search::memory::{Evict, Hold, budget, held, register};
use cbformat::fixture::{Builder, quiet};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};
use cbformat::v2::Database;

struct Flag(AtomicBool);

impl Evict for Flag {
    fn evict(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn a_build_waits_for_memory_and_never_evicts() {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    for _ in 0..50 {
        b.game(e4);
    }
    let db = b.write("explorer-budget");
    let d = Database::open(db.dir().join("db.2cbh")).unwrap();
    let flag = Arc::new(Flag(AtomicBool::new(false)));
    let weak: Weak<dyn Evict> = Arc::downgrade(&(Arc::clone(&flag) as Arc<dyn Evict>));
    register(weak);
    // Searches hold the whole budget for a second: the build, which cannot
    // reserve a byte meanwhile, ends only after they give it back.
    let all = Hold::reserve_quietly(budget() - held()).unwrap();
    let freed = Arc::new(AtomicBool::new(false));
    let release = {
        let freed = Arc::clone(&freed);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            freed.store(true, Ordering::SeqCst);
            drop(all);
        })
    };
    let dir = std::env::temp_dir().join(format!("bridge-explorer-budget-{}", std::process::id()));
    let loaded = explorer::prepare(&d, 1, &dir, "db", &Progress::default()).unwrap();
    assert!(freed.load(Ordering::SeqCst), "the build waited for the memory");
    assert!(!flag.0.load(Ordering::SeqCst), "nothing searches kept was evicted");
    assert_eq!(loaded.games(), 50);
    release.join().unwrap();
    assert!(held() > 0, "the open index holds its table of blocks");
    drop(loaded);
    assert_eq!(held(), 0, "the build returned what it reserved, and the index its table");
    std::fs::remove_dir_all(&dir).unwrap();
}
