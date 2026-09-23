//! The database window list, from synthetic files built in the documented layout.

use std::path::PathBuf;

use cbformat::dbitems::{self, Format, Value};
use cbformat::fixture::DbItems;

/// A list shaped like the ones ChessBase writes: 2CBH databases in `2cbg`, the
/// reference database in `2cbh`, other formats in `Databases`, then the
/// window's own settings.
fn sample() -> DbItems {
    let mut f = DbItems::new();
    f.section("2cbg")
        .database(r"C:\Users\u\Documents\ChessBase\Bases\Big.2cbh", "Big Base", [0, 28, 11966514, 47, 1037620, 1037559])
        .database(
            r"C:\Users\u\Documents\ChessBase\MyWork\Чорні.2cbh",
            "Чорні - репертуар",
            [43, 28, 14, 1242, 1037623, 1037559],
        )
        .database(
            r"C:\Users\u\Documents\ChessBase\MyWork\Club, 2026.2cbh",
            "Club, 2026 games",
            [2, 28, 350, 1962, 1037623, 1037559],
        )
        .section("2cbh")
        .text(0x1a, b"RefDB", br"C:\Users\u\Documents\ChessBase\Bases\Big.2cbh")
        .section("Databases")
        .database(r"C:\Users\u\Documents\ChessBase\MyWork\Old.cbh", "", [0, 1, 350, 6, 1037623, 1037623])
        .database(
            r"C:\Users\u\Documents\ChessBase\MyWork\Downloads.pgn",
            "Downloads (pgn)",
            [0, 3, 9, 5, 1037616, 1037616],
        )
        .section("Pathes")
        .section("Status")
        .int("DesktopTop", 0)
        .text(0x19, b"Selected", br"C:\Users\u\Documents\ChessBase\MyWork\Old.cbh")
        .int("Sort", 6);
    for i in 0..8 {
        f.byte(&format!("SortDir{i}"), i % 2);
    }
    f
}

#[test]
fn entries_come_in_file_order_with_their_fields() {
    let list = dbitems::parse(&sample().bytes()).unwrap();
    let names: Vec<&str> = list.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["Big Base", "Чорні - репертуар", "Club, 2026 games", "Old", "Downloads (pgn)"]);
    let formats: Vec<Format> = list.entries.iter().map(|e| e.format).collect();
    assert_eq!(formats, [Format::Cbh2, Format::Cbh2, Format::Cbh2, Format::Cbh, Format::Pgn]);
    let sections: Vec<&str> = list.entries.iter().map(|e| list.section_of(e).unwrap()).collect();
    assert_eq!(sections, ["2cbg", "2cbg", "2cbg", "Databases", "Databases"]);
    assert_eq!(list.sections, ["2cbg", "2cbh", "Databases", "Pathes", "Status"]);
    let big = &list.entries[0];
    assert_eq!(big.path, r"C:\Users\u\Documents\ChessBase\Bases\Big.2cbh");
    assert_eq!((big.type_code(), big.games()), (28, 11966514));
    assert_eq!(big.last_used().pgn(), "2026.09.20");
    assert_eq!(big.added().pgn(), "2026.07.23");
    assert_eq!(list.entries[1].path, r"C:\Users\u\Documents\ChessBase\MyWork\Чорні.2cbh");
    assert_eq!(list.reference.as_deref(), Some(r"C:\Users\u\Documents\ChessBase\Bases\Big.2cbh"));
    assert_eq!(list.selected.as_deref(), Some(r"C:\Users\u\Documents\ChessBase\MyWork\Old.cbh"));
    assert_eq!(list.sort, Some(6));
    assert_eq!(list.sort_dir, [0, 1, 0, 1, 0, 1, 0, 1].map(Some));
}

#[test]
fn sort_directions_are_read_only_from_status() {
    let mut f = DbItems::new();
    f.section("Other").byte("SortDir0", 9).section("Status").byte("SortDir3", 1).byte("SortDir9", 1);
    let list = dbitems::parse(&f.bytes()).unwrap();
    assert_eq!(list.sort_dir, [None, None, None, Some(1), None, None, None, None]);
    assert_eq!(list.sort, None);
}

#[test]
fn an_entry_before_any_section_has_none() {
    let mut f = DbItems::new();
    f.database(r"C:\x\A.2cbh", "A", [0, 28, 1, 0, 0, 0]);
    let list = dbitems::parse(&f.bytes()).unwrap();
    assert_eq!(list.entries[0].section, None);
    assert_eq!(list.section_of(&list.entries[0]), None);
}

/// A section name of 512 KiB over 10,000 entries: each name is stored once, so
/// what the list holds stays within twice the file's size.
#[test]
fn a_long_section_name_is_not_copied_into_every_entry() {
    let mut f = DbItems::new();
    f.section(&"s".repeat(512 << 10));
    for _ in 0..10_000 {
        f.database("a.2cbh", "", [0, 28, 1, 0, 0, 0]);
    }
    let bytes = f.bytes();
    assert!(bytes.len() as u64 <= dbitems::MAX_FILE);
    let list = dbitems::parse(&bytes).unwrap();
    assert_eq!(list.entries.len(), 10_000);
    assert_eq!(list.sections.len(), 1);
    let held: usize = list.sections.iter().map(String::len).sum::<usize>()
        + list.entries.iter().map(|e| e.path.len() + e.name.len()).sum::<usize>();
    assert!(held <= 2 * bytes.len(), "{held} bytes held for a {}-byte file", bytes.len());
    assert!(list.entries.iter().all(|e| list.section_of(e).map(str::len) == Some(512 << 10)));
}

