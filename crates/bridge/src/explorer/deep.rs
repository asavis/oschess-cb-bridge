//! The deep section of an index (#133): for each bucket of structures
//! ([`super::format::structure`]), the games whose main line holds a
//! structure of that bucket past [`super::format::PRUNE_PLY`]. The tree
//! answers the positions it holds; a position it does not hold is looked for
//! in the games of its structure's bucket, which are few, by replaying them.
//!
//! A build hands each worker's postings (`bucket << 32 | game`) to a
//! [`Sink`], which spreads them over partition files by the bucket's top
//! bits. [`write_section`] then sorts one partition at a time, in memory when
//! it fits the build's share and in chunks on disk when not, and writes its
//! buckets in order: per block of [`BLOCK_BUCKETS`] buckets, each bucket's
//! game count and its ascending games as varint deltas, the block covered by
//! a CRC-32 in the section's table.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::indexdir::crc32_update;
use crate::search::SearchError;
use crate::search::memory::Hold;

use super::format::{DEEP_BLOCK_BITS, DEEP_BLOCK_ENTRY, MAX_DEEP_BITS, read_varint, varint};
use super::runs::{Progress, io, reserve};

/// Buckets per block.
pub const BLOCK_BUCKETS: usize = 1 << DEEP_BLOCK_BITS;
/// The most partitions a build spreads its postings over.
const MAX_PART_BITS: u8 = 8;
/// A worker's postings kept before they go to the partitions.
pub const WORKER_POSTINGS: usize = 1 << 17;
/// What a worker's postings take.
pub const WORKER_BYTES: usize = WORKER_POSTINGS * 8;

/// What each partition file's writer buffers.
const PART_BUFFER: usize = 4 << 10;

/// The postings of one build, spread over partition files by bucket.
pub struct Sink {
    bits: u8,
    part_bits: u8,
    parts: Vec<Mutex<Partition>>,
    /// The partition files' buffers, held until [`Sink::finish`].
    _memory: Hold,
}

struct Partition {
    path: PathBuf,
    out: BufWriter<File>,
    postings: u64,
}

impl Sink {
    /// Partition files in `dir` for buckets of `bits` bits, their buffers
    /// held in the search budget first.
    pub fn create(dir: &Path, bits: u8, progress: &Progress) -> Result<Sink, SearchError> {
        let part_bits = MAX_PART_BITS.min(bits.saturating_sub(DEEP_BLOCK_BITS));
        let memory = reserve((1 << part_bits) * PART_BUFFER, progress)?;
        let mut parts = Vec::new();
        for p in 0..1usize << part_bits {
            let path = dir.join(format!("deep-{p}"));
            let out = BufWriter::with_capacity(PART_BUFFER, File::create(&path).map_err(|e| io(&path, e))?);
            parts.push(Mutex::new(Partition { path, out, postings: 0 }));
        }
        Ok(Sink { bits, part_bits, parts, _memory: memory })
    }

    pub fn bits(&self) -> u8 {
        self.bits
    }

    /// What the partition files' buffers hold in the search budget.
    pub fn bytes(&self) -> usize {
        self.parts.len() * PART_BUFFER
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
        let Sink { parts, _memory, .. } = self;
        for part in parts {
            let Partition { path, out, postings } = part.into_inner().unwrap_or_else(|e| e.into_inner());
            out.into_inner().map_err(|e| io(&path, e.into_error()))?;
            done.push((path, postings));
        }
        Ok(done)
    }
}

/// Block bytes gathered before they are written.
const PENDING: usize = 64 << 10;
/// What writing the section holds whatever the postings: the block bytes
/// gathered, a block's bucket counts and the table at its largest.
pub const FIXED_BYTES: usize =
    PENDING + BLOCK_BUCKETS * 8 + (1 << (MAX_DEEP_BITS - DEEP_BLOCK_BITS)) * DEEP_BLOCK_ENTRY;
/// The buffer of a sorted chunk's writer, and of the merged file's.
const CHUNK_WRITER: usize = 64 << 10;
/// What a partition is read through.
const READ_BUFFER: usize = 64 << 10;
/// The least a merge reads at a time from each sorted chunk.
const MIN_BUFFER: usize = 4 << 10;
/// The least memory the section is written in: its fixed part, and room to
/// sort and merge at least two chunks.
pub const MIN_MEMORY: usize = FIXED_BYTES + CHUNK_WRITER + READ_BUFFER + 2 * (MIN_BUFFER + 16);

