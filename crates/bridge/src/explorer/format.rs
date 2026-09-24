//! The index file's layout (`docs/format-notes.md`, "Position index"): a
//! header, blocks of sorted keys each followed by the records they point to,
//! and a table of the blocks. Every part is covered by a CRC-32, so a torn or
//! damaged file is rebuilt instead of misread.

use chesscore::{Board, Move, Piece, Square};

pub const MAGIC: [u8; 8] = *b"OSCBIDX\0";
pub const VERSION: u32 = 1;
pub const HEADER_LEN: usize = 128;
/// Keys per block. A lookup reads one block: its keys and its records.
pub const BLOCK_KEYS: usize = 4096;
/// A key and the offset of its record in the block's data.
pub const KEY_ENTRY: usize = 12;
/// A block in the table: first key, offset, key count, data length, CRC.
pub const BLOCK_ENTRY: usize = 28;
/// The notable games kept per position.
pub const TOP_GAMES: usize = 12;
/// Positions reached in the first `MAX_PLY` plies are indexed, with the moves
/// played from them for plies below it.
pub const MAX_PLY: u8 = 40;
/// A position reached by one game only is dropped beyond this ply.
pub const PRUNE_PLY: u8 = 20;
/// `prune_ply` of an index that keeps every position (a delta).
pub const NO_PRUNING: u8 = u8::MAX;

/// What the index was built from and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// 0 for a full index, 1 for a delta over records appended since.
    pub kind: u8,
    pub max_ply: u8,
    pub prune_ply: u8,
    /// Records `first_record..=last_record` are indexed.
    pub first_record: u32,
    pub last_record: u32,
    /// The database's generation when the index was built.
    pub generation: u64,
    /// The digest of header records `1..=last_record` (see `digest`).
    pub digest: u64,
    pub games: u64,
    pub keys: u64,
    pub blocks: u32,
    pub table_offset: u64,
    pub table_crc: u32,
    pub file_len: u64,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..8].copy_from_slice(&MAGIC);
        b[8..12].copy_from_slice(&VERSION.to_le_bytes());
        b[12..16].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
        b[16] = self.kind;
        b[17] = self.max_ply;
        b[18] = self.prune_ply;
        b[20..24].copy_from_slice(&self.first_record.to_le_bytes());
        b[24..28].copy_from_slice(&self.last_record.to_le_bytes());
        b[32..40].copy_from_slice(&self.generation.to_le_bytes());
        b[40..48].copy_from_slice(&self.digest.to_le_bytes());
        b[48..56].copy_from_slice(&self.games.to_le_bytes());
        b[56..64].copy_from_slice(&self.keys.to_le_bytes());
        b[64..68].copy_from_slice(&self.blocks.to_le_bytes());
        b[72..80].copy_from_slice(&self.table_offset.to_le_bytes());
        b[80..84].copy_from_slice(&self.table_crc.to_le_bytes());
        b[88..96].copy_from_slice(&self.file_len.to_le_bytes());
        let crc = crc32(&b[..124]);
        b[124..128].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// The header, or `None` when the bytes are not a header of this version.
    pub fn decode(b: &[u8]) -> Option<Header> {
        if b.len() < HEADER_LEN || b[0..8] != MAGIC || u32_at(b, 8) != VERSION || u32_at(b, 12) as usize != HEADER_LEN {
            return None;
        }
        if crc32(&b[..124]) != u32_at(b, 124) {
            return None;
        }
        Some(Header {
            kind: b[16],
            max_ply: b[17],
            prune_ply: b[18],
            first_record: u32_at(b, 20),
            last_record: u32_at(b, 24),
            generation: u64_at(b, 32),
            digest: u64_at(b, 40),
            games: u64_at(b, 48),
            keys: u64_at(b, 56),
            blocks: u32_at(b, 64),
            table_offset: u64_at(b, 72),
            table_crc: u32_at(b, 80),
            file_len: u64_at(b, 88),
        })
    }
}

/// A block as the table describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    pub first_key: u64,
    pub offset: u64,
    pub keys: u32,
    pub data_len: u32,
    /// Over the block's keys and data together.
    pub crc: u32,
}

impl Block {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend(self.first_key.to_le_bytes());
        out.extend(self.offset.to_le_bytes());
        out.extend(self.keys.to_le_bytes());
        out.extend(self.data_len.to_le_bytes());
        out.extend(self.crc.to_le_bytes());
    }

    pub fn decode(b: &[u8]) -> Block {
        Block {
            first_key: u64_at(b, 0),
            offset: u64_at(b, 8),
            keys: u32_at(b, 16),
            data_len: u32_at(b, 20),
            crc: u32_at(b, 24),
        }
    }

    /// The bytes the block takes: its keys, then its data.
    pub fn bytes(&self) -> usize {
        self.keys as usize * KEY_ENTRY + self.data_len as usize
    }
}

/// How a game ended, as the index counts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    White = 0,
    Draw = 1,
    Black = 2,
    /// No result, a line, both lost, or unknown: counted in the games only.
    Other = 3,
}

impl Outcome {
    pub fn from_bits(b: u32) -> Outcome {
        match b & 3 {
            0 => Outcome::White,
            1 => Outcome::Draw,
            2 => Outcome::Black,
            _ => Outcome::Other,
        }
    }
}

/// Games through a position or a move: all of them, and by result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub games: u64,
    pub white: u64,
    pub draws: u64,
    pub black: u64,
}

impl Counts {
    pub fn add(&mut self, outcome: Outcome) {
        self.games += 1;
        match outcome {
            Outcome::White => self.white += 1,
            Outcome::Draw => self.draws += 1,
            Outcome::Black => self.black += 1,
            Outcome::Other => {}
        }
    }

