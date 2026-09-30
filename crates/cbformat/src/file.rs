//! Database files read at positions, never mapped, and never held open while
//! nothing reads them (#241). A program that writes a database, as ChessBase
//! does when it saves a game, opens its files so that no other handle may
//! exist, whatever that handle shares; a reader that kept its handles would
//! make every save fail. So a file opens its handles when a read needs one,
//! and a thread of this module closes them once the file has not been read
//! for [`IDLE`]: a scan keeps its handles, an idle database holds none. A
//! read that meets a file another program holds fails at once, never waiting
//! for it with the handles of the database's other files open, which that
//! program may need next.

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
    /// The file the database was opened on: a handle opened again at `path`
    /// must be this one.
    identity: Identity,
    handles: Mutex<Handles>,
}

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
        let file = File::open(&path).map_err(|e| Error::Io(path.clone(), e))?;
        let identity = identity(&file).map_err(|e| Error::Io(path.clone(), e))?;
        let inner = Arc::new(Inner {
            path: path.into_boxed_path(),
            identity,
            handles: Mutex::new(Handles { spare: Vec::new(), last_read: Instant::now(), watched: false }),
        });
        inner.give(Arc::new(file));
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

    /// Calls `read` with a handle of the file, opened again at its path when
    /// the file holds none that a reader may take, and leaves the handle for
    /// the next reads.
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
    /// A handle for one read: a spare one, or one opened again at the path,
    /// which must still name the file the database was opened on.
    fn take(&self) -> std::io::Result<Arc<File>> {
        let spare = {
            let mut handles = lock(&self.handles);
            if cfg!(windows) { handles.spare.pop() } else { handles.spare.first().cloned() }
        };
        if let Some(handle) = spare {
            return Ok(handle);
        }
        let file = File::open(&self.path)?;
        if identity(&file)? != self.identity {
            return Err(std::io::Error::other("the file was replaced since the database was opened"));
        }
        Ok(Arc::new(file))
    }

    /// Leaves `handle` for the next reads, and has the closer close it once
    /// the file is idle; without a closer the handle closes now.
    fn give(self: &Arc<Self>, handle: Arc<File>) {
        if !closer_runs() {
            return;
        }
        let watch = {
            let mut handles = lock(&self.handles);
            handles.last_read = Instant::now();
            let room = if cfg!(windows) { SPARE_HANDLES } else { 1 };
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

/// Which file a handle has open: its volume and file index on Windows, its
/// device and inode elsewhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity(u64, u64);

#[cfg(unix)]
fn identity(file: &File) -> std::io::Result<Identity> {
    use std::os::unix::fs::MetadataExt;
    let m = file.metadata()?;
    Ok(Identity(m.dev(), m.ino()))
}

#[cfg(windows)]
fn identity(file: &File) -> std::io::Result<Identity> {
    use std::os::windows::io::{AsRawHandle, RawHandle};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(file: RawHandle, information: *mut [u32; 13]) -> i32;
    }
    // BY_HANDLE_FILE_INFORMATION: thirteen 32-bit words, of which the 8th is
    // the volume's serial number and the 12th and 13th the file index.
    let mut information = [0u32; 13];
    // SAFETY: `file` holds its handle open for the whole call, and the
    // buffer has the size and alignment of the structure the call fills.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Identity(u64::from(information[7]), u64::from(information[11]) << 32 | u64::from(information[12])))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("cbformat-file-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
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
        let f = DbFile::open(path.clone()).unwrap();
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
        let f = DbFile::open(path.clone()).unwrap();
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
        let f = DbFile::open(path.clone()).unwrap();
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

    /// Readers at the same time read what the file holds, each through a
    /// handle of its own, and leave the handles for the next reads.
    #[cfg(windows)]
    #[test]
    fn readers_at_the_same_time_read_through_handles_of_their_own() {
        let bytes: Vec<u8> = (0..1u32 << 20).map(|i| (i * 7 % 251) as u8).collect();
        let path = temp("readers", &bytes);
        let f = DbFile::open(path.clone()).unwrap();
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
        let f = DbFile::open(path.clone()).unwrap();
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
}
