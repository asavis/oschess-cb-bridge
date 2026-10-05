//! The move stream as a build writes it ([`Writer`]): each of the build's
//! workers writes a block of records at a time ([`Part`]).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::explorer::file::write_at;
use crate::explorer::runs::io;
use crate::explorer::source::Line;
use crate::indexdir::crc32;
use crate::search::SearchError;
use crate::sync::lock;

use super::{
    ALIGN, BATCH, CRC_AT, Departures, Entry, HEADER_LEN, Header, INDEXED, MAX_PLIES, PREFIX_AT, PREFIX_BYTES,
    PREFIX_WORDS, RANK_AT, SETUP, SETUP_BYTES, SLOT_BYTES, START_MOVE_AT, TABLE_ENTRY, TAIL_BUFFER, block_crc,
    record_crc, records,
};

/// The stream being written, `<id>.moves.partial`, from its start to its
/// end: each worker appends its tails as they fill its buffer and the slots
/// of each block it ends, so that a game's tail is found only by its offset
/// and a block only by the table.
pub struct Writer {
    pub(crate) anchor: u32,
    file: File,
    path: PathBuf,
    first: u32,
    records: u64,
    /// Where the next append goes: the end of what is appended.
    end: Mutex<u64>,
    /// Each block's offset once appended, 0 until then, and its slots' CRC.
    blocks: Vec<(AtomicU64, AtomicU32)>,
    games: AtomicU64,
    plies: AtomicU64,
}

impl Writer {
    pub(crate) fn rank(&self, line: &Line) -> u32 {
        crate::explorer::ranking::key(line.rating_sum, line.date, self.anchor)
    }

    /// A new stream at `path` for records `first..=last`.
    pub fn create(path: &Path, first: u32, last: u32) -> Result<Writer, SearchError> {
        let records = records(first, last);
        let count = usize::try_from(records.div_ceil(BATCH as u64)).map_err(|_| SearchError::TooLarge)?;
        let mut blocks = Vec::new();
        blocks.try_reserve_exact(count).map_err(|_| SearchError::TooLarge)?;
        blocks.extend((0..count).map(|_| (AtomicU64::new(0), AtomicU32::new(0))));
        let file =
            File::options().read(true).write(true).create(true).truncate(true).open(path).map_err(|e| io(path, e))?;
        Ok(Writer {
            anchor: 0,
            file,
            path: path.to_path_buf(),
            first,
            records,
            end: Mutex::new(HEADER_LEN as u64),
            blocks,
            games: AtomicU64::new(0),
            plies: AtomicU64::new(0),
        })
    }

    /// A worker's part: its buffers, allocated fallibly, which the worker
    /// holds in the budget first ([`super::WORKER_BYTES`]).
    pub fn part(&self) -> Option<Part<'_>> {
        fn buf<T>(n: usize) -> Option<Vec<T>> {
            let mut v = Vec::new();
            v.try_reserve_exact(n).ok()?;
            Some(v)
        }
        Some(Part {
            writer: self,
            block: 0,
            first: 0,
            count: 0,
            slots: buf(BATCH * SLOT_BYTES)?,
            tail: buf(TAIL_BUFFER)?,
            pending: buf(BATCH)?,
            games: 0,
            plies: 0,
        })
    }

    /// Appends `bytes`, a multiple of [`ALIGN`] long, after what is
    /// appended; where they start. Each append takes its place in turn and
    /// is written at once, so the file grows from its start to its end.
    fn append(&self, bytes: &[u8]) -> Result<u64, SearchError> {
        debug_assert!((bytes.len() as u64).is_multiple_of(ALIGN));
        let at = {
            let mut end = lock(&self.end);
            let at = *end;
            *end += bytes.len() as u64;
            at
        };
        write_at(&self.file, at, bytes).map_err(|e| io(&self.path, e))?;
        Ok(at)
    }

    /// Ends the stream of the database at `generation`, built with
    /// `build_id`: the table of blocks after them, then the header. The file
    /// is not synced: a torn one fails its CRCs and is rebuilt.
    pub fn finish(self, generation: u64, build_id: u64) -> Result<Header, SearchError> {
        let table_offset = *lock(&self.end);
        let mut table = Vec::new();
        table.try_reserve_exact(TABLE_ENTRY * self.blocks.len()).map_err(|_| SearchError::TooLarge)?;
        for (at, crc) in &self.blocks {
            let at = at.load(Ordering::Relaxed);
            if at == 0 {
                return Err(SearchError::Bug("a block of the move stream was not written"));
            }
            table.extend(at.to_le_bytes());
            table.extend(crc.load(Ordering::Relaxed).to_le_bytes());
        }
        write_at(&self.file, table_offset, &table).map_err(|e| io(&self.path, e))?;
        let header = Header {
            first_record: self.first,
            last_record: (u64::from(self.first) + self.records).saturating_sub(1) as u32,
            generation,
            build_id,
            games: self.games.load(Ordering::Relaxed),
            plies: self.plies.load(Ordering::Relaxed),
            table_offset,
            blocks: u32::try_from(self.blocks.len()).map_err(|_| SearchError::TooLarge)?,
            table_crc: crc32(&table),
        };
        write_at(&self.file, 0, &header.encode()).map_err(|e| io(&self.path, e))?;
        Ok(header)
    }
}

/// A worker's part of the stream: one block of records at a time.
pub struct Part<'a> {
    writer: &'a Writer,
    /// The block, its first record, and how many records it has.
    block: usize,
    first: u32,
    count: usize,
    slots: Vec<u8>,
    /// Tails not yet appended, and the block's records whose tails they hold,
    /// by their place in the block: their slots count their tail offsets from
    /// the buffer's start, and lack their CRCs, until it is appended.
    tail: Vec<u8>,
    pending: Vec<u32>,
    games: u64,
    plies: u64,
}

