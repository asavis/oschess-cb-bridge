//! Database files read at positions, never mapped, and on Windows never held
//! open between reads (#241). ChessBase opens the files of a database it
//! saves so that no other handle may exist, whatever that handle shares; a
//! reader that kept its handles would make every save fail. So a file opens
//! its handles when a read needs one, and a thread of this module closes them
//! once the file has not been read for [`IDLE`]: a scan keeps its handles, an
//! idle database holds none. A read that meets a file another program holds
//! fails at once, never waiting for it with the handles of the database's
//! other files open, which that program may need next.
//!
//! A handle opened again at a file's path must be the file the database was
//! opened on, which only an identity that no other file takes, then or
//! later, can tell: the volume and 128-bit file id of NTFS and ReFS. Where a
//! file has none, as an inode, which passes to another file once the last
//! handle of the old one closes, or a FAT directory slot, the file keeps a
//! handle for as long as it is open, as every file did before #241, and is
//! never opened again at its path. ChessBase runs on Windows only.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use crate::{Error, Result};

/// How long a file keeps its handles after its last read.
pub const IDLE: Duration = Duration::from_millis(200);

/// How often the closer looks at the files that hold handles.
const TICK: Duration = Duration::from_millis(100);

/// Most handles a file keeps for its readers on Windows; a reader beyond them
/// opens one for its read and closes it after.
const SPARE_HANDLES: usize = 64;

/// One file of a database, read at positions.
pub struct DbFile {
    inner: Arc<Inner>,
}

struct Inner {
    path: Box<Path>,
    source: Source,
    handles: Mutex<Handles>,
}

/// Where a reader that finds no spare handle gets one.
enum Source {
    /// The path, for a file whose identity no other file takes: a handle
    /// opened there must have `identity`, as `identify` reads it. Between
    /// reads, such a file holds no handle.
    Path { identity: Identity, identify: Identify },
    /// A handle kept for as long as the file is open, which each reader opens
    /// again on Windows and the readers share elsewhere: for a file without
    /// such an identity.
    Kept(Arc<File>),
}

/// Reads the identity of the file a handle has open, when it has one that no
/// other file takes, then or later.
type Identify = fn(&File) -> std::io::Result<Option<Identity>>;

/// Which file a handle has open: its volume and file id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity(u64, u128);

/// The handles of a file that no read uses now, and when it was last read.
struct Handles {
    /// On Windows a handle reads one read at a time, so each reader takes one
    /// of its own and leaves it here after; elsewhere reads at positions on
    /// one handle run at the same time, and the readers share the first.
    spare: Vec<Arc<File>>,
    last_read: Instant,
    /// Whether the closer watches the file: from the first handle left here
    /// until the closer has closed them all.
    watched: bool,
}

impl DbFile {
    pub fn open(path: PathBuf) -> Result<DbFile> {
        DbFile::open_with(path, lasting_identity)
    }

    /// [`DbFile::open`], reading the file's identity with `identify`.
    fn open_with(path: PathBuf, identify: Identify) -> Result<DbFile> {
        let file = File::open(&path).map_err(|e| Error::Io(path.clone(), e))?;
        let identity = identify(&file).map_err(|e| Error::Io(path.clone(), e))?;
        let (source, first) = match identity {
            Some(identity) => (Source::Path { identity, identify }, Some(file)),
            None => (Source::Kept(Arc::new(file)), None),
        };
        let inner = Arc::new(Inner {
            path: path.into_boxed_path(),
            source,
            handles: Mutex::new(Handles { spare: Vec::new(), last_read: Instant::now(), watched: false }),
        });
        if let Some(file) = first {
            inner.give(Arc::new(file));
        }
        Ok(DbFile { inner })
    }

