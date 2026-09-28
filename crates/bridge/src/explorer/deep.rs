//! The deep section of an index (#133): for each bucket of structures
//! ([`super::format::structure`]), the games whose main line holds a
//! structure of that bucket past [`super::format::PRUNE_PLY`]. The tree
//! answers the positions it holds; a position it does not hold is looked for
//! in the games of its structure's bucket, which are few, by replaying them.
//!
//! A build hands each worker's postings (`bucket << 32 | game`) to a
//! [`Sink`], which spreads them over partition files by the bucket's top
//! bits. [`write_section`] then sorts one partition at a time and writes its
//! buckets in order: per block of [`BLOCK_BUCKETS`] buckets, each bucket's
//! game count and its ascending games as varint deltas, the block covered by
//! a CRC-32 in the section's table.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::indexdir::crc32;
use crate::search::SearchError;

use super::format::{DEEP_BLOCK_BITS, DEEP_BLOCK_ENTRY, read_varint, varint};
use super::runs::{Progress, io, reserve};

/// Buckets per block.
pub const BLOCK_BUCKETS: usize = 1 << DEEP_BLOCK_BITS;
/// The most partitions a build spreads its postings over.
const MAX_PART_BITS: u8 = 8;
/// A worker's postings kept before they go to the partitions.
pub const WORKER_POSTINGS: usize = 1 << 17;
/// What a worker's postings take.
pub const WORKER_BYTES: usize = WORKER_POSTINGS * 8;

/// The postings of one build, spread over partition files by bucket.
pub struct Sink {
    bits: u8,
    part_bits: u8,
    parts: Vec<Mutex<Partition>>,
}

struct Partition {
    path: PathBuf,
    out: BufWriter<File>,
    postings: u64,
}

impl Sink {
    /// Partition files in `dir` for buckets of `bits` bits.
    pub fn create(dir: &Path, bits: u8) -> Result<Sink, SearchError> {
        let part_bits = MAX_PART_BITS.min(bits.saturating_sub(DEEP_BLOCK_BITS));
        let mut parts = Vec::new();
        for p in 0..1usize << part_bits {
            let path = dir.join(format!("deep-{p}"));
            let out = BufWriter::with_capacity(16 << 10, File::create(&path).map_err(|e| io(&path, e))?);
            parts.push(Mutex::new(Partition { path, out, postings: 0 }));
        }
        Ok(Sink { bits, part_bits, parts })
    }

    pub fn bits(&self) -> u8 {
        self.bits
    }

    /// Takes a worker's postings, emptying `postings`.
    pub fn add(&self, postings: &mut Vec<u64>) -> Result<(), SearchError> {
        postings.sort_unstable();
        let shift = 32 + u32::from(self.bits - self.part_bits);
        let mut rest = &postings[..];
        while let Some(&first) = rest.first() {
            let part = (first >> shift) as usize;
            let n = rest.partition_point(|&p| (p >> shift) as usize == part);
            let mut target = self.parts[part].lock().unwrap_or_else(|e| e.into_inner());
            let Partition { path, out, postings: count } = &mut *target;
            for p in &rest[..n] {
                out.write_all(&p.to_le_bytes()).map_err(|e| io(path, e))?;
            }
            *count += n as u64;
            rest = &rest[n..];
        }
        postings.clear();
        Ok(())
    }

    /// Closes the partition files: each one's path and postings, in bucket order.
    pub fn finish(self) -> Result<Vec<(PathBuf, u64)>, SearchError> {
        let mut done = Vec::new();
        for part in self.parts {
            let Partition { path, out, postings } = part.into_inner().unwrap_or_else(|e| e.into_inner());
            out.into_inner().map_err(|e| io(&path, e.into_error()))?;
            done.push((path, postings));
        }
        Ok(done)
    }
}

/// Writes the section's blocks to `out`, which is at `offset` in the index
/// file, from `parts` in bucket order, each removed once written. Returns the
/// table of blocks and the postings kept, a game once per bucket.
pub fn write_section(
    parts: &[(PathBuf, u64)],
    bits: u8,
    out: &mut impl Write,
    mut offset: u64,
    target: &Path,
    progress: &Progress,
) -> Result<(Vec<u8>, u64), SearchError> {
    let blocks = 1usize << (bits - DEEP_BLOCK_BITS);
    let blocks_per_part = blocks / parts.len().max(1);
    let mut table = Vec::with_capacity(blocks * DEEP_BLOCK_ENTRY);
    let mut kept = 0u64;
    let mut block = Vec::new();
    for (p, (path, postings)) in parts.iter().enumerate() {
        let bytes = usize::try_from(postings.saturating_mul(8)).map_err(|_| SearchError::TooLarge)?;
        let _memory = reserve(bytes, progress)?;
        let mut all = read_postings(path, *postings)?;
        let _ = std::fs::remove_file(path);
        all.sort_unstable();
        all.dedup();
        kept += all.len() as u64;
        let mut rest = &all[..];
        for b in 0..blocks_per_part {
            let first_bucket = ((p * blocks_per_part + b) * BLOCK_BUCKETS) as u64;
            block.clear();
            for bucket in first_bucket..first_bucket + BLOCK_BUCKETS as u64 {
                let n = rest.partition_point(|&x| x >> 32 == bucket);
                varint(&mut block, n as u64);
                let mut last = 0u64;
                for &x in &rest[..n] {
                    let game = x & 0xffff_ffff;
                    varint(&mut block, game - last);
                    last = game;
                }
                rest = &rest[n..];
            }
            out.write_all(&block).map_err(|e| io(target, e))?;
            table.extend(offset.to_le_bytes());
            table.extend((block.len() as u32).to_le_bytes());
            table.extend(crc32(&block).to_le_bytes());
            offset += block.len() as u64;
        }
        if !rest.is_empty() {
            return Err(io(path, std::io::Error::other("a posting lies outside its partition")));
        }
    }
    Ok((table, kept))
}