    pub fn merge(&mut self, other: &Counts) {
        self.games += other.games;
        self.white += other.white;
        self.draws += other.draws;
        self.black += other.black;
    }
}

/// What the index holds for one position.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub counts: Counts,
    /// Each move played from the position, with its packed code (`pack_move`).
    pub moves: Vec<(u16, Counts)>,
    /// The notable games, best first.
    pub top: Vec<u32>,
}

impl Stats {
    /// Appends the record: counts, moves (most played first), notable games,
    /// each number as an unsigned LEB128 varint.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let counts = |out: &mut Vec<u8>, c: &Counts| {
            for v in [c.games, c.white, c.draws, c.black] {
                varint(out, v);
            }
        };
        counts(out, &self.counts);
        varint(out, self.moves.len() as u64);
        for (code, c) in &self.moves {
            out.extend(code.to_le_bytes());
            counts(out, c);
        }
        varint(out, self.top.len() as u64);
        for &g in &self.top {
            varint(out, u64::from(g));
        }
    }

    /// The record at the start of `b`; `None` when it runs past the end or
    /// holds more than it could.
    pub fn decode(b: &[u8]) -> Option<Stats> {
        let mut at = 0;
        let total = read_counts(b, &mut at)?;
        // No position has more legal moves than 218.
        let n = read_varint(b, &mut at)?;
        if n > 218 {
            return None;
        }
        let mut moves = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let code = u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            moves.push((code, read_counts(b, &mut at)?));
        }
        let t = read_varint(b, &mut at)?;
        if t > TOP_GAMES as u64 {
            return None;
        }
        let mut top = Vec::with_capacity(t as usize);
        for _ in 0..t {
            top.push(u32::try_from(read_varint(b, &mut at)?).ok()?);
        }
        Some(Stats { counts: total, moves, top })
    }
}

/// A move in 14 bits: from, to, and the promotion piece (queen, rook, bishop,
/// knight as 0-3), which counts only for a pawn reaching the last rank. 0 is
/// no move (`a1a1`). Castling is the king moving onto its rook, as in
/// `chesscore`.
pub fn pack_move(mv: Move) -> u16 {
    let promo = match mv.promotion {
        Some(Piece::Rook) => 1,
        Some(Piece::Bishop) => 2,
        Some(Piece::Knight) => 3,
        _ => 0,
    };
    mv.from.index() as u16 | (mv.to.index() as u16) << 6 | promo << 12
}

pub const NO_MOVE: u16 = 0;

/// The move `code` names in `board`, `None` for no move.
pub fn unpack_move(board: &Board, code: u16) -> Option<Move> {
    if code == NO_MOVE {
        return None;
    }
    let from = Square::from_index((code & 63) as u8)?;
    let to = Square::from_index((code >> 6 & 63) as u8)?;
    let pawn = matches!(board.piece_at(from), Some((Piece::Pawn, _)));
    let piece = match code >> 12 & 3 {
        1 => Piece::Rook,
        2 => Piece::Bishop,
        3 => Piece::Knight,
        _ => Piece::Queen,
    };
    let promotion = (pawn && (to.rank() == 0 || to.rank() == 7)).then_some(piece);
    Some(Move::new(from, to, promotion))
}

fn read_counts(b: &[u8], at: &mut usize) -> Option<Counts> {
    Some(Counts {
        games: read_varint(b, at)?,
        white: read_varint(b, at)?,
        draws: read_varint(b, at)?,
        black: read_varint(b, at)?,
    })
}

pub fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn read_varint(b: &[u8], at: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*at)?;
        *at += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

pub fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap_or([0; 4]))
}

pub fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap_or([0; 8]))
}

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC_TABLE: [u32; 256] = crc_table();

/// CRC-32 (IEEE 802.3), as zlib and PNG compute it.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in bytes {
        c = CRC_TABLE[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn records_and_headers_round_trip() {
        let s = Stats {
            counts: Counts { games: 300, white: 120, draws: 100, black: 70 },
            moves: vec![(pack_move("e2e4".parse().unwrap()), Counts { games: 200, white: 90, draws: 60, black: 45 })],
            top: vec![7, 1_000_000, 3],
        };
        let mut b = Vec::new();
        s.encode(&mut b);
        assert_eq!(Stats::decode(&b), Some(s));
        assert_eq!(Stats::decode(&b[..b.len() - 1]), None, "a cut record is refused");
        let h = Header {
            kind: 0,
            max_ply: MAX_PLY,
            prune_ply: PRUNE_PLY,
            first_record: 1,
            last_record: 99,
            generation: 5,
            digest: 6,
            games: 90,
            keys: 1000,
            blocks: 1,
            table_offset: 4096,
            table_crc: 7,
            file_len: 9000,
        };
        let e = h.encode();
        assert_eq!(Header::decode(&e), Some(h));
        let mut bad = e;
        bad[30] ^= 1;
        assert_eq!(Header::decode(&bad), None, "the header's CRC covers it");
    }

    #[test]
    fn moves_pack_with_promotions() {
        let b = Board::from_fen("7k/P7/8/8/8/8/8/K7 w - - 0 1").unwrap();
        for uci in ["a7a8q", "a7a8r", "a7a8b", "a7a8n"] {
            let mv: Move = uci.parse().unwrap();
            assert_eq!(unpack_move(&b, pack_move(mv)), Some(mv), "{uci}");
        }
        let king: Move = "a1b1".parse().unwrap();
        assert_eq!(unpack_move(&b, pack_move(king)), Some(king));
        assert_eq!(unpack_move(&b, NO_MOVE), None);
    }
}
