//! The index file's layout (`docs/format-notes.md`, "Position index"): a
//! header, blocks of sorted keys each followed by the records they point to,
//! and a table of the blocks; then the deep section (#133): blocks of
//! structure buckets, each with the games that hold such a structure past
//! [`MAX_PLY`], and their table. Every part is covered by a CRC-32, so a
//! torn or damaged file is rebuilt instead of misread.

use chesscore::{Board, Color, Move, Piece, Square};

use crate::indexdir::{crc32, u32_at, u64_at};

pub const MAGIC: [u8; 8] = *b"OSCBIDX\0";
/// 2 added the deep section (#133); 3 the build id, which the move stream
/// built with the index carries too (#145); 4 deep blocks of 256 buckets, and
/// with each game its structure's print and whether it holds that structure
/// beyond the tree's plies (#146); 5 the tree to ply 20 in full, its blocks
/// ending where the build's parts of the keys end (#147).
pub const VERSION: u32 = 5;
pub const HEADER_LEN: usize = 128;
/// Keys per block. A lookup reads one block: its keys and its records.
pub const BLOCK_KEYS: usize = 4096;
/// A block ends once its records reach this size, so a lookup reads little
/// whatever a position holds.
pub const BLOCK_DATA: usize = 1 << 20;
/// The most a block's records can take: [`BLOCK_DATA`], plus the record that
/// passed it, which is at most 218 moves and 12 games of varints.
pub const MAX_BLOCK_DATA: usize = BLOCK_DATA + (16 << 10);
/// The fewest bytes a record takes: four counts, no moves, no games.
pub const MIN_RECORD: usize = 6;
/// A key and the offset of its record in the block's data.
pub const KEY_ENTRY: usize = 12;
/// A block in the table: first key, offset, key count, data length, CRC.
pub const BLOCK_ENTRY: usize = 28;
/// The notable games kept per position.
pub const TOP_GAMES: usize = 12;
/// Positions reached in the first `MAX_PLY` plies are indexed, every one of
/// them, with the moves played from them. Every position past it is found
/// through the deep section.
pub const MAX_PLY: u8 = 20;
/// The ply beyond which a position reached by one game only would be
/// dropped: the tree's depth, so that none is (#147).
pub const PRUNE_PLY: u8 = MAX_PLY;
/// Buckets per deep block: a lookup reads one block, a few KiB, and walks to
/// its bucket.
pub const DEEP_BLOCK_BITS: u8 = 8;
/// The fewest and the most bucket bits a deep section has.
pub const MIN_DEEP_BITS: u8 = DEEP_BLOCK_BITS;
pub const MAX_DEEP_BITS: u8 = 24;
/// A deep block in its table: offset, length, CRC.
pub const DEEP_BLOCK_ENTRY: usize = 16;
/// What the index was built from and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub max_ply: u8,
    pub prune_ply: u8,
    /// Records `first_record..=last_record` are indexed.
    pub first_record: u32,
    pub last_record: u32,
    /// The database's generation when the index was built.
    pub generation: u64,
    pub games: u64,
    pub keys: u64,
    pub blocks: u32,
    pub table_offset: u64,
    pub table_crc: u32,
    pub file_len: u64,
    /// The deep section: its buckets are the top `deep_bits` bits of a
    /// [`structure`]; it starts right after the tree's table.
    pub deep_bits: u8,
    pub deep_postings: u64,
    pub deep_offset: u64,
    pub deep_table_offset: u64,
    pub deep_table_crc: u32,
    /// The build's id: the index answers only with the move stream of the
    /// same build ([`super::stream`]).
    pub build_id: u64,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..8].copy_from_slice(&MAGIC);
        b[8..12].copy_from_slice(&VERSION.to_le_bytes());
        b[12..16].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
        b[17] = self.max_ply;
        b[18] = self.prune_ply;
        b[19] = self.deep_bits;
        b[20..24].copy_from_slice(&self.first_record.to_le_bytes());
        b[24..28].copy_from_slice(&self.last_record.to_le_bytes());
        b[32..40].copy_from_slice(&self.generation.to_le_bytes());
        b[40..48].copy_from_slice(&self.deep_postings.to_le_bytes());
        b[48..56].copy_from_slice(&self.games.to_le_bytes());
        b[56..64].copy_from_slice(&self.keys.to_le_bytes());
        b[64..68].copy_from_slice(&self.blocks.to_le_bytes());
        b[72..80].copy_from_slice(&self.table_offset.to_le_bytes());
        b[80..84].copy_from_slice(&self.table_crc.to_le_bytes());
        b[88..96].copy_from_slice(&self.file_len.to_le_bytes());
        b[96..104].copy_from_slice(&self.deep_offset.to_le_bytes());
        b[104..112].copy_from_slice(&self.deep_table_offset.to_le_bytes());
        b[112..116].copy_from_slice(&self.deep_table_crc.to_le_bytes());
        b[116..124].copy_from_slice(&self.build_id.to_le_bytes());
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
            max_ply: b[17],
            prune_ply: b[18],
            first_record: u32_at(b, 20),
            last_record: u32_at(b, 24),
            generation: u64_at(b, 32),
            games: u64_at(b, 48),
            keys: u64_at(b, 56),
            blocks: u32_at(b, 64),
            table_offset: u64_at(b, 72),
            table_crc: u32_at(b, 80),
            file_len: u64_at(b, 88),
            deep_bits: b[19],
            deep_postings: u64_at(b, 40),
            deep_offset: u64_at(b, 96),
            deep_table_offset: u64_at(b, 104),
            deep_table_crc: u32_at(b, 112),
            build_id: u64_at(b, 116),
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
        self.add_games(outcome, 1);
    }

    /// Adds `games` games that ended with `outcome`.
    pub fn add_games(&mut self, outcome: Outcome, games: u64) {
        self.games += games;
        match outcome {
            Outcome::White => self.white += games,
            Outcome::Draw => self.draws += games,
            Outcome::Black => self.black += games,
            Outcome::Other => {}
        }
    }

    pub fn merge(&mut self, other: &Counts) {
        self.games += other.games;
        self.white += other.white;
        self.draws += other.draws;
        self.black += other.black;
    }

    /// These and `other` added, `None` when a sum passes 64 bits.
    pub fn checked_merge(&self, other: &Counts) -> Option<Counts> {
        Some(Counts {
            games: self.games.checked_add(other.games)?,
            white: self.white.checked_add(other.white)?,
            draws: self.draws.checked_add(other.draws)?,
            black: self.black.checked_add(other.black)?,
        })
    }

    /// Whether these are counts of at most `games` games, with their results
    /// adding up within their own games, as a sound index's are.
    pub fn within(&self, games: u64) -> bool {
        let results = self.white.checked_add(self.draws).and_then(|r| r.checked_add(self.black));
        self.games <= games && results.is_some_and(|r| r <= self.games)
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
        encode_record(out, &self.counts, &self.moves, self.top.iter().copied());
    }

    /// The record at the start of `b`, in an index of `games` games; `None`
    /// when it runs past the end or holds more than it could: more games
    /// than the index, results beyond their games, or moves played by more
    /// games than reach the position, each of which plays one move from it
    /// at most.
    pub fn decode(b: &[u8], games: u64) -> Option<Stats> {
        let mut at = 0;
        let total = read_counts(b, &mut at).filter(|c| c.within(games))?;
        // No position has more legal moves than 218.
        let n = read_varint(b, &mut at)?;
        if n > 218 {
            return None;
        }
        let mut moves = Vec::with_capacity(n as usize);
        let mut played = 0u64;
        for _ in 0..n {
            let code = u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?);
            at += 2;
            let counts = read_counts(b, &mut at).filter(|c| c.within(total.games))?;
            played = played.checked_add(counts.games).filter(|&p| p <= total.games)?;
            moves.push((code, counts));
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

/// Appends the record of a position with `counts`, `moves` in their order
/// and the notable games `top`, as [`Stats::encode`] does.
pub fn encode_record(
    out: &mut Vec<u8>,
    counts: &Counts,
    moves: &[(u16, Counts)],
    top: impl ExactSizeIterator<Item = u32>,
) {
    let put = |out: &mut Vec<u8>, c: &Counts| {
        for v in [c.games, c.white, c.draws, c.black] {
            varint(out, v);
        }
    };
    put(out, counts);
    varint(out, moves.len() as u64);
    for (code, c) in moves {
        out.extend(code.to_le_bytes());
        put(out, c);
    }
    varint(out, top.len() as u64);
    for g in top {
        varint(out, u64::from(g));
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
        // The tenth byte holds the top bit alone: anything more is damage,
        // never a number cut to 64 bits.
        if shift == 63 && byte > 1 {
            return None;
        }
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

impl Header {
    /// The deep section's blocks.
    pub fn deep_blocks(&self) -> u64 {
        1u64 << self.deep_bits.saturating_sub(DEEP_BLOCK_BITS)
    }
}

/// The bucket bits for a database of `records` records: about one bucket for
/// every game, within [`MIN_DEEP_BITS`] and [`MAX_DEEP_BITS`], so that a
/// small database's section stays small and a large one's buckets stay few
/// games each.
pub fn deep_bits(records: u32) -> u8 {
    let log = 32 - records.max(1).leading_zeros();
    (log as u8).clamp(MIN_DEEP_BITS, MAX_DEEP_BITS)
}

/// A hash of `board`'s structure: what only a pawn move or a capture
/// changes, each side's pawns and its pieces by kind. A pawn never moves back
/// and a man taken never comes back, so a game passes through each structure
/// once, in one stretch of plies, and a position is found among the few games
/// that hold its structure (#133). The pieces split what the pawns alone
/// share widely: every pawnless ending has one pawn structure.
pub fn structure(board: &Board) -> u64 {
    let mut pieces = 0u64;
    for (i, piece) in STRUCTURE_PIECES.into_iter().enumerate() {
        for color in [Color::White, Color::Black] {
            pieces += u64::from(board.colored(piece, color).count_ones()) << piece_shift(i, color);
        }
    }
    let pawns = |color| board.colored(Piece::Pawn, color);
    structure_of(pawns(Color::White), pawns(Color::Black), pieces)
}

/// The pieces a structure counts, by kind, in the order of their counts.
pub const STRUCTURE_PIECES: [Piece; 4] = [Piece::Knight, Piece::Bishop, Piece::Rook, Piece::Queen];

/// Where the count of `color`'s pieces of kind `i` of [`STRUCTURE_PIECES`]
/// lies among a structure's counts: four bits each, which hold at most ten
/// of a kind, two and eight promoted pawns, and the fifteen of a set-up
/// position.
pub fn piece_shift(i: usize, color: Color) -> u32 {
    8 * i as u32 + if color == Color::White { 0 } else { 4 }
}

/// The structure of white pawns `white`, black pawns `black` and the pieces
/// counted `pieces` ([`piece_shift`]), as [`structure`] gives a board's: a
/// build follows the three through a line's words without a board.
pub fn structure_of(white: u64, black: u64, pieces: u64) -> u64 {
    mix(white ^ mix(black ^ mix(pieces ^ STRUCTURE_SEED)))
}

/// What [`structure_of`] starts from.
const STRUCTURE_SEED: u64 = 0x9e37_79b9_7f4a_7c15;

/// Whether this processor hashes structures in vectors
/// ([`structures_of`]).
pub fn structures_in_vectors() -> bool {
    #[cfg(target_arch = "x86_64")]
    return wide::has_avx512() || wide::has_avx2();
    #[cfg(not(target_arch = "x86_64"))]
    false
}

/// The [`structure_of`] of each of the first `n` of `white`, `black` and
/// `pieces` taken together, into `out`, in vectors: eight at a time on a
/// processor with AVX-512 (`avx512f` and `avx512dq`), four at a time on one
/// with AVX2. Each is three rounds of [`mix`], whose multiplications a
/// processor makes one after another, and a build hashes a structure in each
/// of its deep passes: in vectors, several at once. Whether it hashed them:
/// on a processor with neither it hashes nothing, since hashing each where
/// it is needed is quicker there than storing them. `N` is a multiple of
/// eight, and the lanes past `n` of the last vector are hashed too, from
/// whatever the arrays hold there.
pub fn structures_of<const N: usize>(
    white: &[u64; N],
    black: &[u64; N],
    pieces: &[u64; N],
    n: usize,
    out: &mut [u64; N],
) -> bool {
    const { assert!(N.is_multiple_of(8)) };
    let n = n.min(N);
    #[cfg(target_arch = "x86_64")]
    {
        if wide::has_avx512() {
            // SAFETY: the processor has the instructions it is compiled for.
            unsafe { wide::structures8(white, black, pieces, n, out) };
            return true;
        }
        if wide::has_avx2() {
            // SAFETY: as above.
            unsafe { wide::structures4(white, black, pieces, n, out) };
            return true;
        }
    }
    let _ = (white, black, pieces, n, out);
    false
}

/// [`structures_of`] in vectors: [`mix`] on four lanes, whose 64-bit
/// multiplications AVX2 makes of three 32-bit ones each, or on eight, which
/// AVX-512 multiplies whole.
#[cfg(target_arch = "x86_64")]
mod wide {
    use std::arch::x86_64::{
        __m256i, __m512i, _mm256_add_epi64, _mm256_loadu_si256, _mm256_mul_epu32, _mm256_set1_epi64x,
        _mm256_slli_epi64, _mm256_srli_epi64, _mm256_storeu_si256, _mm256_xor_si256, _mm512_loadu_si512,
        _mm512_mullo_epi64, _mm512_set1_epi64, _mm512_srli_epi64, _mm512_storeu_si512, _mm512_xor_si512,
    };

    use super::{MIX_1, MIX_2, STRUCTURE_SEED};

    pub fn has_avx512() -> bool {
        std::arch::is_x86_feature_detected!("avx512f") && std::arch::is_x86_feature_detected!("avx512dq")
    }

    pub fn has_avx2() -> bool {
        std::arch::is_x86_feature_detected!("avx2")
    }

    #[target_feature(enable = "avx512f,avx512dq")]
    pub fn structures8<const N: usize>(
        white: &[u64; N],
        black: &[u64; N],
        pieces: &[u64; N],
        n: usize,
        out: &mut [u64; N],
    ) {
        let seed = _mm512_set1_epi64(STRUCTURE_SEED as i64);
        let (w, b, p) = (white.as_chunks::<8>().0, black.as_chunks::<8>().0, pieces.as_chunks::<8>().0);
        for (i, o) in out.as_chunks_mut::<8>().0.iter_mut().enumerate().take(n.div_ceil(8)) {
            // SAFETY: each chunk holds the eight lanes loaded or stored.
            let load = |c: &[u64; 8]| unsafe { _mm512_loadu_si512(c.as_ptr().cast()) };
            let s = mix8(_mm512_xor_si512(
                load(&w[i]),
                mix8(_mm512_xor_si512(load(&b[i]), mix8(_mm512_xor_si512(load(&p[i]), seed)))),
            ));
            // SAFETY: as above.
            unsafe { _mm512_storeu_si512(o.as_mut_ptr().cast(), s) };
        }
    }

    #[inline]
    #[target_feature(enable = "avx512f,avx512dq")]
    fn mix8(z: __m512i) -> __m512i {
        let z = _mm512_mullo_epi64(_mm512_xor_si512(z, _mm512_srli_epi64::<30>(z)), _mm512_set1_epi64(MIX_1 as i64));
        let z = _mm512_mullo_epi64(_mm512_xor_si512(z, _mm512_srli_epi64::<27>(z)), _mm512_set1_epi64(MIX_2 as i64));
        _mm512_xor_si512(z, _mm512_srli_epi64::<31>(z))
    }

    #[target_feature(enable = "avx2")]
    pub fn structures4<const N: usize>(
        white: &[u64; N],
        black: &[u64; N],
        pieces: &[u64; N],
        n: usize,
        out: &mut [u64; N],
    ) {
        let seed = _mm256_set1_epi64x(STRUCTURE_SEED as i64);
        let (w, b, p) = (white.as_chunks::<4>().0, black.as_chunks::<4>().0, pieces.as_chunks::<4>().0);
        for (i, o) in out.as_chunks_mut::<4>().0.iter_mut().enumerate().take(n.div_ceil(4)) {
            // SAFETY: each chunk holds the four lanes loaded or stored.
            let load = |c: &[u64; 4]| unsafe { _mm256_loadu_si256(c.as_ptr().cast()) };
            let s = mix4(_mm256_xor_si256(
                load(&w[i]),
                mix4(_mm256_xor_si256(load(&b[i]), mix4(_mm256_xor_si256(load(&p[i]), seed)))),
            ));
            // SAFETY: as above.
            unsafe { _mm256_storeu_si256(o.as_mut_ptr().cast(), s) };
        }
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    fn mix4(z: __m256i) -> __m256i {
        let z = times4(_mm256_xor_si256(z, _mm256_srli_epi64::<30>(z)), MIX_1);
        let z = times4(_mm256_xor_si256(z, _mm256_srli_epi64::<27>(z)), MIX_2);
        _mm256_xor_si256(z, _mm256_srli_epi64::<31>(z))
    }

    /// `a` times `k`, lane by lane, modulo 2^64: the low halves' product,
    /// plus each low half times the other's high half, shifted up.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn times4(a: __m256i, k: u64) -> __m256i {
        let (low, high) = (_mm256_set1_epi64x((k & 0xffff_ffff) as i64), _mm256_set1_epi64x((k >> 32) as i64));
        let cross = _mm256_add_epi64(_mm256_mul_epu32(_mm256_srli_epi64::<32>(a), low), _mm256_mul_epu32(a, high));
        _mm256_add_epi64(_mm256_mul_epu32(a, low), _mm256_slli_epi64::<32>(cross))
    }
}

/// The bits of a key that name its part for a database of `records`
/// records: about 256 records a part, from 16 to 65,536 parts. A build
/// writes the tree part by part, on its workers at once, and a block of the
/// tree ends where a part ends (#147).
pub fn part_bits(records: u32) -> u8 {
    deep_bits(records).saturating_sub(DEEP_BLOCK_BITS).clamp(4, 16)
}

/// The part of the keys, of `bits` bits, that `key` lies in.
pub fn part_of(key: u64, bits: u8) -> usize {
    (key >> (64 - u32::from(bits))) as usize
}

/// The bucket of `structure` among `1 << bits`.
pub fn deep_bucket(structure: u64, bits: u8) -> u32 {
    (structure >> (64 - u32::from(bits))) as u32
}

/// The bits of a structure below its bucket's that the deep section keeps
/// with each game: the games of another structure of the bucket are then
/// left out without a replay, all but one in 128.
pub const PRINT_BITS: u8 = 7;

/// The print of `structure` in a bucket of `bits` bits: its [`PRINT_BITS`]
/// bits below the bucket's.
pub fn deep_print(structure: u64, bits: u8) -> u8 {
    (structure >> (64 - u32::from(bits) - u32::from(PRINT_BITS))) as u8 & ((1 << PRINT_BITS) - 1)
}

/// The splitmix64 finaliser: every input bit reaches every output bit.
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(MIX_1);
    z = (z ^ (z >> 27)).wrapping_mul(MIX_2);
    z ^ (z >> 31)
}

/// [`mix`]'s multipliers.
const MIX_1: u64 = 0xbf58_476d_1ce4_e5b9;
const MIX_2: u64 = 0x94d0_49bb_1331_11eb;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_headers_round_trip() {
        let s = Stats {
            counts: Counts { games: 300, white: 120, draws: 100, black: 70 },
            moves: vec![(pack_move("e2e4".parse().unwrap()), Counts { games: 200, white: 90, draws: 60, black: 45 })],
            top: vec![7, 1_000_000, 3],
        };
        let mut b = Vec::new();
        s.encode(&mut b);
        assert_eq!(Stats::decode(&b, 300), Some(s.clone()));
        assert_eq!(Stats::decode(&b[..b.len() - 1], 300), None, "a cut record is refused");
        // Counts no sound index holds: more games than the index's, results
        // beyond their games, a move's too, or moves of more games than the
        // position's, each a sum within 64 bits or not.
        assert_eq!(Stats::decode(&b, 299), None, "more games than the index holds");
        let max = Counts { games: u64::MAX, white: u64::MAX, draws: 0, black: 0 };
        let e4 = s.moves[0].0;
        for (counts, moves) in [
            (max, s.moves.clone()),
            (Counts { games: 300, white: 120, draws: 100, black: 81 }, s.moves.clone()),
            (s.counts, vec![(e4, max)]),
            (s.counts, vec![(e4, Counts { games: 301, white: 0, draws: 0, black: 0 })]),
            (s.counts, vec![(e4, Counts { games: 200, white: 90, draws: 60, black: 51 })]),
            (
                s.counts,
                vec![(e4, Counts { games: 200, ..Counts::default() }), (1, Counts { games: 101, ..Counts::default() })],
            ),
        ] {
            let mut b = Vec::new();
            Stats { counts, moves: moves.clone(), top: vec![] }.encode(&mut b);
            assert_eq!(Stats::decode(&b, 300), None, "{counts:?} {moves:?}");
        }
        let h = Header {
            max_ply: MAX_PLY,
            prune_ply: PRUNE_PLY,
            first_record: 1,
            last_record: 99,
            generation: 5,
            games: 90,
            keys: 1000,
            blocks: 1,
            table_offset: 4096,
            table_crc: 7,
            file_len: 9000,
            deep_bits: 20,
            deep_postings: 123_456,
            deep_offset: 5000,
            deep_table_offset: 8000,
            deep_table_crc: 11,
            build_id: 0x0123_4567_89ab_cdef,
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

    #[test]
    fn varints_hold_64_bits_and_no_more() {
        for v in [0, 1, 127, 128, 1 << 63, u64::MAX] {
            let mut b = Vec::new();
            varint(&mut b, v);
            let mut at = 0;
            assert_eq!((read_varint(&b, &mut at), at), (Some(v), b.len()), "{v}");
        }
        // A tenth byte of more than the top bit, or one that goes on.
        let mut at = 0;
        assert_eq!(read_varint(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02], &mut at), None);
        let mut at = 0;
        assert_eq!(read_varint(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x81, 0x00], &mut at), None);
    }

    #[test]
    fn a_structure_is_the_pawns_and_the_pieces_by_kind() {
        let s = |fen: &str| structure(&Board::from_fen(fen).unwrap());
        // Pieces elsewhere, the king too, or the other side to move: one structure.
        let rooks = s("4k3/p7/8/8/8/8/P7/R3K3 w - - 0 1");
        assert_eq!(s("3k4/p7/8/8/8/8/P7/4K2R b - - 0 1"), rooks);
        // A piece of another kind, or of the other side.
        assert_ne!(s("4k3/p7/8/8/8/8/P7/Q3K3 w - - 0 1"), rooks);
        assert_ne!(s("r3k3/p7/8/8/8/8/P7/4K3 w - - 0 1"), rooks);
        // A pawn one square on, or of the other side.
        assert_ne!(s("4k3/p7/8/8/8/P7/8/R3K3 w - - 0 1"), rooks);
        assert_ne!(s("4k3/P7/8/8/8/8/p7/R3K3 w - - 0 1"), rooks);
        // Pawnless endings split by what is left.
        assert_ne!(s("4k3/8/8/8/8/8/8/R3K3 w - - 0 1"), s("4k3/8/8/8/8/8/8/Q3K3 w - - 0 1"));
        assert_ne!(s("4k3/8/8/8/8/8/8/4K3 w - - 0 1"), s("4k3/8/8/8/8/8/8/R3K3 w - - 0 1"));
    }

    /// Structures hashed several at a time are those hashed one at a time,
    /// however many there are, on each path this processor can take.
    #[test]
    fn structures_hashed_together_are_those_hashed_alone() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut parts = [[0u64; 64]; 3];
        for v in parts.iter_mut().flatten() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *v = x;
        }
        let [white, black, pieces] = &parts;
        let alone: Vec<u64> = (0..64).map(|i| structure_of(white[i], black[i], pieces[i])).collect();
        type Path = fn(&[u64; 64], &[u64; 64], &[u64; 64], usize, &mut [u64; 64]);
        let mut paths: Vec<(&str, Path)> = Vec::new();
        #[cfg(target_arch = "x86_64")]
        {
            if wide::has_avx2() {
                // SAFETY: the processor has the instructions it is compiled for.
                paths.push(("four at a time", |w, b, p, n, o| unsafe { wide::structures4(w, b, p, n, o) }));
            }
            if wide::has_avx512() {
                // SAFETY: as above.
                paths.push(("eight at a time", |w, b, p, n, o| unsafe { wide::structures8(w, b, p, n, o) }));
            }
        }
        for (name, path) in paths {
            for n in 0..=64 {
                let mut out = [0; 64];
                path(white, black, pieces, n, &mut out);
                assert_eq!(out[..n], alone[..n], "{name}, {n}");
            }
        }
        let mut out = [0; 64];
        if structures_of(white, black, pieces, 64, &mut out) {
            assert_eq!(out[..], alone[..], "whichever this processor takes");
        }
        // The standard start's structure, from its parts.
        let start = Board::startpos();
        assert_eq!(structure(&start), structure_of(0xff00, 0xff << 48, 0x1122_2222));
    }
}