fn read_postings(path: &Path, postings: u64) -> Result<Vec<u64>, SearchError> {
    let mut file = File::open(path).map_err(|e| io(path, e))?;
    let len = file.metadata().map_err(|e| io(path, e))?.len();
    if len != postings * 8 {
        return Err(io(path, std::io::Error::other("a deep partition has the wrong length")));
    }
    let mut all = Vec::new();
    all.try_reserve_exact(postings as usize).map_err(|_| SearchError::TooLarge)?;
    let mut buf = vec![0u8; 64 << 10];
    let mut left = len as usize;
    while left > 0 {
        let n = left.min(buf.len());
        file.read_exact(&mut buf[..n]).map_err(|e| io(path, e))?;
        all.extend(buf[..n].as_chunks::<8>().0.iter().map(|c| u64::from_le_bytes(*c)));
        left -= n;
    }
    Ok(all)
}

/// The games of bucket `local` in a block's bytes, at most `max_game` each;
/// `None` when the block does not hold them as written.
pub fn bucket_games(block: &[u8], local: usize, max_game: u32) -> Option<Vec<u32>> {
    let mut at = 0;
    for _ in 0..local {
        let n = read_varint(block, &mut at)?;
        for _ in 0..n {
            read_varint(block, &mut at)?;
        }
    }
    let n = read_varint(block, &mut at)?;
    // Each game takes a byte at least, so a damaged count never reserves
    // more than the block's size.
    if n > u64::from(max_game) || n > (block.len() - at) as u64 {
        return None;
    }
    let mut games = Vec::new();
    games.try_reserve_exact(n as usize).ok()?;
    let mut game = 0u64;
    for _ in 0..n {
        let delta = read_varint(block, &mut at)?;
        game = game.checked_add(delta)?;
        if delta == 0 || game > u64::from(max_game) {
            return None;
        }
        games.push(game as u32);
    }
    Some(games)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postings_come_back_per_bucket_in_game_order() {
        let dir = std::env::temp_dir().join(format!("bridge-deep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bits = 14;
        let sink = Sink::create(&dir, bits).unwrap();
        let posting = |bucket: u64, game: u64| bucket << 32 | game;
        let last = (1u64 << bits) - 1;
        // Two workers, out of order, with a game twice in one bucket.
        sink.add(&mut vec![posting(5, 9), posting(last, 2), posting(5, 3)]).unwrap();
        sink.add(&mut vec![posting(5, 9), posting(4096, 7), posting(0, 1)]).unwrap();
        let parts = sink.finish().unwrap();
        let mut out = Vec::new();
        let (table, kept) = write_section(&parts, bits, &mut out, 1000, &dir.join("x"), &Progress::default()).unwrap();
        assert_eq!(kept, 5, "a game counts once per bucket");
        assert_eq!(table.len(), 4 * DEEP_BLOCK_ENTRY);
        let block = |i: usize| {
            let e = &table[i * DEEP_BLOCK_ENTRY..];
            let off = u64::from_le_bytes(e[0..8].try_into().unwrap()) as usize - 1000;
            let len = u32::from_le_bytes(e[8..12].try_into().unwrap()) as usize;
            assert_eq!(crc32(&out[off..off + len]), u32::from_le_bytes(e[12..16].try_into().unwrap()));
            out[off..off + len].to_vec()
        };
        assert_eq!(bucket_games(&block(0), 5, 100), Some(vec![3, 9]));
        assert_eq!(bucket_games(&block(0), 0, 100), Some(vec![1]));
        assert_eq!(bucket_games(&block(0), 6, 100), Some(vec![]));
        assert_eq!(bucket_games(&block(1), 0, 100), Some(vec![7]));
        assert_eq!(bucket_games(&block(3), BLOCK_BUCKETS - 1, 100), Some(vec![2]));
        // A game past the database's last record is damage.
        assert_eq!(bucket_games(&block(0), 5, 8), None);
        // So is a delta that wraps around to a game already listed.
        let mut wrap = Vec::new();
        for v in [2, 5, u64::MAX - 2] {
            varint(&mut wrap, v);
        }
        assert_eq!(bucket_games(&wrap, 0, 100), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
