//! Reading an index file with positional reads: the header's counts are
//! checked against the file's length before anything is allocated, the table
//! of blocks is checked when it opens, and every block against its CRC when a
//! lookup reads it.

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::search::memory::{Hold, Refused};

use super::format::{
    BLOCK_ENTRY, BLOCK_KEYS, Block, HEADER_LEN, Header, KEY_ENTRY, MAX_BLOCK_DATA, MIN_RECORD, Stats, crc32, u32_at,
    u64_at,
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
        if header.file_len != len
            || header.table_offset.checked_add(table_len) != Some(len)
            || blocks > keys
            || keys > blocks.saturating_mul(BLOCK_KEYS as u64)
            || keys.checked_mul((KEY_ENTRY + MIN_RECORD) as u64).is_none_or(|b| b > data)
        {
            return Err(Bad::Corrupt("counts do not fit the file"));
        }
        let memory = Hold::reserve_quietly(table_len as usize + blocks as usize * std::mem::size_of::<Block>())
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
        Ok(IndexFile { file, path: path.to_path_buf(), header, blocks: decoded, _memory: memory })
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

#[cfg(unix)]
fn read_at(file: &File, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
fn read_at(file: &File, mut offset: u64, mut buf: &mut [u8]) -> std::io::Result<()> {
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
            file_len: table_offset + u64::from(blocks) * BLOCK_ENTRY as u64,
        };
        let f = File::create(&path).unwrap();
        f.set_len(h.file_len).unwrap();
        std::os::unix::fs::FileExt::write_all_at(&f, &h.encode(), 0).unwrap();
        assert!(matches!(IndexFile::open(&path), Err(Bad::Corrupt(_))));
        std::fs::remove_file(&path).unwrap();
    }
}
