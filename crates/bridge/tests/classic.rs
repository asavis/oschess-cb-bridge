//! Classic databases through the HTTP API: listed, searched and served like
//! 2CBH ones, and answering as a 2CBH copy of the same content answers.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bridge::catalog::id_of;
use bridge::store::MAX_GAME_BYTES;
use cbformat::fixture_cbh::{Builder, Tok, annotation_record, encode, move_record};
use chesscore::Board;

mod common;
use common::{
    classic_fixture, fixture, get, has_members, has_object, member, object_with, start_with_dir, without_generation,
};

fn index_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bridge-classic-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// The fixture of `docs/search-grammar.md` in both formats, served together:
/// the classic copy is listed as `cbh` and ready, and its lists, searches,
/// sorts, suggestions, games and explorer answers are the 2CBH copy's.
#[test]
fn a_classic_copy_answers_as_its_2cbh_copy() {
    let (f2, fc) = (fixture("classic-api-2cbh", &[]), classic_fixture("classic-api-cbh", &[]));
    let (p2, pc) = (f2.dir().join("db.2cbh"), fc.dir().join("db.cbh"));
    let dir = index_dir("copies");
    let (port, _app) = start_with_dir([pc.clone(), p2.clone()], &dir);
    let (ic, i2) = (id_of(&pc), id_of(&p2));

    let (status, body) = get(port, "/v1/databases");
    assert_eq!(status, 200);
    let listed = format!(r#""id":"{ic}","name":"db","format":"cbh","state":"ready","records":10"#);
    let listed = object_with(&body, &listed).unwrap_or_else(|| panic!("{body}"));
    assert!(member(listed, "generation").starts_with('"'), "{listed}");
    assert!(get(port, "/v1/status").1.contains(r#""ready":2"#));

    let both = |path: &str| {
        let (a, b) = (get(port, &path.replace("{id}", &ic)), get(port, &path.replace("{id}", &i2)));
        assert_eq!(a.0, b.0, "{path}: {}", a.1);
        assert_eq!(without_generation(&a.1), without_generation(&b.1), "{path}");
        a
    };
    for query in [
        "",
        "offset=3&limit=4",
        "sort=number-desc&offset=2&limit=5",
        "q=morphy",
        "q=london",
        "q=annotator%3Animzo&sort=date",
        "q=eco%3AC5&sort=eco-desc",
        "q=event%3Apetersburg&sort=white",
        "sort=tournament",
        "sort=annotator-desc",
        "sort=round",
        "q=tag%3Ax",
    ] {
        both(&format!("/v1/databases/{{id}}/games?{query}"));
    }
    for field in ["player", "event", "annotator"] {
        for prefix in ["a", "c", "l", "m", "mik", "n", "st", "t"] {
            both(&format!("/v1/databases/{{id}}/suggest?field={field}&prefix={prefix}"));
        }
    }
    // The guiding text: its title as the event, and no game fields.
    let (_, row) = both("/v1/databases/{id}/games?offset=7&limit=1");
    assert!(has_object(&row, r#""number":8,"kind":"text","white":"","event":"London""#), "{row}");
    assert_eq!(both("/v1/databases/{id}/games/8").0, 422);
    assert_eq!(both("/v1/databases/{id}/games/11").0, 404);
    for number in [1, 3, 5, 9, 10] {
        let (status, body) = both(&format!("/v1/databases/{{id}}/games/{number}"));
        assert_eq!(status, 200, "{body}");
    }

    let start_fen = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR%20w%20KQkq%20-%200%201";
    let after_e4 = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR%20b%20KQkq%20-%200%201";
    for fen in [start_fen, after_e4] {
        let url = format!("/v1/databases/{{id}}/explorer?fen={fen}");
        let deadline = Instant::now() + Duration::from_secs(30);
        while [&ic, &i2].iter().any(|id| get(port, &url.replace("{id}", id)).0 == 409) {
            assert!(Instant::now() < deadline, "the indexes were not built");
            std::thread::sleep(Duration::from_millis(20));
        }
        let (status, body) = both(&url);
        assert_eq!(status, 200, "{body}");
    }
    let (_, body) = get(port, &format!("/v1/databases/{ic}/explorer?fen={start_fen}"));
    // Games 1-7 and 10: not the guiding text, and not the deleted game.
    assert!(has_members(&body, r#""index":{"records":10,"games":8}"#), "{body}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A classic game is served with its annotations; a move or annotation record
/// over the rendering limit is refused before it is read.
#[test]
fn classic_games_are_rendered_within_the_limits() {
    let e4 = move_record(0, None, None, &encode(&Board::startpos(), &[Tok::Mv("e2e4"), Tok::End], 0, false));
    let mut b = Builder::new();
    b.game(&e4);
    b.annotations(&annotation_record(1, &[(0, 0x02, b"\x00\x2afine"), (0, 0x03, &[1])]));
    // A move record over the limit.
    b.game(&move_record(0, None, None, &vec![0; MAX_GAME_BYTES]));
    // An annotation record over the limit: 40 comments of 60,000 bytes.
    b.game(&e4);
    let mut long = b"\x00\x2a".to_vec();
    long.resize(60_000, b'x');
    let items: Vec<(i32, u8, &[u8])> = (0..40).map(|_| (0, 0x02, &long[..])).collect();
    b.annotations(&annotation_record(3, &items));
    let f = b.write("classic-api-limits");
    let path = f.dir().join("db.cbh");
    let dir = index_dir("limits");
    let (port, _app) = start_with_dir([path.clone()], &dir);
    let id = id_of(&path);

    let (status, body) = get(port, &format!("/v1/databases/{id}/games/1?lang=en"));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""annotations":"complete""#), "{body}");
    assert!(body.contains("1. e4 $1 {fine} 1-0"), "{body}");
    for number in [2, 3] {
        let (status, body) = get(port, &format!("/v1/databases/{id}/games/{number}"));
        assert_eq!(status, 422, "{body}");
        assert!(body.contains(r#""code":"unreadable_game""#) && body.contains("-byte limit"), "{body}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
