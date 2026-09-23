//! Suggestions: names by prefix with their game counts, the most games first.

use std::cmp::Ordering as Order;
use std::sync::atomic::{AtomicU32, Ordering};

use cbformat::v2::{Database, RecordKind};

use super::memory::{Cancel, Held, Hold, Refused};
use super::names::{Groups, Kind, NO_GROUP, NameTable, groups};
use super::query::MAX_VALUE_CHARS;
use super::scan::{self, Control};
use super::{Indexes, SearchError, cached};

/// How many games have each name identity as a player, annotator and tournament.
pub(super) struct Counts {
    players: Vec<u32>,
    annotators: Vec<u32>,
    tournaments: Vec<u32>,
}

fn counts(
    db: &Database,
    ctl: &Control<'_>,
    players: &Groups,
    tournaments: &Groups,
) -> Result<Held<Counts>, SearchError> {
    let (np, nt) = (players.first_id.len(), tournaments.first_id.len());
    let hold = Hold::reserve((2 * np + nt) * 4)?;
    let zeros = |n: usize| -> Result<Vec<AtomicU32>, Refused> {
        let mut v = Vec::new();
        v.try_reserve_exact(n).map_err(|_| Refused::Busy)?;
        v.extend((0..n).map(|_| AtomicU32::new(0)));
        Ok(v)
    };
    let (p, a, t) = (zeros(np)?, zeros(np)?, zeros(nt)?);
    let group =
        |g: &Groups, id: i64| usize::try_from(id).ok().and_then(|i| g.of_id.get(i)).copied().filter(|&x| x != NO_GROUP);
    let bump = |v: &[AtomicU32], g: Option<u32>| {
        if let Some(g) = g {
            v[g as usize].fetch_add(1, Ordering::Relaxed);
        }
    };
    scan::scan(
        db,
        ctl,
        |_| Ok(()),
        |_, r| {
            if matches!(r.kind(), RecordKind::Game) {
                // A game counts once for a name, whichever colours carry it.
                let (w, b) = (group(players, r.white()), group(players, r.black()));
                bump(&p, w);
                if b != w {
                    bump(&p, b);
                }
                bump(&a, group(players, r.annotator()));
                bump(&t, group(tournaments, r.tournament()));
            }
            Ok(())
        },
        |_| {},
    )?;
    let plain = |v: Vec<AtomicU32>| v.into_iter().map(AtomicU32::into_inner).collect();
    Ok(Held::new(Counts { players: plain(p), annotators: plain(a), tournaments: plain(t) }, hold))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuggestField {
    Player,
    Event,
    Annotator,
}

/// A name offered for a field, complete, and its game count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    pub name: String,
    pub games: u32,
}

/// Whether `name` can be searched exactly: a quoted qualifier value holds at
/// most [`MAX_VALUE_CHARS`] characters and no double quote.
fn searchable(name: &str) -> bool {
    !name.contains('"') && (name.len() <= MAX_VALUE_CHARS || name.chars().count() <= MAX_VALUE_CHARS)
}

/// Up to `limit` names of `field` starting with `prefix` (or whose first name
/// does, for people), with their game counts: most games first, then by name.
/// Only the best `limit` are kept while the names are looked through, and the
/// copies returned hold their bytes in the budget.
pub fn suggest(
    db: &Database,
    idx: &Indexes,
    field: SuggestField,
    prefix: &str,
    limit: usize,
) -> Result<Held<Vec<Suggestion>>, SearchError> {
    let never = Cancel::never();
    let ctl = Control { cancel: &never, scanned: &idx.scanned };
    let players = idx.names(db, Kind::Players, &never)?;
    let tournaments = idx.names(db, Kind::Tournaments, &never)?;
    let player_groups = cached(&idx.player_groups, || groups(&players))?;
    let tournament_groups = cached(&idx.tournament_groups, || groups(&tournaments))?;
    let counts = cached(&idx.counts, || counts(db, &ctl, &player_groups, &tournament_groups))?;
    let (table, groups, games): (&NameTable, &Groups, &[u32]) = match field {
        SuggestField::Player => (&players, &player_groups, &counts.players),
        SuggestField::Annotator => (&players, &player_groups, &counts.annotators),
        SuggestField::Event => (&tournaments, &tournament_groups, &counts.tournaments),
    };
    let people = matches!(field, SuggestField::Player | SuggestField::Annotator);
    let prefix = prefix.trim().to_lowercase();
    if limit == 0 {
        return Ok(Held::new(Vec::new(), Hold::default()));
    }
    // Most games first, then the name ignoring case, then as shown; compared in
    // place, without copies.
    let order = |a: (u32, usize), b: (u32, usize)| -> Order {
        b.0.cmp(&a.0)
            .then_with(|| table.lower(a.1).cmp(table.lower(b.1)))
            .then_with(|| table.name(a.1 as i64).cmp(table.name(b.1 as i64)))
    };
    let mut best: Vec<(u32, usize)> = Vec::new();
    best.try_reserve_exact(limit + 1).map_err(|_| Refused::Busy)?;
    for (g, &n) in games.iter().enumerate() {
        let id = groups.first_id[g] as usize;
        if n == 0 || (best.len() == limit && order((n, id), best[limit - 1]) != Order::Less) {
            continue;
        }
        let lower = table.lower(id);
        // A person's first name counts too; an event's name after a comma does not.
        let first_name = if people { lower.split_once(", ").map(|(_, f)| f) } else { None };
        if !(lower.starts_with(&prefix) || first_name.is_some_and(|f| f.starts_with(&prefix))) {
            continue;
        }
        if !searchable(table.name(id as i64)) {
            continue;
        }
        let at = best.partition_point(|&x| order(x, (n, id)) == Order::Less);
        best.insert(at, (n, id));
        best.truncate(limit);
    }
    let bytes: usize = best.iter().map(|&(_, id)| table.name(id as i64).len()).sum();
    let hold = Hold::reserve(bytes + best.len() * std::mem::size_of::<Suggestion>())?;
    let list =
        best.into_iter().map(|(games, id)| Suggestion { name: table.name(id as i64).to_string(), games }).collect();
    Ok(Held::new(list, hold))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn searchable_names() {
        assert!(searchable("Carlsen, Magnus"));
        assert!(searchable(&"é".repeat(MAX_VALUE_CHARS)));
        assert!(!searchable(&"é".repeat(MAX_VALUE_CHARS + 1)));
        assert!(!searchable("The \"Hedgehog\""));
    }
}