    /// [`DbFile::open`] for a file a database may lack: `None` when there is
    /// none at `path`.
    pub(crate) fn open_optional(path: PathBuf) -> Result<Option<DbFile>> {
        match DbFile::open(path) {
            Ok(f) => Ok(Some(f)),
            Err(Error::Io(_, e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The file's size in bytes now.
    pub fn size(&self) -> Result<u64> {
        self.with_handle(|file| file.metadata().map(|m| m.len()))
            .map_err(|e| Error::Io(self.inner.path.to_path_buf(), e))
    }

    /// Fills `buf` from `offset`; a short file is an error.
    pub fn read_into(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.with_handle(|file| read_exact_at(file, buf, offset))
            .map_err(|e| Error::Io(self.inner.path.to_path_buf(), e))
    }

    /// Calls `read` with a handle of the file, opened again when the file
    /// holds none that a reader may take, and leaves the handle for the next
    /// reads.
    fn with_handle<T>(&self, read: impl FnOnce(&File) -> std::io::Result<T>) -> std::io::Result<T> {
        let handle = self.inner.take()?;
        let result = read(&handle);
        self.inner.give(handle);
        result
    }

    /// `len` bytes from `offset`. Callers bound `len` first.
    pub(crate) fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0; len];
        self.read_into(offset, &mut buf)?;
        Ok(buf)
    }

    /// The handles the file holds that no read uses now.
    #[cfg(test)]
    fn spare_handles(&self) -> usize {
        lock(&self.inner.handles).spare.len()
    }
}

impl Inner {
    /// A handle for one read: a spare one, else one opened again, at the path
    /// when it must still name the file the database was opened on, else from
    /// the kept handle.
    fn take(&self) -> std::io::Result<Arc<File>> {
        let spare = {
            let mut handles = lock(&self.handles);
            if cfg!(windows) { handles.spare.pop() } else { handles.spare.first().cloned() }
        };
        if let Some(handle) = spare {
            return Ok(handle);
        }
        match &self.source {
            Source::Path { identity, identify } => {
                let file = File::open(&self.path)?;
                if identify(&file)?.as_ref() != Some(identity) {
                    return Err(std::io::Error::other("the file was replaced since the database was opened"));
                }
                Ok(Arc::new(file))
            }
            // A handle of its own when one opens, else the kept one, which
            // reads one read at a time.
            #[cfg(windows)]
            Source::Kept(kept) => Ok(reopen(kept).map_or_else(|_| Arc::clone(kept), Arc::new)),
            #[cfg(not(windows))]
            Source::Kept(kept) => Ok(Arc::clone(kept)),
        }
    }

    /// Leaves `handle` for the next reads, and has the closer close it once
    /// the file is idle; without a closer the handle closes now. Elsewhere
    /// than on Windows, a kept handle stays with the file, and readers leave
    /// nothing.
    fn give(self: &Arc<Self>, handle: Arc<File>) {
        let room = match self.source {
            _ if cfg!(windows) => SPARE_HANDLES,
            Source::Path { .. } => 1,
            Source::Kept(_) => 0,
        };
        if room == 0 || !closer_runs() {
            return;
        }
        let watch = {
            let mut handles = lock(&self.handles);
            handles.last_read = Instant::now();
            if handles.spare.len() < room {
                handles.spare.push(handle);
            }
            let watch = !handles.watched && !handles.spare.is_empty();
            handles.watched |= watch;
            watch
        };
        if watch {
            let mut watched = lock(&CLOSER.watched);
            watched.push(Arc::downgrade(self));
            CLOSER.wake.notify_one();
        }
    }

    /// Closes the handles no read uses when the file has not been read for
    /// `idle` at `now`; whether the file still holds some, and stays watched.
    fn close_idle(&self, now: Instant, idle: Duration) -> bool {
        let mut handles = lock(&self.handles);
        if now.saturating_duration_since(handles.last_read) >= idle {
            handles.spare.clear();
        }
        handles.watched = !handles.spare.is_empty();
        handles.watched
    }
}

/// The files that hold handles, which the closer's thread looks at every
/// [`TICK`] while there are any, and waits for while there are none.
struct Closer {
    watched: Mutex<Vec<Weak<Inner>>>,
    wake: Condvar,
}

static CLOSER: LazyLock<Closer> = LazyLock::new(|| Closer { watched: Mutex::new(Vec::new()), wake: Condvar::new() });

/// Whether the closer's thread runs, started at the first call; a file never
/// keeps a handle that no thread would close.
fn closer_runs() -> bool {
    static STARTED: OnceLock<bool> = OnceLock::new();
    *STARTED.get_or_init(|| std::thread::Builder::new().name("dbfile-closer".into()).spawn(close_idle_files).is_ok())
}

fn close_idle_files() {
    loop {
        {
            let mut watched = lock(&CLOSER.watched);
            while watched.is_empty() {
                watched = CLOSER.wake.wait(watched).unwrap_or_else(|e| e.into_inner());
            }
        }
        std::thread::sleep(TICK);
        let now = Instant::now();
        lock(&CLOSER.watched).retain(|file| file.upgrade().is_some_and(|file| file.close_idle(now, IDLE)));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The volume and 128-bit id of the file `file` has open, on NTFS and ReFS,
/// whose ids no other file of the volume takes; `None` on another file system
/// or when the file system gives no id. FAT, for one, numbers a file by its
/// directory slot, which a file made in its place takes over.
#[cfg(windows)]
fn lasting_identity(file: &File) -> std::io::Result<Option<Identity>> {
    use std::os::windows::io::{AsRawHandle, RawHandle};
    /// FILE_ID_INFO.
    #[repr(C)]
    struct FileIdInfo {
        volume: u64,
        id: [u8; 16],
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetVolumeInformationByHandleW(
            file: RawHandle,
            volume_name: *mut u16,
            volume_name_size: u32,
            serial: *mut u32,
            component_length: *mut u32,
            flags: *mut u32,
            file_system_name: *mut u16,
            file_system_name_size: u32,
        ) -> i32;
        fn GetFileInformationByHandleEx(file: RawHandle, class: i32, information: *mut FileIdInfo, size: u32) -> i32;
    }
    /// FILE_INFO_BY_HANDLE_CLASS's FileIdInfo.
    const FILE_ID_INFO: i32 = 18;
    // MAX_PATH + 1, the most the call writes.
    let mut name = [0u16; 261];
    let null = std::ptr::null_mut();
    // SAFETY: `file` holds its handle open for the whole call, the name
    // buffer is as long as its size says, and the call may leave out every
    // other part of the answer, given as null.
    let named = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            null,
            0,
            null.cast(),
            null.cast(),
            null.cast(),
            name.as_mut_ptr(),
            261,
        )
    };
    if named == 0 {
        return Ok(None);
    }
    let name = String::from_utf16_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(name.len())]);
    if !name.eq_ignore_ascii_case("NTFS") && !name.eq_ignore_ascii_case("ReFS") {
        return Ok(None);
    }
    let mut info = FileIdInfo { volume: 0, id: [0; 16] };
    // SAFETY: as above, and `info` has the size and layout of the
    // structure the call fills.
    let filled = unsafe {
        GetFileInformationByHandleEx(file.as_raw_handle(), FILE_ID_INFO, &mut info, size_of::<FileIdInfo>() as u32)
    };
    if filled == 0 || !names_a_file(&info.id) {
        return Ok(None);
    }
    Ok(Some(Identity(info.volume, u128::from_le_bytes(info.id))))
}

