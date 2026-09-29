//! What the bridge's index folders share (#119). The data folder keeps files
//! of each database: its position index and move stream, heads file and
//! names files in `index`, and the header index of a PGN file in `pgn`. Each
//! kind has a registry, which sweeps its folder in its own way; this module
//! holds what they have in common: the names of the files, how long they
//! outlive their database's place on the list (#60), and the CRC-32 and
//! fixed-width reads the files are checked and decoded with.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// How long a database's files stay in an index folder after it left the
/// list (#60): a list that loses a database for a moment, as while ChessBase
/// rewrites its list, must not cost a rebuild: for the Mega Database, half a
/// minute of half the search workers.
pub const SWEEP_GRACE: Duration = Duration::from_secs(10 * 60);

/// Empties `dir`, an index folder the bridge no longer uses (#147), of the
/// files and folders it wrote there, those whose names `kept` knows, then
/// removes it once nothing else is in it: anything else stays, and so does
/// the folder then. Each step is tried once, and what stays is tried again at
/// the next start.
pub fn sweep_moved(dir: &Path, kept: impl Fn(&str) -> bool) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if !entry.file_name().to_str().is_some_and(&kept) {
            continue;
        }
        // A link is removed, never followed.
        let _ = match entry.file_type() {
            Ok(t) if t.is_dir() => std::fs::remove_dir_all(entry.path()),
            _ => std::fs::remove_file(entry.path()),
        };
    }
    let _ = std::fs::remove_dir(dir);
}

/// The database id of an index folder entry named `<id><suffix>`, for one of
/// `suffixes`, and the index of that suffix; `None` for anything else, which
/// the bridge did not write and never touches. The id is 16 hex digits in
/// lower case, as the bridge writes it: on a file system that ignores case, as
/// Windows's does, a name in upper case is also the file of the id in lower
/// case, which may be listed.
pub fn db_id<'a>(name: &'a str, suffixes: &[&str]) -> Option<(&'a str, usize)> {
    let (id, suffix) = (name.get(..16)?, name.get(16..)?);
    let kind = suffixes.iter().position(|s| *s == suffix)?;
    id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)).then_some((id, kind))
}

/// The databases off the list whose files a sweep found in an index folder,
/// since when, and how long their files outlive that. A sweep asks
/// [`Unlisted::due`] of each such file, marks with [`Unlisted::still`] each
/// database a file of which stays, and ends with [`Unlisted::retain`].
pub struct Unlisted {
    since: HashMap<String, Instant>,
    /// The databases marked during the sweep that runs.
    still: HashSet<String>,
    grace: Duration,
}

impl Default for Unlisted {
    fn default() -> Unlisted {
        Unlisted { since: HashMap::new(), still: HashSet::new(), grace: SWEEP_GRACE }
    }
}

impl Unlisted {
    /// Sets how long a database's files outlive its place on the list.
    pub fn set_grace(&mut self, grace: Duration) {
        self.grace = grace;
    }

    /// How long a database's files outlive its place on the list.
    pub fn grace(&self) -> Duration {
        self.grace
    }

    /// Whether a file of `id`, found off the list at `now`, goes: the grace
    /// has passed since a sweep first found `id` off the list.
    pub fn due(&mut self, id: &str, now: Instant) -> bool {
        let since = *self.since.entry(id.to_string()).or_insert(now);
        now.duration_since(since) >= self.grace
    }

    /// Keeps the time `id` was first found off the list: a file of it stays.
    pub fn still(&mut self, id: &str) {
        self.still.insert(id.to_string());
    }

    /// Ends a sweep. A database none of whose files stayed, being back on the
    /// list or its files gone, starts afresh.
    pub fn retain(&mut self) {
        let still = std::mem::take(&mut self.still);
        self.since.retain(|id, _| still.contains(id));
    }
}

/// The little-endian `u32` at `at` in `b`; zero when `b` ends before it, so
/// that a damaged file never panics the bridge. Callers bound their offsets.
pub fn u32_at(b: &[u8], at: usize) -> u32 {
    b.get(at..).and_then(|b| b.first_chunk()).map_or(0, |c| u32::from_le_bytes(*c))
}

/// The little-endian `u64` at `at` in `b`; zero when `b` ends before it, as
/// [`u32_at`].
pub fn u64_at(b: &[u8], at: usize) -> u64 {
    b.get(at..).and_then(|b| b.first_chunk()).map_or(0, |c| u64::from_le_bytes(*c))
}

/// CRC-32 (IEEE 802.3), as zlib and PNG compute it, eight bytes at a time:
/// every index file is checked with it, each block of the position index and
/// of the heads file as it is read.
pub fn crc32(bytes: &[u8]) -> u32 {
    !crc32_update(!0, bytes)
}

