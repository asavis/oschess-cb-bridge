//! How much of a database one game may make a reader take, so that a damaged
//! or hostile file costs a bounded amount of memory and work (#168).

/// The bounds of reading one game. No entry point renders a game without
/// them: [`crate::view::Base::pgn`], [`crate::pgn::game_with`] and the others
/// that take them read within those given, and the ones that take none within
/// [`Limits::default`]. The default bounds a game as a server must;
/// [`Limits::format_max`] reads all that each format allows, as a check that a
/// database decodes does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The largest move or annotation record read for a game, in bytes: a
    /// 2CBH record's content or spare area, a classic record whole, a PGN
    /// file's game text. A larger one is refused before it is read. The
    /// largest record of any kind in a Mega Database is about 1.2 MB, a
    /// guiding text; a record near the 2CBH reader's own 64 MiB limit would
    /// take gigabytes to render.
    pub game_bytes: usize,
    /// The longest entity record read for a name read on its own, as for a
    /// list of games, in bytes. Real names are a few dozen bytes; a longer
    /// record, which only a damaged or hostile file holds, reads as no name,
    /// and none is ever read whole. A game's PGN names its players and
    /// tournament as their records hold them, which the format bounds.
    pub name_bytes: usize,
}

impl Limits {
    /// [`Limits::default`], for a constant.
    pub const DEFAULT: Limits = Limits { game_bytes: 2 << 20, name_bytes: 4 << 10 };

    /// No bound but each format's own: a 2CBH record's content or spare area
    /// of up to 64 MiB, a classic move record of up to 16 MiB (its size has 24
    /// bits) and annotation record of up to
    /// [`crate::cbh::MAX_ANNOTATION_RECORD`], a PGN game of up to
    /// [`crate::pgnfile::MAX_TEXT`], and a name as long as its record.
    pub const fn format_max() -> Limits {
        Limits { game_bytes: usize::MAX, name_bytes: usize::MAX }
    }
}

impl Default for Limits {
    /// The bounds of a server: 2 MiB of move or annotation record per game,
    /// and 4 KiB per name.
    fn default() -> Self {
        Limits::DEFAULT
    }
}