/// Whether a 128-bit file id names a file: a file system without ids gives
/// all zeros or all ones, in 64 bits or 128.
#[cfg(windows)]
fn names_a_file(id: &[u8; 16]) -> bool {
    let unset = |half: &[u8]| half.iter().all(|&b| b == 0) || half.iter().all(|&b| b == 0xff);
    !(unset(&id[..8]) && unset(&id[8..]))
}

/// `None`: an inode passes to another file once the last handle of the old
/// one closes, so a file here keeps a handle for as long as it is open.
#[cfg(not(windows))]
fn lasting_identity(_: &File) -> std::io::Result<Option<Identity>> {
    Ok(None)
}

/// The stem of the database `path` names: `path` without its extension when
/// that is `main`, in any case, else `path` itself, taken as a bare stem.
pub(crate) fn stem(path: &Path, main: &str) -> PathBuf {
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case(main)) {
        path.with_extension("")
    } else {
        path.to_path_buf()
    }
}

/// The path `stem` takes with `ext` appended.
pub(crate) fn with_extension(stem: &Path, ext: &str) -> PathBuf {
    let mut s = stem.as_os_str().to_owned();
    s.push(ext);
    PathBuf::from(s)
}

/// The paths `stem` takes with each of `extensions` appended.
pub(crate) fn with_extensions<'a>(stem: &Path, extensions: impl IntoIterator<Item = &'a &'a str>) -> Vec<PathBuf> {
    extensions.into_iter().map(|ext| with_extension(stem, ext)).collect()
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
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

