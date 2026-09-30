//! What the bridge's index folders share (#119). The data folder keeps files
//! of each database: its position index and move stream, heads file and
//! names files in `index`, and the header index of a PGN file in `pgn`. Each
//! kind has a registry, which sweeps its folder in its own way; this module
//! holds what they have in common: the names of the files, how long they
//! outlive their database's place on the list (#60), how a build's file is
//! written beside its place and renamed into it (#176), and the CRC-32 and
//! fixed-width reads the files are checked and decoded with.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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

/// The file a build writes `path` as until it renames it into place with
/// [`replace`]: `path` with `.partial` after its name, which is how the sweeps
/// know it.
pub fn partial(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".partial");
    PathBuf::from(p)
}

/// How long [`replace`] keeps trying: longer than a reader holds a file it
/// replaces, an answer that has the old move stream mapped or a pass over a
/// heads file. A build ends with it, and no request waits for a build.
const REPLACE_WAIT: Duration = Duration::from_secs(60);

/// How often [`replace`] tries again.
const REPLACE_EVERY: Duration = Duration::from_millis(20);

/// Renames the file a build wrote, `from`, to `to`, replacing the file there.
/// On Windows a file that another handle holds cannot be replaced or moved:
/// an answer still in flight maps the old move stream, a pass still reads a
/// heads file removed as broken, an antivirus or indexing service opens a new
/// file to scan it. Such a refusal ([`held`]) is tried again every
/// [`REPLACE_EVERY`] for up to [`REPLACE_WAIT`]. Any other failure is
/// returned at once, and so is every failure on other systems, where a rename
/// replaces an open file and a refusal does not pass by waiting.
pub fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
    retried(|| std::fs::rename(from, to), held, REPLACE_WAIT)
}

/// Runs `op` until it succeeds, fails for a reason `transient` does not
/// accept, or `wait` has passed, trying again every [`REPLACE_EVERY`]; what
/// it gave last.
fn retried(
    mut op: impl FnMut() -> std::io::Result<()>,
    transient: impl Fn(&std::io::Error) -> bool,
    wait: Duration,
) -> std::io::Result<()> {
    let deadline = Instant::now() + wait;
    loop {
        match op() {
            Err(e) if transient(&e) && Instant::now() < deadline => std::thread::sleep(REPLACE_EVERY),
            result => return result,
        }
    }
}

