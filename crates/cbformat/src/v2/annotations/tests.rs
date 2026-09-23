//! Decoding of synthetic annotation records.
use super::*;

fn block(position: i32, anns: &[Vec<u8>]) -> Vec<u8> {
    let mut v = position.to_le_bytes().to_vec();
    v.extend((anns.len() as i32).to_le_bytes());
    for a in anns {
        v.extend(a);
    }
    v
}

fn text(t: u16, language: u16, s: &[u8]) -> Vec<u8> {
    let mut v = t.to_le_bytes().to_vec();
    v.extend([0, 0]);
    v.extend(language.to_le_bytes());
    v.extend((s.len() as i32).to_le_bytes());
    v.extend(s);
    v
}

fn end(mut v: Vec<u8>) -> Vec<u8> {
    v.extend(END_MARKER.to_le_bytes());
    v
}

#[test]
fn empty_record() {
    let a = GameAnnotations::parse(&end(Vec::new())).unwrap();
    assert!(a.is_empty());
}

#[test]
fn texts_symbols_squares_arrows() {
    let mut c = block(-1, &[text(2, 0, b"Game")]);
    c.extend(block(
        0,
        &[
            vec![3, 0, 1, 18, 142],
            vec![4, 0, 4, 0, 0, 0, 2, 1, 4, 64],
            vec![5, 0, 3, 0, 0, 0, 3, 13, 29],
            text(0x82, 1, b"Vor"),
        ],
    ));
    let a = GameAnnotations::parse(&end(c)).unwrap();
    assert_eq!(a.stopped_at, None);
    assert_eq!(a.blocks[0].annotations, [Annotation::Text { before: false, language: 0, text: "Game".into() }]);
    let b = &a.blocks[1].annotations;
    assert_eq!(b[0], Annotation::Symbols { on_move: 1, on_position: 18, prefix: 142 });
    // 1 is a1, 64 is h8; 13 is b5 and 29 d5 (file by file, from 1).
    assert_eq!(b[1], Annotation::Squares(vec![Square { colour: 2, square: 0 }, Square { colour: 4, square: 63 }]));
    assert_eq!(b[2], Annotation::Arrows(vec![Arrow { colour: 3, from: 33, to: 35 }]));
    assert_eq!(b[3], Annotation::Text { before: true, language: 1, text: "Vor".into() });
}

#[test]
fn cp1252_fallback() {
    let a = GameAnnotations::parse(&end(block(0, &[text(2, 0, &[b'a', 0x93, 0xe9])]))).unwrap();
    assert_eq!(a.blocks[0].annotations[0], Annotation::Text { before: false, language: 0, text: "a“é".into() });
}

#[test]
fn unknown_layout_stops_with_what_came_before() {
    let mut c = block(0, &[text(2, 0, b"kept"), vec![0x1a, 0, 1, 2, 3], text(2, 0, b"lost")]);
    c.extend(block(5, &[text(2, 0, b"lost too")]));
    let a = GameAnnotations::parse(&end(c)).unwrap();
    assert_eq!(a.stopped_at, Some(Unknown { position: 0, type_code: 0x1a }));
    assert_eq!(a.blocks.len(), 1);
    assert_eq!(a.blocks[0].annotations.len(), 1);
    assert!(!a.is_empty());
}

#[test]
fn known_types_are_skipped_by_layout() {
    let c = block(
        3,
        &[
            vec![0x18, 0, 1],
            vec![0x22, 0, 1, 2, 3, 4],
            vec![0x27, 0, 1, 0],
            vec![0x26, 0, 1, 2, 0, 0, 0, 9, 9],
            text(2, 0, b"after"),
        ],
    );
    let a = GameAnnotations::parse(&end(c)).unwrap();
    let b = &a.blocks[0].annotations;
    assert_eq!(
        b[..4],
        [Annotation::Other(0x18), Annotation::Other(0x22), Annotation::Other(0x27), Annotation::Other(0x26)]
    );
    assert_eq!(b[4], Annotation::Text { before: false, language: 0, text: "after".into() });
}

#[test]
fn damage_is_an_error() {
    let good = end(block(0, &[text(2, 0, b"abc")]));
    for cut in 0..good.len() {
        assert!(GameAnnotations::parse(&good[..cut]).is_err(), "cut at {cut}");
    }
    let mut trailing = good.clone();
    trailing.push(0);
    assert!(GameAnnotations::parse(&trailing).is_err());
    // A length far past the end, a huge count, a square of 0 or 65.
    assert!(GameAnnotations::parse(&end(block(0, &[vec![2, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0x7f]]))).is_err());
    let mut huge = 0i32.to_le_bytes().to_vec();
    huge.extend(i32::MAX.to_le_bytes());
    assert!(GameAnnotations::parse(&end(huge)).is_err());
    assert!(GameAnnotations::parse(&end(block(0, &[vec![4, 0, 2, 0, 0, 0, 2, 0]]))).is_err());
    assert!(GameAnnotations::parse(&end(block(0, &[vec![4, 0, 2, 0, 0, 0, 2, 65]]))).is_err());
    assert!(GameAnnotations::parse(&end(block(0, &[vec![5, 0, 2, 0, 0, 0, 2, 1]]))).is_err());
}
