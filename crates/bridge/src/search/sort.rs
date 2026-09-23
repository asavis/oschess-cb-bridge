//! Sort keys and directions, as `sort` parameters and `sort:` tokens name them.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SortKey {
    Number,
    White,
    Black,
    WhiteElo,
    BlackElo,
    Result,
    Moves,
    Eco,
    Tournament,
    Date,
    Round,
    Annotator,
}

impl SortKey {
    pub fn of(name: &str) -> Option<SortKey> {
        Some(match name.to_ascii_lowercase().as_str() {
            "number" => SortKey::Number,
            "white" => SortKey::White,
            "black" => SortKey::Black,
            "whiteelo" => SortKey::WhiteElo,
            "blackelo" => SortKey::BlackElo,
            "result" => SortKey::Result,
            "moves" => SortKey::Moves,
            "eco" => SortKey::Eco,
            "tournament" | "event" => SortKey::Tournament,
            "date" | "pgndate" => SortKey::Date,
            "round" => SortKey::Round,
            "annotator" => SortKey::Annotator,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            SortKey::Number => "number",
            SortKey::White => "white",
            SortKey::Black => "black",
            SortKey::WhiteElo => "whiteElo",
            SortKey::BlackElo => "blackElo",
            SortKey::Result => "result",
            SortKey::Moves => "moves",
            SortKey::Eco => "eco",
            SortKey::Tournament => "tournament",
            SortKey::Date => "date",
            SortKey::Round => "round",
            SortKey::Annotator => "annotator",
        }
    }

    /// Dates and move counts read best newest and longest first.
    fn descending_by_default(self) -> bool {
        matches!(self, SortKey::Date | SortKey::Moves)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
}

impl Sort {
    pub const DEFAULT: Sort = Sort { key: SortKey::Number, descending: false };

    /// `date`, `date-asc` or `date-desc`; `None` for an unknown key.
    pub fn parse(text: &str) -> Option<Sort> {
        let text = text.trim();
        let lower = text.to_ascii_lowercase();
        let (key, explicit) = if let Some(k) = lower.strip_suffix("-desc") {
            (k, Some(true))
        } else if let Some(k) = lower.strip_suffix("-asc") {
            (k, Some(false))
        } else {
            (lower.as_str(), None)
        };
        let key = SortKey::of(key)?;
        Some(Sort { key, descending: explicit.unwrap_or(key.descending_by_default()) })
    }

    pub fn name(self) -> String {
        format!("{}-{}", self.key.name(), if self.descending { "desc" } else { "asc" })
    }
}