/// Writes the section's blocks to `out`, which is at `offset` in the index
/// file, from `parts` in bucket order, each removed once written, holding at
/// most `memory` bytes of the search budget at once. A partition whose
/// postings fit is sorted in memory; a larger one in sorted chunks on disk,
/// merged into one file that its blocks are then read from. Returns the table
/// of blocks and the postings kept, a game once per bucket.
pub fn write_section(
    parts: &[(PathBuf, u64)],
    bits: u8,
    out: &mut impl Write,
    offset: u64,
    target: &Path,
    progress: &Progress,
    memory: usize,
) -> Result<(Vec<u8>, u64), SearchError> {
    let room = memory.checked_sub(FIXED_BYTES).filter(|_| memory >= MIN_MEMORY).ok_or(SearchError::TooLarge)?;
    let _fixed = reserve(FIXED_BYTES, progress)?;
    let blocks = 1usize << (bits - DEEP_BLOCK_BITS);
    let blocks_per_part = blocks / parts.len().max(1);
    let mut w = Blocks {
        out,
        target,
        offset,
        table: Vec::with_capacity(blocks * DEEP_BLOCK_ENTRY),
        pending: Vec::with_capacity(PENDING),
        start: offset,
        crc: !0,
    };
    let mut kept = 0u64;
    for (p, (path, postings)) in parts.iter().enumerate() {
        let first = p * blocks_per_part;
        if postings.saturating_mul(8) <= (room - READ_BUFFER) as u64 {
            let _memory = reserve(*postings as usize * 8 + READ_BUFFER, progress)?;
            let mut all = read_postings(path, *postings)?;
            let _ = std::fs::remove_file(path);
            all.sort_unstable();
            all.dedup();
            kept += all.len() as u64;
            w.write_slice(&all, first, blocks_per_part, path)?;
        } else {
            let sorted = merged_path(path);
            let written = sort_on_disk(path, *postings, room, progress).and_then(|count| {
                let _ = std::fs::remove_file(path);
                let _memory = reserve(room, progress)?;
                w.write_file(&sorted, count, first, blocks_per_part, room / 2)
            });
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(&sorted);
            kept += written?;
        }
    }
    Ok((w.table, kept))
}

/// The blocks as they are written: each block's bytes go out as they come,
/// and its offset, length and CRC go to the table once it ends.
struct Blocks<'a, W: Write> {
    out: &'a mut W,
    target: &'a Path,
    offset: u64,
    table: Vec<u8>,
    pending: Vec<u8>,
    start: u64,
    crc: u32,
}

impl<W: Write> Blocks<'_, W> {
    fn begin(&mut self) {
        self.start = self.offset;
        self.crc = !0;
    }

    fn put(&mut self, v: u64) -> Result<(), SearchError> {
        varint(&mut self.pending, v);
        // A varint takes ten bytes at most: the room reserved is never passed.
        if self.pending.len() + 10 > PENDING {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), SearchError> {
        self.out.write_all(&self.pending).map_err(|e| io(self.target, e))?;
        self.crc = crc32_update(self.crc, &self.pending);
        self.offset += self.pending.len() as u64;
        self.pending.clear();
        Ok(())
    }

    fn end(&mut self) -> Result<(), SearchError> {
        self.flush()?;
        let len = u32::try_from(self.offset - self.start).map_err(|_| SearchError::TooLarge)?;
        self.table.extend(self.start.to_le_bytes());
        self.table.extend(len.to_le_bytes());
        self.table.extend((!self.crc).to_le_bytes());
        Ok(())
    }

    /// Blocks `first..first + count` from a partition's sorted postings.
    fn write_slice(&mut self, all: &[u64], first: usize, count: usize, path: &Path) -> Result<(), SearchError> {
        let mut rest = all;
        for b in first..first + count {
            let first_bucket = (b * BLOCK_BUCKETS) as u64;
            self.begin();
            for bucket in first_bucket..first_bucket + BLOCK_BUCKETS as u64 {
                let n = rest.partition_point(|&x| x >> 32 == bucket);
                self.put(n as u64)?;
                let mut last = 0u64;
                for &x in &rest[..n] {
                    let game = x & 0xffff_ffff;
                    self.put(game - last)?;
                    last = game;
                }
                rest = &rest[n..];
            }
            self.end()?;
        }
        if !rest.is_empty() {
            return Err(outside(path));
        }
        Ok(())
    }

    /// Blocks `first..first + count` from the `postings` sorted in the file
    /// at `path`, read twice per block through buffers of `buffer` bytes:
    /// once for its buckets' counts, then for their games. Returns the
    /// postings written.
    fn write_file(
        &mut self,
        path: &Path,
        postings: u64,
        first: usize,
        count: usize,
        buffer: usize,
    ) -> Result<u64, SearchError> {
        let mut counts = Vec::new();
        counts.try_reserve_exact(BLOCK_BUCKETS).map_err(|_| SearchError::Busy)?;
        counts.resize(BLOCK_BUCKETS, 0u64);
        let mut ahead = PostingReader::open(path, buffer)?;
        let mut games = PostingReader::open(path, buffer)?;
        let mut at = 0u64;
        for b in first..first + count {
            let first_bucket = (b * BLOCK_BUCKETS) as u64;
            counts.fill(0);
            let mut in_block = 0u64;
            while let Some(x) = ahead.peek()? {
                let local = (x >> 32).checked_sub(first_bucket).ok_or_else(|| outside(path))?;
                if local >= BLOCK_BUCKETS as u64 {
                    break;
                }
                counts[local as usize] += 1;
                ahead.next()?;
                in_block += 1;
            }
            games.seek(at)?;
            self.begin();
            for &n in &counts {
                self.put(n)?;
                let mut last = 0u64;
                for _ in 0..n {
                    let game = games.next()?.ok_or_else(|| outside(path))? & 0xffff_ffff;
                    self.put(game - last)?;
                    last = game;
                }
            }
            self.end()?;
            at += in_block;
        }
        if at != postings {
            return Err(outside(path));
        }
        Ok(at)
    }
}