#[test]
fn every_item_is_read_to_the_end() {
    let items = dbitems::items(&sample().bytes()).unwrap();
    assert_eq!(items.len(), 5 + 5 + 1 + 3 + 8);
    assert_eq!(items[0].value, Value::Section);
    assert_eq!(items[0].key, "2cbg");
    assert_eq!(items.last().unwrap().value, Value::Byte(1));
    assert!(matches!(items[2].value, Value::Text { tag: 0x1e, .. }));
}

#[test]
fn an_empty_list_has_no_entries() {
    let list = dbitems::parse(&DbItems::new().bytes()).unwrap();
    assert!(list.entries.is_empty());
    assert_eq!(list.reference, None);
}

#[test]
fn a_non_utf8_title_is_read_as_latin1() {
    let mut f = DbItems::new();
    f.section("Databases").text(0x19, br"C:\x\M.cbh", b"M\xfcller,0,1,2,3,4,5");
    assert_eq!(dbitems::parse(&f.bytes()).unwrap().entries[0].name, "Müller");
}

#[test]
fn damaged_files_are_errors_never_panics() {
    let good = sample().bytes();
    // Every truncation, with the header length kept or fixed up.
    for len in 0..good.len() {
        let mut cut = good[..len].to_vec();
        assert!(dbitems::parse(&cut).is_err(), "truncated to {len}");
        if len >= 8 {
            cut[4..8].copy_from_slice(&((len - 8) as u32).to_be_bytes());
            let _ = dbitems::parse(&cut);
        }
    }
    // Every single-byte change.
    for i in 0..good.len() {
        for v in [0x00, 0x7f, 0x80, 0xff] {
            let mut b = good.clone();
            b[i] = v;
            let _ = dbitems::parse(&b);
        }
    }
    let bad_magic = [&[0u8, 1, 2, 3][..], &good[4..]].concat();
    assert!(dbitems::parse(&bad_magic).is_err());
    let mut unknown = DbItems::new();
    unknown.section("x").raw(&[0x42]);
    let err = dbitems::parse(&unknown.bytes()).unwrap_err().to_string();
    assert!(err.contains("unknown item tag 0x42"), "{err}");
    let mut negative = DbItems::new();
    negative.raw(&[0xff]).raw(&(-1i32).to_le_bytes());
    assert!(dbitems::parse(&negative.bytes()).is_err());
    let mut long = DbItems::new();
    long.raw(&[0xff]).raw(&i32::MAX.to_le_bytes());
    assert!(dbitems::parse(&long.bytes()).is_err());
    assert!(dbitems::parse(&vec![0u8; dbitems::MAX_FILE as usize + 1]).is_err());
}

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dir(name: &str) -> Dir {
    let d = std::env::temp_dir().join(format!("cbformat-dbitems-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    Dir(d)
}

#[test]
fn the_main_file_is_read_and_conflict_copies_are_not() {
    let empty = dir("none");
    assert_eq!(dbitems::read(&empty.0).unwrap(), None);
    assert_eq!(dbitems::locate(&empty.0).unwrap(), Default::default());

    let copy_only = dir("copy-only");
    std::fs::write(copy_only.0.join("DBItems-NOTEBOOK.cbini"), sample().bytes()).unwrap();
    let located = dbitems::locate(&copy_only.0).unwrap();
    assert_eq!(located.file, None);
    assert_eq!(located.conflict_copies, [copy_only.0.join("DBItems-NOTEBOOK.cbini")]);
    assert_eq!(dbitems::read(&copy_only.0).unwrap(), None);

    let both = dir("both");
    let mut other = DbItems::new();
    other.section("2cbg").database(r"C:\x\Other.2cbh", "Other", [0, 28, 1, 1, 1, 1]);
    std::fs::write(both.0.join("DBItems.cbini"), sample().bytes()).unwrap();
    std::fs::write(both.0.join("DBItems-NOTEBOOK.cbini"), other.bytes()).unwrap();
    let located = dbitems::locate(&both.0).unwrap();
    assert_eq!(located.file, Some(both.0.join("DBItems.cbini")));
    assert_eq!(located.conflict_copies.len(), 1);
    assert_eq!(dbitems::read(&both.0).unwrap().unwrap().entries.len(), 5);

    let corrupt = dir("corrupt");
    std::fs::write(corrupt.0.join("DBItems.cbini"), b"not a list").unwrap();
    assert!(dbitems::read(&corrupt.0).is_err());

    // Larger than the bound: refused after reading just past it.
    let large = dir("large");
    let mut big = sample();
    big.section(&"x".repeat(dbitems::MAX_FILE as usize));
    std::fs::write(large.0.join("DBItems.cbini"), big.bytes()).unwrap();
    let err = dbitems::read(&large.0).unwrap_err().to_string();
    assert!(err.contains("more than 1048576 bytes"), "{err}");
    assert!(dbitems::locate(&corrupt.0.join("missing")).is_err());
}