impl Part<'_> {
    /// Starts the block of records `first..=last`, at most [`BATCH`] of them
    /// from the start of a block: none is indexed until added.
    pub fn begin(&mut self, first: u32, last: u32) {
        let from = first.saturating_sub(self.writer.first) as usize;
        debug_assert_eq!(from % BATCH, 0, "a block starts at a multiple of BATCH");
        self.block = from / BATCH;
        self.first = first;
        self.count = (last.saturating_sub(first) as usize + 1).min(BATCH);
        self.slots.clear();
        self.slots.resize(self.count * SLOT_BYTES, 0);
        for slot in self.slots.as_chunks_mut::<SLOT_BYTES>().0 {
            slot[PREFIX_AT..PREFIX_AT + PREFIX_BYTES].fill(0xff);
        }
    }

    /// Adds `line`, a game of the block that the index holds.
    pub fn add(&mut self, line: &Line) -> Result<(), SearchError> {
        let Some(i) = line.number.checked_sub(self.first).map(|i| i as usize).filter(|&i| i < self.count) else {
            return Ok(());
        };
        let words = &line.words[..line.words.len().min(MAX_PLIES)];
        let past = &words[words.len().min(PREFIX_WORDS)..];
        let setup = line.setup.as_ref();
        let bytes = setup.map_or(0, |_| SETUP_BYTES) + 2 * past.len();
        let mut tail = 0;
        if bytes > 0 {
            // A tail is at most 131,064 bytes: it always fits an empty buffer.
            if self.tail.len() + bytes > TAIL_BUFFER {
                self.flush()?;
            }
            tail = (self.tail.len() / 2) as u32;
            if let Some(s) = setup {
                self.tail.extend_from_slice(s);
            }
            for w in past {
                self.tail.extend(w.to_le_bytes());
            }
            self.pending.push(i as u32);
        }
        let mut flags = line.outcome as u16 | line.elo.min(4095) << 2 | INDEXED;
        if setup.is_some() {
            flags |= SETUP;
        }
        let departures = if setup.is_some() { Departures::default() } else { line.departures };
        let entry = Entry { tail, plies: words.len() as u16, flags, departures };
        let slot = &mut self.slots[i * SLOT_BYTES..(i + 1) * SLOT_BYTES];
        slot[..PREFIX_AT].copy_from_slice(&entry.encode());
        slot[RANK_AT..RANK_AT + 4].copy_from_slice(&self.writer.rank(line).to_le_bytes());
        if setup.is_some() {
            slot[START_MOVE_AT..START_MOVE_AT + 2].copy_from_slice(&line.start_move.to_le_bytes());
        }
        for (to, w) in slot[PREFIX_AT..PREFIX_AT + PREFIX_BYTES].as_chunks_mut::<2>().0.iter_mut().zip(words) {
            *to = w.to_le_bytes();
        }
        self.games += 1;
        self.plies += words.len() as u64;
        Ok(())
    }

    /// Appends the tails gathered, and ends the slots of the records whose
    /// tails they are: their offsets and their CRCs.
    fn flush(&mut self) -> Result<(), SearchError> {
        if self.tail.is_empty() {
            return Ok(());
        }
        let len = self.tail.len();
        self.tail.resize(len.next_multiple_of(ALIGN as usize), 0);
        let base = self.writer.append(&self.tail)? / 2;
        for &i in &self.pending {
            let slot = &mut self.slots[i as usize * SLOT_BYTES..(i as usize + 1) * SLOT_BYTES];
            let entry = Entry::decode(slot);
            let at = 2 * entry.tail as usize;
            let tail = u32::try_from(base + u64::from(entry.tail)).map_err(|_| SearchError::TooLarge)?;
            slot[0..4].copy_from_slice(&tail.to_le_bytes());
            let bytes = self.tail.get(at..at + entry.tail_bytes().1).unwrap_or_default();
            let crc = record_crc(self.first + i, &slot[..CRC_AT], bytes);
            slot[CRC_AT..].copy_from_slice(&crc.to_le_bytes());
        }
        self.pending.clear();
        self.tail.clear();
        Ok(())
    }

    /// Ends the block: its tails appended, the CRCs of its other records,
    /// then its slots appended and placed in the table with their CRC.
    pub fn end(&mut self) -> Result<(), SearchError> {
        self.flush()?;
        for (i, slot) in self.slots.as_chunks_mut::<SLOT_BYTES>().0.iter_mut().enumerate() {
            if Entry::decode(slot).tail_bytes().1 == 0 {
                let crc = record_crc(self.first + i as u32, &slot[..CRC_AT], &[]);
                slot[CRC_AT..].copy_from_slice(&crc.to_le_bytes());
            }
        }
        let w = self.writer;
        let crc = block_crc(self.block as u32, &self.slots);
        self.slots.resize(self.slots.len().next_multiple_of(ALIGN as usize), 0);
        let at = w.append(&self.slots)?;
        if let Some((block, sum)) = w.blocks.get(self.block) {
            block.store(at, Ordering::Relaxed);
            sum.store(crc, Ordering::Relaxed);
        }
        w.games.fetch_add(std::mem::take(&mut self.games), Ordering::Relaxed);
        w.plies.fetch_add(std::mem::take(&mut self.plies), Ordering::Relaxed);
        Ok(())
    }
}
