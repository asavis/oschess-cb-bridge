//! A game's players and its tournament, as both formats name them, and all
//! the entities a game names together.

use super::Date;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Player {
    pub last: String,
    pub first: String,
}

impl Player {
    pub fn pgn(&self) -> String {
        if self.first.is_empty() { self.last.clone() } else { format!("{}, {}", self.last, self.first) }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tournament {
    pub title: String,
    pub place: String,
    pub start: Date,
}

/// The entities a game names; `None` for an unused or unreadable entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Names {
    pub white: Option<Player>,
    pub black: Option<Player>,
    pub tournament: Option<Tournament>,
    pub annotator: Option<String>,
}
