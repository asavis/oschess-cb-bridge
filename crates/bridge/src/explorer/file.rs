//! Reading an index file with positional reads: the header's counts are
//! checked against the file's length before anything is allocated, the table
//! of blocks is checked when it opens, and every block against its CRC when a
//! lookup reads it.

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::indexdir::{crc32, u32_at, u64_at};
use crate::search::memory::{Hold, Refused};

use super::deep::{BLOCK_BUCKETS, bucket_games};
use super::format::{
    BLOCK_ENTRY, BLOCK_KEYS, Block, DEEP_BLOCK_ENTRY, HEADER_LEN, Header, KEY_ENTRY, MAX_BLOCK_DATA, MAX_DEEP_BITS,
    MIN_DEEP_BITS, MIN_RECORD, Stats,
};

/// Why an index file cannot be used.
#[derive(Debug)]
pub enum Bad {
    /// No file, or it cannot be read now.
    Io(std::io::Error),
    /// The file is not an index of this version, its counts do not fit its
    /// length, or a CRC fails: it is rebuilt.
    Corrupt(&'static str),
    /// The search budget has no room for its table now; the next request
    /// tries again.
    Busy,
}

pub struct IndexFile {
    file: File,
    pub path: PathBuf,
    pub header: Header,
    blocks: Vec<Block>,
    /// The deep section's blocks (#133): offset, length and CRC of each.
    deep: Vec<(u64, u32, u32)>,
    _memory: Hold,
}

impl IndexFile {
    pub fn open(path: &Path) -> Result<IndexFile, Bad> {
        let file = File::open(path).map_err(Bad::Io)?;
        let len = file.metadata().map_err(Bad::Io)?.len();
        let mut head = [0u8; HEADER_LEN];
        read_at(&file, 0, &mut head).map_err(Bad::Io)?;
        let header = Header::decode(&head).ok_or(Bad::Corrupt("header"))?;
        let (blocks, keys) = (u64::from(header.blocks), header.keys);
        // Every count must fit the bytes the file has, before anything is
        // allocated from them: each block holds 1 to BLOCK_KEYS keys, and
        // each key a 12-byte entry and a record of at least MIN_RECORD bytes.
        let data = header.table_offset.checked_sub(HEADER_LEN as u64).ok_or(Bad::Corrupt("table offset"))?;
        let table_len = blocks.checked_mul(BLOCK_ENTRY as u64).ok_or(Bad::Corrupt("block count"))?;
        if !(MIN_DEEP_BITS..=MAX_DEEP_BITS).contains(&header.deep_bits) {
            return Err(Bad::Corrupt("deep bits"));
        }
        let deep_table_len = header.deep_blocks() * DEEP_BLOCK_ENTRY as u64;
        if header.file_len != len
            || header.table_offset.checked_add(table_len) != Some(header.deep_offset)
            || header.deep_table_offset.checked_add(deep_table_len) != Some(len)
            || header.deep_table_offset < header.deep_offset
            || blocks > keys
            || keys > blocks.saturating_mul(BLOCK_KEYS as u64)
            || keys.checked_mul((KEY_ENTRY + MIN_RECORD) as u64).is_none_or(|b| b > data)
        {
            return Err(Bad::Corrupt("counts do not fit the file"));
        }
        let memory = Hold::reserve_quietly(
            table_len as usize + blocks as usize * std::mem::size_of::<Block>() + 2 * deep_table_len as usize,
        )
        .map_err(|r| if r == Refused::TooLarge { Bad::Corrupt("table larger than memory") } else { Bad::Busy })?;
        let mut table = Vec::new();
        table.try_reserve_exact(table_len as usize).map_err(|_| Bad::Busy)?;
        table.resize(table_len as usize, 0);
        read_at(&file, header.table_offset, &mut table).map_err(Bad::Io)?;
        if crc32(&table) != header.table_crc {
            return Err(Bad::Corrupt("block table"));
        }
        let mut decoded = Vec::new();
        decoded.try_reserve_exact(blocks as usize).map_err(|_| Bad::Busy)?;
        decoded.extend(table.as_chunks::<BLOCK_ENTRY>().0.iter().map(|b| Block::decode(b)));
        drop(table);
        let mut end = HEADER_LEN as u64;
        let mut sum = 0u64;
        for b in &decoded {
            if b.offset != end || b.keys == 0 || b.keys as usize > BLOCK_KEYS || b.data_len as usize > MAX_BLOCK_DATA {
                return Err(Bad::Corrupt("block layout"));
            }
            end += b.bytes() as u64;
            sum += u64::from(b.keys);
        }
        if end != header.table_offset || sum != keys || !decoded.windows(2).all(|w| w[0].first_key < w[1].first_key) {
            return Err(Bad::Corrupt("block layout"));
        }
        let deep = deep_table(&file, &header, deep_table_len)?;
        Ok(IndexFile { file, path: path.to_path_buf(), header, blocks: decoded, deep, _memory: memory })
    }

