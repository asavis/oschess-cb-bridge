//! The order ChessBase's database window shows its databases in.

use std::cmp::Ordering;

use super::{DbList, Entry};

/// The `Sort` value of a window in ChessBase's default order: by icon.
pub const SORT_BY_ICON: i32 = 6;

impl Entry {
    /// The first number after the title: the icon the window shows. Entries
    /// with the default icon of their format hold 0.
    pub fn icon(&self) -> i64 {
        self.numbers[0]
    }
}

impl DbList {
    /// The entries in the order the window shows them.
    ///
    /// With `Sort` 6, the only setting seen, ChessBase orders by icon number,
    /// largest first, and entries with the same icon by title, last first. The
    /// window showed that order in its icon and detail views on the owner's
    /// machine (asavis/oschess-cb-bridge#20). Any other setting keeps the file
    /// order, since its meaning is not known.
    pub fn window_order(&self) -> Vec<&Entry> {
        let mut entries: Vec<&Entry> = self.entries.iter().collect();
        if self.sort == Some(SORT_BY_ICON) {
            entries.sort_by(|a, b| by_icon(a, b));
        }
        entries
    }

    /// [`Self::window_order`], taking the entries.
    pub fn into_window_order(mut self) -> Vec<Entry> {
        if self.sort == Some(SORT_BY_ICON) {
            self.entries.sort_by(by_icon);
        }
        self.entries
    }
}

fn by_icon(a: &Entry, b: &Entry) -> Ordering {
    b.icon().cmp(&a.icon()).then_with(|| b.name.to_lowercase().cmp(&a.name.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbitems::Format;

    fn entry(name: &str, icon: i64) -> Entry {
        Entry {
            path: format!("C:\\Bases\\{name}.2cbh"),
            name: name.to_owned(),
            format: Format::of(&format!("{name}.2cbh")),
            section: None,
            numbers: [icon, 28, 1, 1, 1_037_624, 1_037_559],
        }
    }

    fn names(list: &[&Entry]) -> Vec<String> {
        list.iter().map(|e| e.name.clone()).collect()
    }

    #[test]
    fn sort_six_orders_by_icon_then_title_both_descending() {
        // The window's shape on the owner's machine, with invented titles.
        let file = [
            ("Morphy games", 0),
            ("Downloads", 26),
            ("Black repertoire", 43),
            ("White repertoire", 42),
            ("Club games", 2),
            ("Openings", 245),
            ("Archive", 34),
            ("Course", 35),
            ("Anderssen [pgn]", 0),
            ("Tal (cbh)", 0),
        ];
        let mut list =
            DbList { entries: file.iter().map(|(n, i)| entry(n, *i)).collect(), sort: Some(6), ..DbList::default() };
        let want = [
            "Openings",
            "Black repertoire",
            "White repertoire",
            "Course",
            "Archive",
            "Downloads",
            "Club games",
            "Tal (cbh)",
            "Morphy games",
            "Anderssen [pgn]",
        ];
        assert_eq!(names(&list.window_order()), want);
        assert_eq!(list.clone().into_window_order().iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), want);
        // Titles compare without case.
        list.entries = vec![entry("apple", 0), entry("Banana", 0)];
        assert_eq!(names(&list.window_order()), ["Banana", "apple"]);
    }

    #[test]
    fn any_other_sort_keeps_the_file_order() {
        let entries = vec![entry("A", 1), entry("B", 9)];
        for sort in [None, Some(0), Some(5), Some(7)] {
            let list = DbList { entries: entries.clone(), sort, ..DbList::default() };
            assert_eq!(names(&list.window_order()), ["A", "B"], "{sort:?}");
        }
    }
}