fn outside(path: &Path) -> SearchError {
    io(path, std::io::Error::other("a posting lies outside its partition"))
}

fn merged_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".sorted");
    path.with_file_name(name)
}

/// Sorts the partition at `path`, too large for `room` bytes, into the file
/// at [`merged_path`], repeated postings dropped: chunks that fit `room`
/// beside a reader and a writer are sorted and written apart, then merged,
/// each read through an equal share of `room`. Returns the postings kept. A
/// partition whose merge would read through less than [`MIN_BUFFER`] a chunk
/// is `TooLarge` at once.
fn sort_on_disk(path: &Path, postings: u64, room: usize, progress: &Progress) -> Result<u64, SearchError> {
    let cap = ((room - CHUNK_WRITER - READ_BUFFER) / 8) as u64;
    let k = postings.div_ceil(cap) as usize;
    let buffer = merge_buffer(room, k).ok_or(SearchError::TooLarge)?;
    let mut chunks = Vec::new();
    let result = sort_chunks(path, postings, cap as usize, room, progress, &mut chunks)
        .and_then(|()| merge_chunks(&chunks, &merged_path(path), buffer, room, progress));
    for (chunk, _) in &chunks {
        let _ = std::fs::remove_file(chunk);
    }
    result
}

/// What a merge of `k` chunks reads each through within `room` beside its
/// writer and heap, a whole number of postings; `None` under [`MIN_BUFFER`].
fn merge_buffer(room: usize, k: usize) -> Option<usize> {
    let each = room.checked_sub(CHUNK_WRITER + k * 16)? / k.max(1) / 8 * 8;
    (each >= MIN_BUFFER).then_some(each)
}

fn sort_chunks(
    path: &Path,
    postings: u64,
    cap: usize,
    room: usize,
    progress: &Progress,
    chunks: &mut Vec<(PathBuf, u64)>,
) -> Result<(), SearchError> {
    let _memory = reserve(room, progress)?;
    let mut file = File::open(path).map_err(|e| io(path, e))?;
    if file.metadata().map_err(|e| io(path, e))?.len() != postings * 8 {
        return Err(io(path, std::io::Error::other("a deep partition has the wrong length")));
    }
    let mut chunk: Vec<u64> = Vec::new();
    chunk.try_reserve_exact(cap).map_err(|_| SearchError::Busy)?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(READ_BUFFER).map_err(|_| SearchError::Busy)?;
    buf.resize(READ_BUFFER, 0);
    let mut left = postings;
    while left > 0 {
        let n = left.min(cap as u64);
        chunk.clear();
        let mut bytes = n as usize * 8;
        while bytes > 0 {
            let m = bytes.min(READ_BUFFER);
            file.read_exact(&mut buf[..m]).map_err(|e| io(path, e))?;
            chunk.extend(buf[..m].as_chunks::<8>().0.iter().map(|c| u64::from_le_bytes(*c)));
            bytes -= m;
        }
        chunk.sort_unstable();
        chunk.dedup();
        let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(format!(".chunk-{}", chunks.len()));
        let chunk_path = path.with_file_name(name);
        chunks.push((chunk_path.clone(), chunk.len() as u64));
        let file = File::create(&chunk_path).map_err(|e| io(&chunk_path, e))?;
        let mut out = BufWriter::with_capacity(CHUNK_WRITER, file);
        for x in &chunk {
            out.write_all(&x.to_le_bytes()).map_err(|e| io(&chunk_path, e))?;
        }
        out.into_inner().map_err(|e| io(&chunk_path, e.into_error()))?;
        left -= n;
    }
    Ok(())
}