/// Continues a CRC-32 over `bytes` from `state`, which starts at `!0`; the
/// CRC of all the bytes so fed is `!state`.
pub fn crc32_update(state: u32, bytes: &[u8]) -> u32 {
    let t = &*TABLES;
    let mut c = state;
    let (chunks, rest) = bytes.as_chunks::<8>();
    for b in chunks {
        let lo = c ^ u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        c = t[7][(lo & 0xff) as usize]
            ^ t[6][(lo >> 8 & 0xff) as usize]
            ^ t[5][(lo >> 16 & 0xff) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][b[4] as usize]
            ^ t[2][b[5] as usize]
            ^ t[1][b[6] as usize]
            ^ t[0][b[7] as usize];
    }
    for &b in rest {
        c = t[0][((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
    }
    c
}

static TABLES: LazyLock<[[u32; 256]; 8]> = LazyLock::new(|| {
    let mut t = [[0u32; 256]; 8];
    for i in 0..256u32 {
        let mut c = i;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        t[0][i as usize] = c;
    }
    for i in 0..256 {
        for k in 1..8 {
            t[k][i] = (t[k - 1][i] >> 8) ^ t[0][(t[k - 1][i] & 0xff) as usize];
        }
    }
    t
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    /// Eight bytes at a time gives the CRC a byte, and a bit, at a time
    /// gives: for every length up to 64, so whole words with each rest, and
    /// fed in two pieces as a names file is.
    #[test]
    fn the_crc_is_the_bytewise_crc() {
        let bytewise = |bytes: &[u8]| {
            let mut c = !0u32;
            for &b in bytes {
                c ^= u32::from(b);
                for _ in 0..8 {
                    c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
                }
            }
            !c
        };
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let bytes: Vec<u8> = (0..64)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        for len in 0..=64 {
            assert_eq!(crc32(&bytes[..len]), bytewise(&bytes[..len]), "{len}");
        }
        assert_eq!(!crc32_update(crc32_update(!0, &bytes[..13]), &bytes[13..]), crc32(&bytes));
    }

    /// Only a name the bridge writes is a database's file: 16 hex digits in
    /// lower case, then one of the folder's suffixes.
    #[test]
    fn a_file_name_is_a_database_id_and_a_known_suffix() {
        let suffixes = [".idx", ".idx.partial", ".build"];
        assert_eq!(db_id("0123456789abcdef.idx", &suffixes), Some(("0123456789abcdef", 0)));
        assert_eq!(db_id("0123456789abcdef.idx.partial", &suffixes), Some(("0123456789abcdef", 1)));
        assert_eq!(db_id("fedcba9876543210.build", &suffixes), Some(("fedcba9876543210", 2)));
        // Windows folds case: this is also the file of 0123456789abcdef.
        assert_eq!(db_id("0123456789ABCDEF.idx", &suffixes), None);
        assert_eq!(db_id("0123456789abcde.idx", &suffixes), None);
        assert_eq!(db_id("0123456789abcdef0.idx", &suffixes), None);
        assert_eq!(db_id("0123456789abcdeg.idx", &suffixes), None);
        assert_eq!(db_id("short.idx", &suffixes), None);
        assert_eq!(db_id("0123456789abcdef.heads", &suffixes), None);
        assert_eq!(db_id("0123456789abcdef.idx.old", &suffixes), None);
        assert_eq!(db_id("0123456789abcdef", &suffixes), None);
        // The 16th byte falls inside a character.
        assert_eq!(db_id("0123456789abcdeé.idx", &suffixes), None);
    }

    /// A database off the list keeps its files within the grace, and they go
    /// after it; one back on the list meanwhile starts afresh when it leaves
    /// again.
    #[test]
    fn files_off_the_list_go_once_the_grace_has_passed() {
        let mut unlisted = Unlisted::default();
        assert_eq!(unlisted.grace(), SWEEP_GRACE);
        unlisted.set_grace(Duration::from_secs(600));
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        // Each sweep: a file of `id` found off the list, which stays unless due.
        let sweep = |unlisted: &mut Unlisted, id: &str, s: u64| {
            let due = unlisted.due(id, at(s));
            if !due {
                unlisted.still(id);
            }
            unlisted.retain();
            due
        };

        assert!(!sweep(&mut unlisted, "a", 0));
        assert!(!sweep(&mut unlisted, "a", 599));
        assert!(sweep(&mut unlisted, "a", 600));

        assert!(!sweep(&mut unlisted, "b", 0));
        // Back on the list: the sweep does not ask about it.
        unlisted.retain();
        assert!(!sweep(&mut unlisted, "b", 700));
        assert!(!sweep(&mut unlisted, "b", 1299));
        assert!(sweep(&mut unlisted, "b", 1300));
    }

    /// A folder the indexes moved out of loses what the bridge wrote there,
    /// and goes once nothing else is left.
    #[test]
    fn a_moved_index_folder_is_emptied_of_the_bridges_files() {
        let dir = std::env::temp_dir().join(format!("bridge-moved-index-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("0123456789abcdef.build").join("runs")).unwrap();
        for name in ["0123456789abcdef.idx", "0123456789abcdef.heads", "notes.txt"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let kept = |name: &str| name.starts_with("0123456789abcdef");
        sweep_moved(&dir, kept);
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, ["notes.txt"], "the folder keeps what the bridge did not write");
        std::fs::remove_file(dir.join("notes.txt")).unwrap();
        std::fs::write(dir.join("0123456789abcdef.moves"), b"x").unwrap();
        sweep_moved(&dir, kept);
        assert!(!dir.exists(), "an emptied folder goes");
        sweep_moved(&dir, kept);
    }

    #[test]
    fn a_short_read_is_zero_not_a_panic() {
        let b = [1, 0, 0, 0, 0, 0, 0, 2];
        assert_eq!((u32_at(&b, 0), u32_at(&b, 4)), (1, 0x0200_0000));
        assert_eq!(u64_at(&b, 0), 0x0200_0000_0000_0001);
        assert_eq!((u32_at(&b, 5), u32_at(&b, 9), u32_at(&b, usize::MAX)), (0, 0, 0));
        assert_eq!((u64_at(&b, 1), u64_at(&b, usize::MAX)), (0, 0));
    }
}
