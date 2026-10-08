//! The database window list, from synthetic files built in the documented layout.

use std::path::PathBuf;

use cbformat::codepage::CodePage;
use cbformat::dbitems::{self, Value};
use cbformat::fixture::DbItems;
use cbformat::view::Format;

/// The code page of the lists that hold only UTF-8 and ASCII.
const PAGE: CodePage = CodePage::WESTERN;

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
    let list = dbitems::parse(&sample().bytes(), PAGE).unwrap();
    let names: Vec<&str> = list.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["Big Base", "Чорні - репертуар", "Club, 2026 games", "Old", "Downloads (pgn)"]);
    let formats: Vec<Option<Format>> = list.entries.iter().map(|e| e.format).collect();
    assert_eq!(formats, [Format::TwoCbh, Format::TwoCbh, Format::TwoCbh, Format::Cbh, Format::Pgn].map(Some));
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
    let list = dbitems::parse(&f.bytes(), PAGE).unwrap();
    assert_eq!(list.sort_dir, [None, None, None, Some(1), None, None, None, None]);
    assert_eq!(list.sort, None);
}

/// The format is the one the stored path's extension names, in any case.
#[test]
fn an_entry_of_another_format_has_none() {
    let mut f = DbItems::new();
    f.section("Databases")
        .database(r"C:\x\A.2CBH", "A", [0, 28, 1, 0, 0, 0])
        .database(r"C:\x\B.PGN", "B", [0, 3, 1, 0, 0, 0])
        .database(r"C:\x\C.cbv", "C", [0, 0, 1, 0, 0, 0])
        .database(r"C:\x.cbh\D", "D", [0, 0, 1, 0, 0, 0]);
    let list = dbitems::parse(&f.bytes(), PAGE).unwrap();
    let formats: Vec<Option<Format>> = list.entries.iter().map(|e| e.format).collect();
    assert_eq!(formats, [Some(Format::TwoCbh), Some(Format::Pgn), None, None]);
}

#[test]
fn an_entry_before_any_section_has_none() {
    let mut f = DbItems::new();
    f.database(r"C:\x\A.2cbh", "A", [0, 28, 1, 0, 0, 0]);
    let list = dbitems::parse(&f.bytes(), PAGE).unwrap();
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
    let list = dbitems::parse(&bytes, PAGE).unwrap();
    assert_eq!(list.entries.len(), 10_000);
    assert_eq!(list.sections.len(), 1);
    let held: usize = list.sections.iter().map(String::len).sum::<usize>()
        + list.entries.iter().map(|e| e.path.len() + e.name.len()).sum::<usize>();
    assert!(held <= 2 * bytes.len(), "{held} bytes held for a {}-byte file", bytes.len());
    assert!(list.entries.iter().all(|e| list.section_of(e).map(str::len) == Some(512 << 10)));
}

#[test]
fn every_item_is_read_to_the_end() {
    let items = dbitems::items(&sample().bytes(), PAGE).unwrap();
    assert_eq!(items.len(), 5 + 5 + 1 + 3 + 8);
    assert_eq!(items[0].value, Value::Section);
    assert_eq!(items[0].key, "2cbg");
    assert_eq!(items.last().unwrap().value, Value::Byte(1));
    assert!(matches!(items[2].value, Value::Text { tag: 0x1e, .. }));
}

#[test]
fn an_empty_list_has_no_entries() {
    let list = dbitems::parse(&DbItems::new().bytes(), PAGE).unwrap();
    assert!(list.entries.is_empty());
    assert_eq!(list.reference, None);
}

/// Paths and titles that are not UTF-8 are read in the computer's code page,
/// as ChessBase reads them: an older ChessBase wrote them in it, as
/// Windows-1251 on a Cyrillic Windows (#288).
#[test]
fn paths_and_titles_that_are_not_utf8_are_read_in_the_code_page() {
    let cyrillic = CodePage::new(1251);
    let mut f = DbItems::new();
    f.section("Databases")
        // `D:\Уроки\Эндшпиль.cbh`, titled `Эндшпиль`, in Windows-1251.
        .text(
            0x19,
            b"D:\\\xd3\xf0\xee\xea\xe8\\\xdd\xed\xe4\xf8\xef\xe8\xeb\xfc.cbh",
            b"\xdd\xed\xe4\xf8\xef\xe8\xeb\xfc,0,1,2,3,4,5",
        )
        // An ASCII title in a folder named in Windows-1251.
        .text(0x19, b"D:\\\xd3\xf0\xee\xea\xe8\\Strategy.cbh", b"Strategy,0,1,2,3,4,5")
        .section("Status")
        .text(0x19, b"Selected", b"D:\\\xd3\xf0\xee\xea\xe8\\Strategy.cbh");
    let list = dbitems::parse(&f.bytes(), cyrillic).unwrap();
    let read: Vec<(&str, &str)> = list.entries.iter().map(|e| (e.path.as_str(), e.name.as_str())).collect();
    assert_eq!(read, [(r"D:\Уроки\Эндшпиль.cbh", "Эндшпиль"), (r"D:\Уроки\Strategy.cbh", "Strategy")]);
    assert_eq!(list.selected.as_deref(), Some(r"D:\Уроки\Strategy.cbh"));
    assert_eq!(list.entries[0].format, Some(Format::Cbh));

    // The same bytes on a Western computer, and UTF-8 on any.
    let mut f = DbItems::new();
    f.section("Databases").text(0x19, br"C:\x\M.cbh", b"M\xfcller,0,1,2,3,4,5").database(
        r"C:\x\Чорні.cbh",
        "Чорні",
        [0, 1, 2, 3, 4, 5],
    );
    let names = |page| -> Vec<String> {
        dbitems::parse(&f.bytes(), page).unwrap().entries.into_iter().map(|e| e.name).collect()
    };
    assert_eq!(names(PAGE), ["Müller", "Чорні"]);
    assert_eq!(names(cyrillic), ["Mьller", "Чорні"]);
}