/// Whether a rename was refused, on Windows, because another handle holds one
/// of its files: access denied, as a file open without sharing its deletion
/// or one being deleted gives, a sharing or lock violation, or a mapped file.
fn held(e: &std::io::Error) -> bool {
    // ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION, ERROR_USER_MAPPED_FILE.
    cfg!(windows)
        && (e.kind() == std::io::ErrorKind::PermissionDenied || matches!(e.raw_os_error(), Some(32 | 33 | 1224)))
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

/// CRC-32 (IEEE 802.3), as zlib and PNG compute it: every index file is
/// checked with it, each block of the position index and of the heads file as
/// it is read.
pub fn crc32(bytes: &[u8]) -> u32 {
    !crc32_update(!0, bytes)
}

/// Continues a CRC-32 over `bytes` from `state`, which starts at `!0`; the
/// CRC of all the bytes so fed is `!state`. On a processor that multiplies
/// without carries, 64 bytes at a time ([`clmul`]), and what is left eight
/// bytes at a time from tables, as a short input is and as other processors
/// take all of it.
pub fn crc32_update(state: u32, bytes: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    if bytes.len() >= clmul::LEAST && clmul::available() {
        // SAFETY: the processor has the instructions `update` is compiled for.
        let (state, rest) = unsafe { clmul::update(state, bytes) };
        return crc32_tables(state, rest);
    }
    crc32_tables(state, bytes)
}

/// [`crc32_update`] eight bytes at a time, from tables.
fn crc32_tables(state: u32, bytes: &[u8]) -> u32 {
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

/// CRC-32 by carry-less multiplication, as Linux's `crc32-pclmul` computes
/// it (Gopal et al., "Fast CRC Computation for Generic Polynomials Using
/// PCLMULQDQ Instruction", Intel, 2009): four 16-byte lanes folded 64 bytes
/// ahead at a time, folded into one, then reduced to 32 bits (Barrett). The
/// constants are powers of x modulo the reflected polynomial.
#[cfg(target_arch = "x86_64")]
mod clmul {
    use std::arch::x86_64::{
        __m128i, _mm_and_si128, _mm_clmulepi64_si128, _mm_cvtsi32_si128, _mm_extract_epi32, _mm_set_epi32,
        _mm_set_epi64x, _mm_srli_si128, _mm_xor_si128,
    };

    /// The least input folded: shorter ones go faster through the tables.
    pub const LEAST: usize = 128;

    /// x^(4*128+32) and x^(4*128-32), x^(128+32) and x^(128-32), x^64, the
    /// polynomial, and the Barrett constant, all mod P(x) and reflected.
    const K1: i64 = 0x1_5444_2bd4;
    const K2: i64 = 0x1_c6e4_1596;
    const K3: i64 = 0x1_7519_97d0;
    const K4: i64 = 0x0_ccaa_009e;
    const K5: i64 = 0x1_63cd_6124;
    const POLY: i64 = 0x1_db71_0641;
    const MU: i64 = 0x1_f701_1641;

    /// Whether this processor has what [`update`] is compiled for.
    pub fn available() -> bool {
        std::arch::is_x86_feature_detected!("pclmulqdq") && std::arch::is_x86_feature_detected!("sse4.1")
    }

    /// `state` continued over the whole 16-byte blocks of `bytes`, at least
    /// [`LEAST`] bytes, and the bytes left after them.
    #[target_feature(enable = "pclmulqdq,sse4.1")]
    pub fn update(state: u32, bytes: &[u8]) -> (u32, &[u8]) {
        let (blocks, rest) = bytes.as_chunks::<16>();
        let [a, b, c, d, more @ ..] = blocks else { return (state, bytes) };
        let mut lanes = [load(a), load(b), load(c), load(d)];
        lanes[0] = _mm_xor_si128(lanes[0], _mm_cvtsi32_si128(state as i32));
        let ahead = _mm_set_epi64x(K2, K1);
        let (quads, singles) = more.as_chunks::<4>();
        for quad in quads {
            for (lane, block) in lanes.iter_mut().zip(quad) {
                *lane = fold(*lane, load(block), ahead);
            }
        }
        let next = _mm_set_epi64x(K4, K3);
        let [a, b, c, d] = lanes;
        let mut x = fold(fold(fold(a, b, next), c, next), d, next);
        for block in singles {
            x = fold(x, load(block), next);
        }
        // 128 bits to 64, then 64 to 32.
        let low32 = _mm_set_epi32(0, 0, 0, !0);
        let x = _mm_xor_si128(_mm_clmulepi64_si128::<0x10>(x, next), _mm_srli_si128::<8>(x));
        let x = _mm_xor_si128(
            _mm_clmulepi64_si128::<0x00>(_mm_and_si128(x, low32), _mm_set_epi64x(0, K5)),
            _mm_srli_si128::<4>(x),
        );
        let poly = _mm_set_epi64x(MU, POLY);
        let t1 = _mm_clmulepi64_si128::<0x10>(_mm_and_si128(x, low32), poly);
        let t2 = _mm_clmulepi64_si128::<0x00>(_mm_and_si128(t1, low32), poly);
        (_mm_extract_epi32::<1>(_mm_xor_si128(x, t2)) as u32, rest)
    }

    /// `a` folded ahead onto `b` by the powers `k`.
    #[inline]
    #[target_feature(enable = "pclmulqdq,sse4.1")]
    fn fold(a: __m128i, b: __m128i, k: __m128i) -> __m128i {
        _mm_xor_si128(_mm_xor_si128(b, _mm_clmulepi64_si128::<0x00>(a, k)), _mm_clmulepi64_si128::<0x11>(a, k))
    }

    #[inline]
    #[target_feature(enable = "pclmulqdq,sse4.1")]
    fn load(b: &[u8; 16]) -> __m128i {
        let (lo, hi) = b.split_at(8);
        let half = |h: &[u8]| i64::from_le_bytes(h.try_into().unwrap_or_default());
        _mm_set_epi64x(half(hi), half(lo))
    }
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

    /// Long inputs, which a processor that multiplies without carries folds
    /// 64 bytes at a time, give the CRC eight bytes at a time from tables
    /// gives, and a byte at a time: every length to 1,100 bytes, whole blocks
    /// and their rests, from any state, fed whole or in two pieces.
    #[test]
    fn a_long_input_gives_the_same_crc() {
        let bytewise = |mut c: u32, bytes: &[u8]| {
            for &b in bytes {
                c ^= u32::from(b);
                for _ in 0..8 {
                    c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
                }
            }
            c
        };
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let bytes: Vec<u8> = (0..70_000).map(|_| next() as u8).collect();
        for len in (0..=1_100).chain([4_096, 65_536, 70_000]) {
            let state = next() as u32;
            let whole = crc32_update(state, &bytes[..len]);
            assert_eq!(whole, crc32_tables(state, &bytes[..len]), "{len}");
            assert_eq!(whole, bytewise(state, &bytes[..len]), "{len}");
            let cut = len / 3;
            assert_eq!(crc32_update(crc32_update(state, &bytes[..cut]), &bytes[cut..len]), whole, "{len} cut");
        }
        assert_eq!(crc32(&[b'1'; 200][..]), !bytewise(!0, &[b'1'; 200]));
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

    /// A build's file is its target's name with `.partial` after it, which
    /// the sweeps and [`db_id`] know.
    #[test]
    fn a_partial_file_is_named_after_its_target() {
        let target = Path::new("index").join("0123456789abcdef.idx");
        assert_eq!(partial(&target), Path::new("index").join("0123456789abcdef.idx.partial"));
        let name = partial(&target).file_name().unwrap().to_str().unwrap().to_owned();
        assert_eq!(db_id(&name, &[".idx", ".idx.partial"]), Some(("0123456789abcdef", 1)));
    }

    /// A refusal that passes is tried again until the rename goes through; any
    /// other failure is returned from the first try, without a wait; and a
    /// refusal that stays is returned once the wait is over.
    #[test]
    fn a_passing_refusal_is_tried_again_and_nothing_else_is() {
        let busy = || std::io::Error::from(std::io::ErrorKind::ResourceBusy);
        let transient = |e: &std::io::Error| e.kind() == std::io::ErrorKind::ResourceBusy;
        let mut tries = 0;
        let passing = retried(
            || {
                tries += 1;
                if tries < 3 { Err(busy()) } else { Ok(()) }
            },
            transient,
            REPLACE_WAIT,
        );
        assert_eq!((passing.is_ok(), tries), (true, 3));

        let mut tries = 0;
        let failed = retried(
            || {
                tries += 1;
                Err(std::io::Error::from(std::io::ErrorKind::NotFound))
            },
            transient,
            REPLACE_WAIT,
        );
        assert_eq!((failed.map_err(|e| e.kind()), tries), (Err(std::io::ErrorKind::NotFound), 1));

        let mut tries = 0;
        let staying = retried(
            || {
                tries += 1;
                Err(busy())
            },
            transient,
            Duration::from_millis(50),
        );
        assert_eq!(staying.map_err(|e| e.kind()), Err(std::io::ErrorKind::ResourceBusy));
        assert!(tries >= 1);
    }

    /// Only Windows refuses a rename for a file another handle holds, so only
    /// there is a refusal tried again: elsewhere access denied is final. A
    /// missing file is final everywhere.
    #[test]
    fn only_a_windows_refusal_for_a_held_file_is_tried_again() {
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert_eq!(held(&denied), cfg!(windows));
        assert_eq!(held(&std::io::Error::from_raw_os_error(32)), cfg!(windows));
        assert!(!held(&std::io::Error::from(std::io::ErrorKind::NotFound)));
        assert!(!held(&std::io::Error::from(std::io::ErrorKind::StorageFull)));
    }

    /// A rename replaces the file at its target; a missing file is reported
    /// at once, on every system.
    #[test]
    fn a_partial_file_replaces_its_target() {
        let dir = std::env::temp_dir().join(format!("bridge-index-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("0123456789abcdef.heads");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(partial(&target), b"new").unwrap();
        replace(&partial(&target), &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!partial(&target).exists());
        let started = Instant::now();
        let missing = replace(&partial(&target), &target).map_err(|e| e.kind());
        assert_eq!(missing, Err(std::io::ErrorKind::NotFound));
        assert!(started.elapsed() < REPLACE_WAIT, "a missing file was waited for");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// On Windows a target another handle holds without sharing its deletion
    /// is replaced once the handle is let go, as a stream an answer maps is.
    #[cfg(windows)]
    #[test]
    fn a_held_target_is_replaced_once_let_go() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = std::env::temp_dir().join(format!("bridge-index-replace-held-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("0123456789abcdef.moves");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(partial(&target), b"new").unwrap();
        let holder = std::fs::OpenOptions::new().read(true).share_mode(0).open(&target).unwrap();
        let let_go = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(holder);
        });
        replace(&partial(&target), &target).unwrap();
        let_go.join().unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&dir);
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
