//! `GET /v1/databases/{id}/texts/{number}` (asavis/oschess-cb-bridge#324): a
//! guiding text's body, with its game links and text links resolved to the
//! records of its database. The records are built by hand; the names are made
//! up.

use bridge::catalog::id_of;
use cbformat::fixture_cbh::guide::{body, content_v1, counted, diagram, formatting, game_link, style};
use cbformat::fixture_cbh::{Builder, Tok, encode, move_record};
use cbformat::game::language;
use chesscore::Board;

mod common;
use common::{TestBridge, app_of, get, has_members, member};

fn e4() -> Vec<u8> {
    move_record(0, None, None, &encode(&Board::startpos(), &[Tok::Mv("e2e4"), Tok::End], 0, false))
}

/// Records 1-2 two games of the same players in two tournaments, 3 a text
/// titled `Index`, and 4 a text linking both games, the index, and a game
/// and a text that are not in the database.
fn database(name: &str) -> cbformat::fixture::TempDb {
    let mut b = Builder::new();
    let (white, black) = (b.player("Doe", "Jane"), b.player("Roe", "Richard"));
    let (open, cup) = (b.tournament("Testville op", "Testville"), b.tournament("Other cup", "Elsewhere"));
    for tournament in [open, cup] {
        let g = b.game(&e4());
        g[0x09..0x0c].copy_from_slice(&white.to_be_bytes()[1..]);
        g[0x0c..0x0f].copy_from_slice(&black.to_be_bytes()[1..]);
        g[0x0f..0x12].copy_from_slice(&tournament.to_be_bytes()[1..]);
    }
    let index = b"Contents";
    let f = formatting(&[], &[(0, style("Arial", 18, false, false))], &[(index.len(), 0)], false);
    b.text_body(&body(1, b"Index", 1, &content_v1(language::ENGLISH, index, &f)));
    let t = b"See \x04, \x04, \x04, \x04 and \x04.";
    let markers: Vec<usize> = t.iter().enumerate().filter(|&(_, &c)| c == 4).map(|(i, _)| i + 1).collect();
    let objects = [
        (0x1a, markers[0], game_link("Doe,Jane", "Roe,Richard", "Testville op", Some("1.1"))),
        (0x1a, markers[1], game_link("Doe,Jane", "Roe,Richard", "Other cup", Some("1.2"))),
        (0x1a, markers[2], game_link("Nobody,Here", "", "", Some("1.3"))),
        (0x05, markers[3], counted(b"Index")),
        (0x05, markers[4], counted(b"Missing")),
        (0x09, 0, diagram(&[("e1", 1)], 0)),
    ];
    let f = formatting(&objects, &[(0, style("Arial", 18, true, false))], &[(t.len(), 0)], false);
    b.text_body(&body(1, b"Chapter", 1, &content_v1(language::ANY, t, &f)))[0x0d..0x10].copy_from_slice(&[0, 0, 0]);
    b.write(name)
}