/// A title ChessBase stored as the database's file name with each UTF-16
/// unit cut to its low byte reads as the file name (#288). A title of its
/// own stays as stored, also one that starts with the image of its file name.
#[test]
fn a_title_cut_to_one_byte_reads_as_the_file_name() {
    let mut f = DbItems::new();
    f.section("Databases")
        // `Ладья`, cut: Л is U+041B, а U+0430, д U+0434, ь U+044C, я U+044F.
        .database(r"D:\Уроки\Ладья.cbh", "\u{1b}04LO", [0, 1, 66, 0, 0, 0])
        .database(r"D:\Уроки\Ладья 2.cbh", "Rook endings", [0, 1, 66, 0, 0, 0])
        .database(r"D:\Уроки\Ферзь.cbh", "\u{1b}04LO", [0, 1, 66, 0, 0, 0])
        .database(r"D:\Уроки\Rook.cbh", "Rook", [0, 1, 66, 0, 0, 0])
        // `ё` cuts to `Q`, and `абв` to `012`.
        .database(r"D:\ё.cbh", "Queen endings", [0, 1, 66, 0, 0, 0])
        .database(r"D:\абв.cbh", "012345 games", [0, 1, 66, 0, 0, 0])
        .database(r"D:\Ладья.cbh", "\u{1b}04LO (cbh)", [0, 1, 66, 0, 0, 0]);
    let list = dbitems::parse(&f.bytes(), PAGE).unwrap();
    let names: Vec<&str> = list.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        ["Ладья", "Rook endings", "\u{1b}04LO", "Rook", "Queen endings", "012345 games", "\u{1b}04LO (cbh)"]
    );
}

#[test]
fn damaged_files_are_errors_never_panics() {
    let good = sample().bytes();
    // Every truncation, with the header length kept or fixed up.
    for len in 0..good.len() {
        let mut cut = good[..len].to_vec();
        assert!(dbitems::parse(&cut, PAGE).is_err(), "truncated to {len}");
        if len >= 8 {
            cut[4..8].copy_from_slice(&((len - 8) as u32).to_be_bytes());
            let _ = dbitems::parse(&cut, PAGE);
        }
    }
    // Every single-byte change.
    for i in 0..good.len() {
        for v in [0x00, 0x7f, 0x80, 0xff] {
            let mut b = good.clone();
            b[i] = v;
            let _ = dbitems::parse(&b, PAGE);
        }
    }
    let bad_magic = [&[0u8, 1, 2, 3][..], &good[4..]].concat();
    assert!(dbitems::parse(&bad_magic, PAGE).is_err());
    let mut unknown = DbItems::new();
    unknown.section("x").raw(&[0x42]);
    let err = dbitems::parse(&unknown.bytes(), PAGE).unwrap_err().to_string();
    assert!(err.contains("unknown item tag 0x42"), "{err}");
    let mut negative = DbItems::new();
    negative.raw(&[0xff]).raw(&(-1i32).to_le_bytes());
    assert!(dbitems::parse(&negative.bytes(), PAGE).is_err());
    let mut long = DbItems::new();
    long.raw(&[0xff]).raw(&i32::MAX.to_le_bytes());
    assert!(dbitems::parse(&long.bytes(), PAGE).is_err());
    assert!(dbitems::parse(&vec![0u8; dbitems::MAX_FILE as usize + 1], PAGE).is_err());
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
    assert_eq!(dbitems::read(&empty.0, PAGE).unwrap(), None);
    assert_eq!(dbitems::locate(&empty.0).unwrap(), Default::default());

    let copy_only = dir("copy-only");
    std::fs::write(copy_only.0.join("DBItems-NOTEBOOK.cbini"), sample().bytes()).unwrap();
    let located = dbitems::locate(&copy_only.0).unwrap();
    assert_eq!(located.file, None);
    assert_eq!(located.conflict_copies, [copy_only.0.join("DBItems-NOTEBOOK.cbini")]);
    assert_eq!(dbitems::read(&copy_only.0, PAGE).unwrap(), None);

    let both = dir("both");
    let mut other = DbItems::new();
    other.section("2cbg").database(r"C:\x\Other.2cbh", "Other", [0, 28, 1, 1, 1, 1]);
    std::fs::write(both.0.join("DBItems.cbini"), sample().bytes()).unwrap();
    std::fs::write(both.0.join("DBItems-NOTEBOOK.cbini"), other.bytes()).unwrap();
    let located = dbitems::locate(&both.0).unwrap();
    assert_eq!(located.file, Some(both.0.join("DBItems.cbini")));
    assert_eq!(located.conflict_copies.len(), 1);
    assert_eq!(dbitems::read(&both.0, PAGE).unwrap().unwrap().entries.len(), 5);

    let corrupt = dir("corrupt");
    std::fs::write(corrupt.0.join("DBItems.cbini"), b"not a list").unwrap();
    assert!(dbitems::read(&corrupt.0, PAGE).is_err());

    // Larger than the bound: refused after reading just past it.
    let large = dir("large");
    let mut big = sample();
    big.section(&"x".repeat(dbitems::MAX_FILE as usize));
    std::fs::write(large.0.join("DBItems.cbini"), big.bytes()).unwrap();
    let err = dbitems::read(&large.0, PAGE).unwrap_err().to_string();
    assert!(err.contains("more than 1048576 bytes"), "{err}");
    assert!(dbitems::locate(&corrupt.0.join("missing")).is_err());
}