    /// The games whose main line holds a structure of `bucket` past the
    /// tree's pruning ply (#133): the candidates for a position that games
    /// reach beyond the tree's plies (#146). The block and the games, at most
    /// four bytes for each of its bytes, are held in the search budget until
    /// the hold is dropped: `Busy` when it has no room.
    pub fn deep_games(&self, bucket: u32) -> Result<(Vec<u32>, Hold), Bad> {
        let local = bucket as usize % BLOCK_BUCKETS;
        let Some(&(offset, len, crc)) = self.deep.get(bucket as usize / BLOCK_BUCKETS) else {
            return Err(Bad::Corrupt("deep bucket"));
        };
        let memory = Hold::reserve_quietly((len as usize).saturating_mul(5)).map_err(|_| Bad::Busy)?;
        let mut buf = Vec::new();
        buf.try_reserve_exact(len as usize).map_err(|_| Bad::Busy)?;
        buf.resize(len as usize, 0);
        read_at(&self.file, offset, &mut buf).map_err(Bad::Io)?;
        if crc32(&buf) != crc {
            return Err(Bad::Corrupt("deep block"));
        }
        let games = bucket_games(&buf, local, self.header.last_record).ok_or(Bad::Corrupt("deep block"))?;
        Ok((games, memory))
    }

    /// The position `key`, `None` when the index does not hold it.
    pub fn lookup(&self, key: u64) -> Result<Option<Stats>, Bad> {
        let i = self.blocks.partition_point(|b| b.first_key <= key);
        let Some(block) = i.checked_sub(1).map(|i| self.blocks[i]) else { return Ok(None) };
        // At most 4096 keys and MAX_BLOCK_DATA bytes, checked when it opened.
        let mut buf = Vec::new();
        buf.try_reserve_exact(block.bytes()).map_err(|_| Bad::Busy)?;
        buf.resize(block.bytes(), 0);
        read_at(&self.file, block.offset, &mut buf).map_err(Bad::Io)?;
        if crc32(&buf) != block.crc {
            return Err(Bad::Corrupt("block"));
        }
        let (keys, data) = buf.split_at(block.keys as usize * KEY_ENTRY);
        let entries = keys.as_chunks::<KEY_ENTRY>().0;
        let Ok(at) = entries.binary_search_by_key(&key, |e| u64_at(e, 0)) else { return Ok(None) };
        let start = u32_at(&entries[at], 8) as usize;
        let record = data.get(start..).ok_or(Bad::Corrupt("record offset"))?;
        Stats::decode(record).map(Some).ok_or(Bad::Corrupt("record"))
    }
}

/// The deep section's table, checked against its CRC and against the
/// section it describes: blocks back to back from `deep_offset`.
fn deep_table(file: &File, header: &Header, len: u64) -> Result<Vec<(u64, u32, u32)>, Bad> {
    let mut table = Vec::new();
    table.try_reserve_exact(len as usize).map_err(|_| Bad::Busy)?;
    table.resize(len as usize, 0);
    read_at(file, header.deep_table_offset, &mut table).map_err(Bad::Io)?;
    if crc32(&table) != header.deep_table_crc {
        return Err(Bad::Corrupt("deep table"));
    }
    let mut blocks = Vec::new();
    blocks.try_reserve_exact(header.deep_blocks() as usize).map_err(|_| Bad::Busy)?;
    let mut end = header.deep_offset;
    for e in table.as_chunks::<DEEP_BLOCK_ENTRY>().0 {
        let (offset, block_len, crc) = (u64_at(e, 0), u32_at(e, 8), u32_at(e, 12));
        if offset != end {
            return Err(Bad::Corrupt("deep layout"));
        }
        end += u64::from(block_len);
        blocks.push((offset, block_len, crc));
    }
    if end != header.deep_table_offset {
        return Err(Bad::Corrupt("deep layout"));
    }
    Ok(blocks)
}

#[cfg(unix)]
pub(super) fn read_at(file: &File, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
pub(super) fn read_at(file: &File, mut offset: u64, mut buf: &mut [u8]) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn write_at(file: &File, offset: u64, buf: &[u8]) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

#[cfg(windows)]
pub(super) fn write_at(file: &File, mut offset: u64, mut buf: &[u8]) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::explorer::format::{MAX_PLY, PRUNE_PLY};

    /// A header that is valid by its own CRC but claims a table of 33,554,432
    /// blocks, in a sparse file of the length it names: refused before the
    /// 940 MB table would be allocated.
    #[test]
    fn a_header_claiming_more_than_the_file_holds_is_refused_first() {
        let path = std::env::temp_dir().join(format!("bridge-index-claims-{}", std::process::id()));
        let blocks: u32 = 1 << 25;
        let table_offset = 4096u64;
        let deep_offset = table_offset + u64::from(blocks) * BLOCK_ENTRY as u64;
        let h = Header {
            max_ply: MAX_PLY,
            prune_ply: PRUNE_PLY,
            first_record: 1,
            last_record: 1,
            generation: 1,
            games: 1,
            keys: u64::from(blocks),
            blocks,
            table_offset,
            table_crc: 0,
            // A deep section of one empty block after the table, so that only
            // the table's claim is wrong.
            file_len: deep_offset + DEEP_BLOCK_ENTRY as u64,
            deep_bits: MIN_DEEP_BITS,
            deep_postings: 0,
            deep_offset,
            deep_table_offset: deep_offset,
            deep_table_crc: 0,
            build_id: 1,
        };
        let f = File::create(&path).unwrap();
        f.set_len(h.file_len).unwrap();
        std::os::unix::fs::FileExt::write_all_at(&f, &h.encode(), 0).unwrap();
        assert!(matches!(IndexFile::open(&path), Err(Bad::Corrupt(_))));
        std::fs::remove_file(&path).unwrap();
    }
}