#[test]
fn a_text_is_served_with_its_links_resolved() {
    let db = database("texts-api");
    let path = db.dir().join("db.cbh");
    let bridge = TestBridge::new(app_of([path.clone()]));
    let (port, id) = (bridge.port, id_of(&path));
    let text = |n: &str| get(port, &format!("/v1/databases/{id}/texts/{n}"));

    let (status, body) = text("4");
    assert_eq!(status, 200, "{body}");
    assert!(has_members(&body, r#""number":4,"title":"Chapter","author":"""#), "{body}");
    let contents = member(&body, "contents");
    assert!(contents.starts_with(r#"[{"lang":"any","paragraphs":[{"spans":[{"text":"See ","font":"Arial","size":18,"bold":true,"italic":false,"underline":false}"#), "{contents}");
    for link in [
        r#"{"game":{"label":"1.1","white":"Doe, Jane","black":"Roe, Richard","event":"Testville op","number":1}}"#,
        r#"{"game":{"label":"1.2","white":"Doe, Jane","black":"Roe, Richard","event":"Other cup","number":2}}"#,
        r#"{"game":{"label":"1.3","white":"Nobody, Here","black":"","event":"","number":null}}"#,
        r#"{"textLink":{"title":"Index","number":3}}"#,
        r#"{"textLink":{"title":"Missing","number":null}}"#,
    ] {
        assert!(contents.contains(link), "{link}: {contents}");
    }
    // An object at position 0 stands at no marker and is left out.
    assert!(!contents.contains("diagram"), "{contents}");

    let (status, body) = text("1");
    assert_eq!((status, body.contains(r#""code":"not_a_text""#)), (422, true), "{body}");
    for n in ["0", "5", "x", "4294967296"] {
        assert_eq!(text(n).0, 404, "{n}");
    }
    let (status, body) = get(port, "/v1/status");
    assert_eq!(status, 200);
    assert!(body.contains(r#""guidingTexts""#), "{body}");
}

/// A text whose record is damaged is `422 unreadable_text`, and a game is not
/// served as a text.
#[test]
fn a_damaged_text_is_unreadable() {
    let mut b = Builder::new();
    b.game(&e4());
    b.text_body(&body(9, b"Chapter", 0, &[]));
    let db = b.write("texts-damaged");
    let path = db.dir().join("db.cbh");
    let bridge = TestBridge::new(app_of([path.clone()]));
    let id = id_of(&path);
    let (status, body) = get(bridge.port, &format!("/v1/databases/{id}/texts/2"));
    assert_eq!((status, body.contains(r#""code":"unreadable_text""#)), (422, true), "{body}");
    assert!(body.contains("reason"), "{body}");
}

/// Links compare names and titles as stored, never cut for display: a title
/// of 210 characters finds its text. A game link that leaves white empty
/// matches any white; one that names nothing names no game.
#[test]
fn links_match_stored_names_and_empty_fields_match_anything() {
    let mut b = Builder::new();
    let (white, black) = (b.player("Doe", "Jane"), b.player("Roe", "Richard"));
    let g = b.game(&e4());
    g[0x09..0x0c].copy_from_slice(&white.to_be_bytes()[1..]);
    g[0x0c..0x0f].copy_from_slice(&black.to_be_bytes()[1..]);
    let long = "T".repeat(210);
    let f = formatting(&[], &[(0, style("Arial", 18, false, false))], &[(4, 0)], false);
    b.text_body(&body(1, long.as_bytes(), 1, &content_v1(language::ENGLISH, b"Long", &f)));
    let t = b"\x04 \x04 \x04";
    let objects = [
        (0x05, 1, counted(long.as_bytes())),
        (0x1a, 3, game_link("", "Roe,Richard", "", Some("2.1"))),
        (0x1a, 5, game_link("", "", "", Some("2.2"))),
    ];
    let f = formatting(&objects, &[(0, style("Arial", 18, false, false))], &[(t.len(), 0)], false);
    b.text_body(&body(1, b"Links", 1, &content_v1(language::ANY, t, &f)));
    let db = b.write("texts-stored-names");
    let path = db.dir().join("db.cbh");
    let bridge = TestBridge::new(app_of([path.clone()]));
    let (status, body) = get(bridge.port, &format!("/v1/databases/{}/texts/3", id_of(&path)));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(&format!(r#"{{"textLink":{{"title":"{long}","number":2}}}}"#)), "{body}");
    assert!(body.contains(r#""label":"2.1","white":"","black":"Roe, Richard","event":"","number":1"#), "{body}");
    assert!(body.contains(r#""label":"2.2","white":"","black":"","event":"","number":null"#), "{body}");
}

/// An answer over the limit is refused, before it is built: an HTML text of
/// control characters, which JSON writes six bytes each.
#[test]
fn a_text_too_large_to_serve_is_unreadable() {
    let html = vec![1u8; 1_500_000];
    let mut content = language::ENGLISH.to_le_bytes().to_vec();
    content.extend((html.len() as u32).to_le_bytes());
    content.extend(&html);
    content.extend([0; 4]);
    let mut b = Builder::new();
    b.text_body(&body(3, b"Big", 1, &content));
    let db = b.write("texts-too-large");
    let path = db.dir().join("db.cbh");
    let bridge = TestBridge::new(app_of([path.clone()]));
    let (status, body) = get(bridge.port, &format!("/v1/databases/{}/texts/1", id_of(&path)));
    assert_eq!((status, body.contains(r#""code":"unreadable_text""#)), (422, true), "{}", &body[..body.len().min(300)]);
    assert!(body.contains("over the"), "{body}");
}