fn merge_chunks(
    chunks: &[(PathBuf, u64)],
    target: &Path,
    buffer: usize,
    room: usize,
    progress: &Progress,
) -> Result<u64, SearchError> {
    let _memory = reserve(room, progress)?;
    let mut readers = Vec::new();
    readers.try_reserve_exact(chunks.len()).map_err(|_| SearchError::Busy)?;
    let mut heap = BinaryHeap::new();
    heap.try_reserve_exact(chunks.len()).map_err(|_| SearchError::Busy)?;
    for (i, (chunk, _)) in chunks.iter().enumerate() {
        let mut r = PostingReader::open(chunk, buffer)?;
        if let Some(x) = r.next()? {
            heap.push(Reverse((x, i)));
        }
        readers.push(r);
    }
    let mut out = BufWriter::with_capacity(CHUNK_WRITER, File::create(target).map_err(|e| io(target, e))?);
    let (mut kept, mut last) = (0u64, None);
    while let Some(Reverse((x, i))) = heap.pop() {
        if last != Some(x) {
            out.write_all(&x.to_le_bytes()).map_err(|e| io(target, e))?;
            kept += 1;
            last = Some(x);
        }
        if let Some(next) = readers[i].next()? {
            heap.push(Reverse((next, i)));
        }
    }
    out.into_inner().map_err(|e| io(target, e.into_error()))?;
    Ok(kept)
}

/// Postings read in order from a file through a buffer of a fixed size.
struct PostingReader {
    file: File,
    path: PathBuf,
    buf: Vec<u8>,
    at: usize,
    end: usize,
}

impl PostingReader {
    fn open(path: &Path, buffer: usize) -> Result<PostingReader, SearchError> {
        let file = File::open(path).map_err(|e| io(path, e))?;
        let mut buf = Vec::new();
        buf.try_reserve_exact(buffer).map_err(|_| SearchError::Busy)?;
        buf.resize(buffer / 8 * 8, 0);
        Ok(PostingReader { file, path: path.to_path_buf(), buf, at: 0, end: 0 })
    }

    /// Goes to the posting at `index`.
    fn seek(&mut self, index: u64) -> Result<(), SearchError> {
        self.file.seek(SeekFrom::Start(index * 8)).map_err(|e| io(&self.path, e))?;
        self.at = 0;
        self.end = 0;
        Ok(())
    }

    fn peek(&mut self) -> Result<Option<u64>, SearchError> {
        if self.at == self.end {
            self.end = read_full(&mut self.file, &mut self.buf).map_err(|e| io(&self.path, e))?;
            self.at = 0;
            if !self.end.is_multiple_of(8) {
                return Err(io(&self.path, std::io::Error::other("a deep file ends inside a posting")));
            }
        }
        Ok((self.at < self.end)
            .then(|| u64::from_le_bytes(self.buf[self.at..self.at + 8].try_into().unwrap_or_default())))
    }

    fn next(&mut self) -> Result<Option<u64>, SearchError> {
        let x = self.peek()?;
        if x.is_some() {
            self.at += 8;
        }
        Ok(x)
    }
}

/// Fills `buf` as far as the file goes; the bytes read.
fn read_full(file: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match file.read(&mut buf[n..])? {
            0 => break,
            m => n += m,
        }
    }
    Ok(n)
}

