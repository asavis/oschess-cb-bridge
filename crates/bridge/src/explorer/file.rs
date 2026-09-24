//! Reading an index file with positional reads: the header and the table of
//! blocks are checked when it opens, and every block against its CRC when a
//! lookup reads it.

use std::fs::File;
use std::path::{Path, PathBuf};

use super::format::{BLOCK_ENTRY, BLOCK_KEYS, Block, HEADER_LEN, Header, KEY_ENTRY, Stats, crc32, u32_at, u64_at};

/// Why an index file cannot be used; any of them means a rebuild.
#[derive(Debug)]
pub enum Bad {
    /// No file, or it cannot be read now.
    Io(std::io::Error),
    /// The file is not an index of this version, is cut short, or a CRC fails.
    Corrupt(&'static str),
}

pub struct IndexFile {
    file: File,
    pub path: PathBuf,
    pub header: Header,
    blocks: Vec<Block>,
}

impl IndexFile {
    pub fn open(path: &Path) -> Result<IndexFile, Bad> {
        let file = File::open(path).map_err(Bad::Io)?;
        let len = file.metadata().map_err(Bad::Io)?.len();
        let mut head = [0u8; HEADER_LEN];
        read_at(&file, 0, &mut head).map_err(Bad::Io)?;
        let header = Header::decode(&head).ok_or(Bad::Corrupt("header"))?;
        let table_len = header.blocks as u64 * BLOCK_ENTRY as u64;
        if header.file_len != len || header.table_offset.checked_add(table_len) != Some(len) {
            return Err(Bad::Corrupt("length"));
        }
        // The table is 28 bytes a block: some 240 KB for 35 million positions.
        let mut table = vec![0u8; table_len as usize];
        read_at(&file, header.table_offset, &mut table).map_err(Bad::Io)?;
        if crc32(&table) != header.table_crc {
            return Err(Bad::Corrupt("block table"));
        }
        let blocks: Vec<Block> = table.as_chunks::<BLOCK_ENTRY>().0.iter().map(|b| Block::decode(b)).collect();
        let mut end = HEADER_LEN as u64;
        let mut keys = 0u64;
        for b in &blocks {
            // A block holds at most BLOCK_KEYS records of at most a few
            // hundred bytes each, so 64 MiB bounds what a lookup reads.
            if b.offset != end || b.keys == 0 || b.keys as usize > BLOCK_KEYS || b.data_len > 64 << 20 {
                return Err(Bad::Corrupt("block layout"));
            }
            end += b.bytes() as u64;
            keys += u64::from(b.keys);
        }
        if end != header.table_offset
            || keys != header.keys
            || !blocks.windows(2).all(|w| w[0].first_key < w[1].first_key)
        {
            return Err(Bad::Corrupt("block layout"));
        }
        Ok(IndexFile { file, path: path.to_path_buf(), header, blocks })
    }

    /// The position `key`, `None` when the index does not hold it.
    pub fn lookup(&self, key: u64) -> Result<Option<Stats>, Bad> {
        let i = self.blocks.partition_point(|b| b.first_key <= key);
        let Some(block) = i.checked_sub(1).map(|i| self.blocks[i]) else { return Ok(None) };
        let mut buf = vec![0u8; block.bytes()];
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
