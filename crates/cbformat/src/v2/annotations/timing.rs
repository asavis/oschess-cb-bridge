//! Engine evaluations and times, decoded from an annotation's data: types
//! `26` (the main line's evaluations), `21` (one move's evaluation), `07`
//! (time spent on a move) and `24` (time control). `docs/format-notes.md`
//! gives the layouts and the evidence. Each decoder takes the data as the
//! format stores it and gives `None` for a layout it does not know; a classic
//! layout that no paired database confirms is not decoded.

/// An engine's score, from White's point of view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Score {
    Centipawns(i16),
    /// Moves to mate: positive when White mates, negative when Black does.
    Mate(i16),
}

/// One entry of type `26`: the evaluation of the main line's position after
/// ply `k` for the `k`th entry, the start position for the first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Evaluation {
    /// Centipawns, or for a mate the plies to it; White's point of view.
    pub value: i16,
    pub depth: u8,
    /// 0 centipawns, 1 mate, `ff` none; 2 and `20` also occur and are
    /// **unknown**.
    pub flag: u8,
}

impl Evaluation {
    /// The score, when the entry holds one: a mate in plies becomes moves.
    pub fn score(&self) -> Option<Score> {
        match (self.flag, self.value) {
            (0, v) => Some(Score::Centipawns(v)),
            (1, 0) => None,
            (1, v) => Some(Score::Mate(v.signum() * (v.unsigned_abs().div_ceil(2) as i16))),
            _ => None,
        }
    }

    /// The value ChessBase's own PGN writes in `[%evp]`: centipawns as they
    /// are, a mate in `n` plies as `30000 − n` (negated for Black), and 32767
    /// for no value.
    pub fn profile_value(&self) -> i32 {
        match (self.flag, i32::from(self.value)) {
            (0, v) => v,
            (1, v) if v > 0 => 30000 - v,
            (1, v) if v < 0 => -30000 - v,
            _ => 32767,
        }
    }
}

/// The entries of a type-`26` record: in 2CBH `01`, an `int` length, a
/// `short` count and 4-byte entries (value `short`, depth, flag); in the
/// classic format a big-endian `short` count and each entry as the same
/// 32-bit value big-endian.
pub fn evaluations(data: &[u8], classic: bool) -> Option<Vec<Evaluation>> {
    let (count, entries) = if classic {
        let count = u16::from_be_bytes([*data.first()?, *data.get(1)?]) as usize;
        (count, data.get(2..)?)
    } else {
        let len = i32::from_le_bytes(data.get(1..5)?.try_into().ok()?);
        let count = u16::from_le_bytes([*data.get(5)?, *data.get(6)?]) as usize;
        if data[0] != 1 || usize::try_from(len).ok()? != 2 + 4 * count {
            return None;
        }
        (count, data.get(7..)?)
    };
    if entries.len() != 4 * count {
        return None;
    }
    let entries = entries.as_chunks::<4>().0.iter().map(|e| {
        let e = if classic { [e[3], e[2], e[1], e[0]] } else { *e };
        Evaluation { value: i16::from_le_bytes([e[0], e[1]]), depth: e[2], flag: e[3] }
    });
    Some(entries.collect())
}

/// A type-`21` record, 2CBH only: three `short`s, the value, its kind (0
/// centipawns, 1 moves to mate; 3 and 32 are **unknown**) and a depth.
pub fn engine_evaluation(data: &[u8], classic: bool) -> Option<Score> {
    let d: &[u8; 6] = data.try_into().ok().filter(|_| !classic)?;
    let short = |i: usize| i16::from_le_bytes([d[i], d[i + 1]]);
    match (short(2), short(0)) {
        (0, v) => Some(Score::Centipawns(v)),
        (1, 0) => None,
        (1, v) => Some(Score::Mate(v)),
        _ => None,
    }
}

/// A type-`07` record, 2CBH only: a byte that is **unknown** (0 in 889,661
/// of 899,160 in the Mega), then seconds, minutes and hours. `(h, m, s)`.
pub fn time_spent(data: &[u8], classic: bool) -> Option<(u8, u8, u8)> {
    let d: &[u8; 4] = data.try_into().ok().filter(|_| !classic)?;
    (d[1] < 60 && d[2] < 60).then_some((d[3], d[2], d[1]))
}

/// One stage of a time control; times in hundredths of a second.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stage {
    pub initial: i32,
    pub increment: i32,
    /// The moves the stage lasts, 1000 for the rest of the game.
    pub moves: u16,
    /// 0 the rest of the game, 1 a stage of `moves` moves, 3 the rest of the
    /// game with an increment, 5 no time; 2 is **unknown**.
    pub kind: u8,
}

/// A type-`24` record, 2CBH only: `01`, three 11-byte stages (`int` initial
/// time, `int` increment, `short` moves, a kind byte) and an `int` 0.
pub fn time_control(data: &[u8], classic: bool) -> Option<[Stage; 3]> {
    let d: &[u8; 38] = data.try_into().ok().filter(|_| !classic)?;
    if d[0] != 1 || d[34..] != [0; 4] {
        return None;
    }
    let stage = |k: usize| {
        let s = &d[1 + 11 * k..12 + 11 * k];
        let int = |i: usize| i32::from_le_bytes([s[i], s[i + 1], s[i + 2], s[i + 3]]);
        Stage { initial: int(0), increment: int(4), moves: u16::from_le_bytes([s[8], s[9]]), kind: s[10] }
    };
    Some([stage(0), stage(1), stage(2)])
}