fn read_postings(path: &Path, postings: u64) -> Result<Vec<u64>, SearchError> {
    let mut file = File::open(path).map_err(|e| io(path, e))?;
    let len = file.metadata().map_err(|e| io(path, e))?.len();
    if len != postings * 8 {
        return Err(io(path, std::io::Error::other("a deep partition has the wrong length")));
    }
    let mut all = Vec::new();
    all.try_reserve_exact(postings as usize).map_err(|_| SearchError::TooLarge)?;
    let mut buf = vec![0u8; READ_BUFFER];
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
    use crate::indexdir::crc32;

    #[test]
    fn postings_come_back_per_bucket_in_game_order() {
        let dir = std::env::temp_dir().join(format!("bridge-deep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bits = 14;
        let sink = Sink::create(&dir, bits, &Progress::default()).unwrap();
        let posting = |bucket: u64, game: u64| bucket << 32 | game;
        let last = (1u64 << bits) - 1;
        // Two workers, out of order, with a game twice in one bucket.
        sink.add(&mut vec![posting(5, 9), posting(last, 2), posting(5, 3)]).unwrap();
        sink.add(&mut vec![posting(5, 9), posting(4096, 7), posting(0, 1)]).unwrap();
        let parts = sink.finish().unwrap();
        let mut out = Vec::new();
        let (table, kept) =
            write_section(&parts, bits, &mut out, 1000, &dir.join("x"), &Progress::default(), 64 << 20).unwrap();
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
        // So is a delta that wraps around to a game already listed, and a
        // count or a delta written past 64 bits, which would read as 0 or 5
        // were the bits beyond cut off.
        let mut wrap = Vec::new();
        for v in [2, 5, u64::MAX - 2] {
            varint(&mut wrap, v);
        }
        assert_eq!(bucket_games(&wrap, 0, 100), None);
        let long = [0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02];
        assert_eq!(bucket_games(&long, 0, 100), None);
        let mut delta = vec![1, 0x85];
        delta.extend(&long[1..]);
        assert_eq!(bucket_games(&delta, 0, 100), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Postings of `bits`-bit buckets, repeats among them, from a fixed seed.
    fn postings(bits: u8, n: usize) -> Vec<u64> {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        (0..n)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                // Every tenth repeats the one before it.
                let j = if i % 10 == 9 { x.wrapping_sub(1) } else { x };
                ((j >> (64 - bits)) << 32) | ((j & 0x3ff) + 1)
            })
            .collect()
    }

    fn written(dir: &Path, bits: u8, all: &[u64], memory: usize) -> Result<(Vec<u8>, Vec<u8>, u64), SearchError> {
        std::fs::create_dir_all(dir).unwrap();
        let sink = Sink::create(dir, bits, &Progress::default()).unwrap();
        for batch in all.chunks(7_000) {
            sink.add(&mut batch.to_vec()).unwrap();
        }
        let parts = sink.finish().unwrap();
        let mut out = Vec::new();
        let result = write_section(&parts, bits, &mut out, 0, &dir.join("x"), &Progress::default(), memory);
        let _ = std::fs::remove_dir_all(dir);
        result.map(|(table, kept)| (out, table, kept))
    }

    /// A partition larger than the memory given is sorted in chunks on disk
    /// and merged, into the very blocks an in-memory sort writes.
    #[test]
    fn a_partition_sorted_on_disk_gives_the_same_blocks() {
        let base = std::env::temp_dir().join(format!("bridge-deep-disk-{}", std::process::id()));
        let bits = 14;
        let all = postings(bits, 60_000);
        let in_memory = written(&base.join("memory"), bits, &all, 64 << 20).unwrap();
        // Four partitions of about 120 KB each, against 72 KB of room.
        const { assert!(15_000 * 8 > MIN_MEMORY - FIXED_BYTES - READ_BUFFER) };
        let on_disk = written(&base.join("disk"), bits, &all, MIN_MEMORY).unwrap();
        assert_eq!(on_disk, in_memory);
        let mut unique = all.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(in_memory.2, unique.len() as u64);
        // Less than the least memory is refused at once, as is a partition
        // whose chunks could not each be read through a buffer.
        assert!(matches!(written(&base.join("less"), bits, &all, MIN_MEMORY - 1), Err(SearchError::TooLarge)));
        let one = postings(12, 40_000);
        assert!(matches!(written(&base.join("many"), 12, &one, MIN_MEMORY), Err(SearchError::TooLarge)));
        assert!(written(&base.join("enough"), 12, &one, MIN_MEMORY + (256 << 10)).is_ok());
    }
}