/// A new handle, for reading, of the file `file` has open: the same file even
/// when its path has since come to name another.
#[cfg(windows)]
fn reopen(file: &File) -> std::io::Result<File> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn ReOpenFile(original: RawHandle, access: u32, share: u32, flags: u32) -> RawHandle;
    }
    const GENERIC_READ: u32 = 0x8000_0000;
    // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, as `File::open`
    // shares a file.
    const SHARE_ALL: u32 = 0x7;
    // SAFETY: `file` holds its handle open for the whole call, and no flags
    // are asked for, so the new handle reads synchronously as `file`'s does.
    let handle = unsafe { ReOpenFile(file.as_raw_handle(), GENERIC_READ, SHARE_ALL, 0) };
    if handle as isize == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the handle was just opened, and the file made from it is its
    // only owner.
    Ok(unsafe { File::from_raw_handle(handle) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("cbformat-file-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// The device and inode of the file a handle has open: an identity no
    /// other file takes while some handle of the file stays open, as in the
    /// tests that open a file again at its path on Linux, which rename the
    /// file they replace and so keep it.
    #[cfg(unix)]
    fn inode(file: &File) -> std::io::Result<Option<Identity>> {
        use std::os::unix::fs::MetadataExt;
        let m = file.metadata()?;
        Ok(Some(Identity(m.dev(), u128::from(m.ino()))))
    }

    /// `path` opened as a file with a lasting identity, which holds no handle
    /// between reads: every file on Windows, where the test folder is on
    /// NTFS; on Linux, by [`inode`].
    fn open_by_path(path: &Path) -> DbFile {
        #[cfg(windows)]
        let identify: Identify = lasting_identity;
        #[cfg(unix)]
        let identify: Identify = inode;
        let f = DbFile::open_with(path.to_path_buf(), identify).unwrap();
        assert!(matches!(f.inner.source, Source::Path { .. }), "the file has no lasting identity");
        f
    }

    /// Closes `f`'s handles as the closer does once the file has been idle.
    fn close_now(f: &DbFile) {
        let last_read = lock(&f.inner.handles).last_read;
        f.inner.close_idle(last_read + IDLE, IDLE);
    }

    /// A file keeps its handles while it is read, holds none once idle, and
    /// opens one again for the next read (#241).
    #[test]
    fn an_idle_file_holds_no_handle_and_opens_one_again_to_read() {
        let path = temp("idle", b"0123456789");
        let f = open_by_path(&path);
        let mut buf = [0u8; 4];
        f.read_into(3, &mut buf).unwrap();
        assert_eq!(f.spare_handles(), 1, "the read left its handle");
        let last_read = lock(&f.inner.handles).last_read;
        assert!(f.inner.close_idle(last_read + IDLE / 2, IDLE), "a file read just now keeps its handle");
        assert_eq!(f.spare_handles(), 1);
        close_now(&f);
        assert_eq!(f.spare_handles(), 0, "an idle file holds no handle");
        f.read_into(6, &mut buf).unwrap();
        assert_eq!(&buf, b"6789");
        assert_eq!(f.size().unwrap(), 10);
        drop(f);
        std::fs::remove_file(&path).unwrap();
    }

    /// The closer's own thread closes the handles of a file nobody reads.
    #[test]
    fn the_closer_closes_the_handles_of_an_idle_file() {
        let path = temp("closer", b"0123456789");
        let f = open_by_path(&path);
        f.read_into(0, &mut [0u8; 10]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(300);
        while f.spare_handles() > 0 {
            assert!(Instant::now() < deadline, "the closer never closed the handles");
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(f);
        std::fs::remove_file(&path).unwrap();
    }

    /// A file renamed while it is open, with another written at its path, is
    /// read through the handle still open, and once that closed is refused:
    /// never is the other file read as the database's own.
    #[test]
    fn a_file_replaced_at_its_path_is_never_read_as_the_open_one() {
        let path = temp("renamed", b"first");
        let moved = path.with_extension("moved");
        let f = open_by_path(&path);
        std::fs::rename(&path, &moved).unwrap();
        std::fs::write(&path, b"other").unwrap();
        let mut buf = [0u8; 5];
        // The closer may have closed the handle of the open by now.
        match f.read_into(0, &mut buf) {
            Ok(()) => assert_eq!(&buf, b"first"),
            Err(e) => assert!(e.to_string().contains("replaced"), "{e}"),
        }
        close_now(&f);
        let refused = f.read_into(0, &mut buf).unwrap_err();
        assert!(refused.to_string().contains("replaced"), "{refused}");
        drop(f);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(&moved).unwrap();
    }

    /// Where a file has no lasting identity, it keeps its handle and reads the
    /// file it was opened on, whatever comes to its path: a file made in its
    /// place, even one that took its inode, or a FIFO, which a reader opening
    /// the path would wait on for a writer.
    #[cfg(unix)]
    #[test]
    fn a_file_without_a_lasting_identity_keeps_its_handle_and_its_file() {
        let path = temp("kept", b"first");
        let f = DbFile::open(path.clone()).unwrap();
        assert!(matches!(f.inner.source, Source::Kept(_)));
        let mut buf = [0u8; 5];
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"other").unwrap();
        f.read_into(0, &mut buf).unwrap();
        assert_eq!(&buf, b"first");
        std::fs::remove_file(&path).unwrap();
        let made = std::process::Command::new("mkfifo").arg(&path).status().unwrap();
        assert!(made.success());
        f.read_into(0, &mut buf).unwrap();
        assert_eq!(&buf, b"first");
        assert_eq!(f.size().unwrap(), 5);
        assert_eq!(f.spare_handles(), 0, "readers share the kept handle");
        drop(f);
        std::fs::remove_file(&path).unwrap();
    }

    /// Readers at the same time read what the file holds, each through a
    /// handle of its own, and leave the handles for the next reads.
    #[cfg(windows)]
    #[test]
    fn readers_at_the_same_time_read_through_handles_of_their_own() {
        let bytes: Vec<u8> = (0..1u32 << 20).map(|i| (i * 7 % 251) as u8).collect();
        let path = temp("readers", &bytes);
        let f = open_by_path(&path);
        let most = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| {
            for t in 0..8u64 {
                let (f, bytes, most) = (&f, &bytes, &most);
                s.spawn(move || {
                    for i in 0..200u64 {
                        let at = (t * 131_071 + i * 4_099) % (bytes.len() as u64 - 4096);
                        let mut buf = [0u8; 4096];
                        f.read_into(at, &mut buf).unwrap();
                        assert_eq!(&buf[..], &bytes[at as usize..at as usize + 4096]);
                        most.fetch_max(f.spare_handles(), std::sync::atomic::Ordering::Relaxed);
                    }
                });
            }
        });
        // The closer may close idle handles meanwhile, never more than 8 open.
        let most = most.into_inner();
        assert!((1..=8).contains(&most), "{most} spare handles");
        drop(f);
        std::fs::remove_file(&path).unwrap();
    }

    /// Whether `path` opens for a writer that shares it with nobody, as
    /// ChessBase opens a database to save a game.
    #[cfg(windows)]
    fn opens_alone(path: &Path) -> bool {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().read(true).write(true).share_mode(0).open(path).is_ok()
    }

    /// While a read runs, a writer that shares the file with nobody cannot
    /// open it; once the file is idle it can (#241).
    #[cfg(windows)]
    #[test]
    fn an_idle_file_lets_a_writer_open_it_alone() {
        let path = temp("alone", b"0123456789");
        let f = open_by_path(&path);
        f.with_handle(|_| {
            assert!(!opens_alone(&path), "a read holds a handle");
            Ok(())
        })
        .unwrap();
        close_now(&f);
        assert!(opens_alone(&path), "an idle file holds no handle");
        drop(f);
        std::fs::remove_file(&path).unwrap();
    }

    /// A file another program holds so that it cannot be opened is refused
    /// at once, and opens once that program lets it go.
    #[cfg(windows)]
    #[test]
    fn a_file_held_by_another_program_is_refused_until_it_is_let_go() {
        use std::os::windows::fs::OpenOptionsExt;
        let path = temp("held", b"0123456789");
        let holder = std::fs::OpenOptions::new().read(true).write(true).share_mode(0).open(&path).unwrap();
        assert!(DbFile::open(path.clone()).is_err(), "a held file opened");
        drop(holder);
        assert!(DbFile::open(path.clone()).is_ok());
        std::fs::remove_file(&path).unwrap();
    }

    /// The ids a file system without ids gives name no file.
    #[cfg(windows)]
    #[test]
    fn unset_file_ids_name_no_file() {
        let id = |low: u64, high: u64| {
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&low.to_le_bytes());
            id[8..].copy_from_slice(&high.to_le_bytes());
            id
        };
        for unset in [id(0, 0), id(u64::MAX, 0), id(u64::MAX, u64::MAX), id(0, u64::MAX)] {
            assert!(!names_a_file(&unset), "{unset:?}");
        }
        for set in [id(0x0005_0000_0000_1234, 0), id(1, 0), id(0, 1), id(u64::MAX, 1)] {
            assert!(names_a_file(&set), "{set:?}");
        }
    }
}
