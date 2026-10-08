//! The bridge's log (#117): a line, with the UTC time, for each start and for
//! each thing that went wrong. A line goes to standard error, and once
//! [`open`] has named the data folder, to `bridge.log` there as well: the
//! Windows app has no standard error, so the file is where its lines stay.
//!
//! A line names a database by its API id (`catalog::id_of`), never by its
//! name, title or path, and carries no game or query, as `cbtool profile`
//! does (#83), so that a user can attach the file to an issue as it is. A file
//! of the bridge's own is named by its fixed name, a database's file by its
//! extension ([`error`]).

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::sync::lock;

/// The log's name in the data folder.
pub const FILE_NAME: &str = "bridge.log";

/// Where [`open`] moves a log over [`CAP`], replacing the one moved before.
pub const OLD_FILE_NAME: &str = "bridge.log.1";

/// The size over which [`open`] starts a new log: 1 MiB.
pub const CAP: u64 = 1 << 20;

/// The log file, once open.
static FILE: Mutex<Option<File>> = Mutex::new(None);

/// Writes a line to the log: `log!("indexing database {id} failed: {why}")`.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::log::write(format_args!($($arg)*))
    };
}

/// Sends the lines from now on to `bridge.log` in `dir` as well, making the
/// folder when it is missing. A log over [`CAP`] is moved aside to
/// `bridge.log.1` first. When the file cannot be opened, lines go to standard
/// error alone.
pub fn open(dir: &Path) {
    let mut file = lock(&FILE);
    // The file open before, if any, is closed first, so it can be moved aside.
    *file = None;
    match open_file(dir) {
        Ok(opened) => *file = Some(opened),
        Err(e) => {
            drop(file);
            write(format_args!("{FILE_NAME} cannot be opened: {e}"));
        }
    }
}

/// `bridge.log` in `dir`, opened to append, after moving it aside when it is
/// over [`CAP`]. A log that cannot be moved is appended to all the same.
fn open_file(dir: &Path) -> std::io::Result<File> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(FILE_NAME);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > CAP) {
        let _ = std::fs::rename(&path, dir.join(OLD_FILE_NAME));
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// Writes `message` as a line, after the time: to the log file once open, and
/// to standard error. Called by [`log!`].
pub fn write(message: fmt::Arguments<'_>) {
    let line = format!("{} {message}\n", timestamp(now()));
    put(&FILE, &line, &mut std::io::stderr());
}

/// Appends `line` to the file `file` holds, if any, and then writes it to
/// `stderr`. Neither can fail the caller: standard error is optional, full or
/// closed at times, and a line it refuses still reaches the file. The
/// standard library's printing macros would panic there instead, and a start
/// would stop at its first line.
fn put(file: &Mutex<Option<File>>, line: &str, stderr: &mut dyn Write) {
    append(file, line);
    // `Stderr` writes the whole line under its own lock.
    let _ = stderr.write_all(line.as_bytes());
}

/// Appends `line` whole to the file `file` holds, if any: under the lock, so
/// the lines of threads at the same time never mix. A line the file refuses
/// is lost from the file alone.
fn append(file: &Mutex<Option<File>>, line: &str) {
    if let Some(file) = lock(file).as_mut() {
        let _ = file.write_all(line.as_bytes());
    }
}

/// `e` as a line may carry it: an I/O error names its file by the extension
/// alone, never by its path, which holds the user's name and the database's.
pub fn error(e: &cbformat::Error) -> String {
    match e {
        cbformat::Error::Io(path, io) => match extension(path) {
            Some(ext) => format!(".{ext}: {io}"),
            None => io.to_string(),
        },
        other => other.to_string(),
    }
}

/// The extension of `path` when it looks like one of a database's files or
/// the bridge's own: a few ASCII letters and digits. Anything else could be
/// part of a folder's name.
fn extension(path: &Path) -> Option<&str> {
    let ext = path.extension()?.to_str()?;
    ((1..=8).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(ext)
}

/// Seconds since 1970-01-01T00:00:00Z, negative for a clock set before it.
fn now() -> i64 {
    seconds(SystemTime::now())
}

/// Seconds from 1970-01-01T00:00:00Z to `time`, negative before it.
fn seconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => i64::try_from(e.duration().as_secs()).map_or(i64::MIN, |s| -s),
    }
}

/// `time` in RFC 3339 to the second, as the database list writes file times
/// (#298): `None` for a time outside the years 0000 to 9999, which RFC 3339's
/// four-digit year cannot hold (a Windows file time reaches the year 30827).
pub fn rfc3339(time: SystemTime) -> Option<String> {
    rfc3339_of(seconds(time))
}

/// [`rfc3339`] of the time `secs` seconds after 1970-01-01T00:00:00Z.
fn rfc3339_of(secs: i64) -> Option<String> {
    let (year, _, _) = civil_from_days(secs.div_euclid(86_400));
    (0..=9999).contains(&year).then(|| timestamp(secs))
}

/// `secs` seconds after 1970-01-01T00:00:00Z, in ISO 8601 to the second:
/// `2026-09-27T15:04:05Z`.
fn timestamp(secs: i64) -> String {
    let (days, time) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", time / 3600, time / 60 % 60, time % 60)
}

/// The date `days` days after 1970-01-01 in the proleptic Gregorian calendar,
/// by Howard Hinnant's `civil_from_days`: a year counted from March, so that
/// the leap day ends it, in eras of 400 years.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 { month_from_march + 3 } else { month_from_march - 9 };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

/// For the tests that open the log: the log is one per process, and the
/// tests of a binary run beside each other.
#[cfg(test)]
pub(crate) mod testing {
    use std::sync::{Mutex, MutexGuard};

    use crate::sync::lock;

    /// The log held by one test, which closes it when dropped.
    pub(crate) struct Held(#[allow(dead_code)] MutexGuard<'static, ()>);

    /// Waits until no other test holds the log, then holds it. Every test that
    /// opens the log, itself or through `start::prepare`, holds it first.
    pub(crate) fn hold() -> Held {
        static ONE: Mutex<()> = Mutex::new(());
        Held(lock(&ONE))
    }

    impl Drop for Held {
        fn drop(&mut self) {
            *lock(&super::FILE) = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bridge-log-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn times_are_utc_in_iso_8601_to_the_second() {
        for (secs, text) in [
            (0, "1970-01-01T00:00:00Z"),
            (-1, "1969-12-31T23:59:59Z"),
            (951_868_799, "2000-02-29T23:59:59Z"),
            (1_790_521_445, "2026-09-27T15:04:05Z"),
            (4_107_542_400, "2100-03-01T00:00:00Z"),
        ] {
            assert_eq!(timestamp(secs), text, "{secs}");
        }
        let now = timestamp(now());
        assert_eq!((now.len(), &now[4..5], &now[10..11], &now[19..]), (20, "-", "T", "Z"), "{now}");
        let at = |secs: i64| match u64::try_from(secs) {
            Ok(s) => UNIX_EPOCH + std::time::Duration::from_secs(s),
            Err(_) => UNIX_EPOCH - std::time::Duration::from_secs(secs.unsigned_abs()),
        };
        for secs in [0, -1, -86_401, 1_790_521_445] {
            assert_eq!(seconds(at(secs)), secs);
        }
        assert_eq!(rfc3339(at(1_790_521_445)).as_deref(), Some("2026-09-27T15:04:05Z"));
        // RFC 3339 holds the years 0000 to 9999 only. Their bounds are checked
        // in seconds: a Windows `SystemTime` cannot hold a time before 1601
        // (#302).
        for (secs, text) in [
            (253_402_300_799, Some("9999-12-31T23:59:59Z")),
            (253_402_300_800, None),
            (-62_167_219_200, Some("0000-01-01T00:00:00Z")),
            (-62_167_219_201, None),
        ] {
            assert_eq!(rfc3339_of(secs).as_deref(), text, "{secs}");
        }
    }

    #[test]
    fn a_log_over_the_cap_is_moved_aside_at_the_start() {
        let dir = folder("cap");
        std::fs::create_dir_all(&dir).unwrap();
        let old = vec![b'x'; CAP as usize + 1];
        std::fs::write(dir.join(FILE_NAME), &old).unwrap();
        std::fs::write(dir.join(OLD_FILE_NAME), "the log moved aside before\n").unwrap();
        let file = Mutex::new(Some(open_file(&dir).unwrap()));
        append(&file, "a line\n");
        drop(file);
        assert_eq!(std::fs::read(dir.join(OLD_FILE_NAME)).unwrap(), old, "the one before is replaced");
        assert_eq!(std::fs::read_to_string(dir.join(FILE_NAME)).unwrap(), "a line\n");

        // A log at the cap or under it is appended to.
        let file = Mutex::new(Some(open_file(&dir).unwrap()));
        append(&file, "another\n");
        drop(file);
        assert_eq!(std::fs::read_to_string(dir.join(FILE_NAME)).unwrap(), "a line\nanother\n");
        assert_eq!(std::fs::read(dir.join(OLD_FILE_NAME)).unwrap().len(), old.len());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lines_of_threads_at_the_same_time_stay_whole() {
        let dir = folder("whole");
        let file = Mutex::new(Some(open_file(&dir).unwrap()));
        let lines: Vec<String> =
            (0..8).map(|t| format!("{}\n", char::from(b'a' + t).to_string().repeat(9000))).collect();
        std::thread::scope(|s| {
            for line in &lines {
                let file = &file;
                s.spawn(move || (0..50).for_each(|_| append(file, line)));
            }
        });
        drop(file);
        let text = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        assert_eq!(text.lines().count(), 8 * 50);
        assert!(text.split_inclusive('\n').all(|l| lines.iter().any(|w| w == l)), "a line was cut");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_line_reaches_the_file_when_standard_error_fails() {
        /// Standard error on a full disk or a closed pipe.
        struct Refusing(usize);
        impl Write for Refusing {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                self.0 += 1;
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            }
        }
        let dir = folder("stderr");
        let file = Mutex::new(Some(open_file(&dir).unwrap()));
        let mut stderr = Refusing(0);
        put(&file, "a line\n", &mut stderr);
        put(&file, "another\n", &mut stderr);
        drop(file);
        assert_eq!(stderr.0, 2, "each line was offered to standard error");
        assert_eq!(std::fs::read_to_string(dir.join(FILE_NAME)).unwrap(), "a line\nanother\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_line_has_the_time_and_goes_to_the_open_log() {
        let held = testing::hold();
        let dir = folder("open");
        open(&dir);
        crate::log!("database {} failed: {}", "0123456789abcdef", 7);
        // Other tests may log beside this one.
        let text = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        let line = text.lines().find(|l| l.contains("0123456789abcdef")).unwrap();
        let (time, message) = line.split_once(' ').unwrap();
        assert_eq!(message, "database 0123456789abcdef failed: 7");
        assert!(time.len() == 20 && time.ends_with('Z'), "{time}");
        drop(held);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_io_error_names_its_file_by_the_extension_alone() {
        let io =
            |path: &str| cbformat::Error::Io(std::path::PathBuf::from(path), std::io::Error::other("the disk is gone"));
        let home = std::env::temp_dir().join("Jane Doe");
        let db = home.join("My Games.2cbg");
        assert_eq!(error(&io(db.to_str().unwrap())), ".2cbg: the disk is gone");
        assert_eq!(error(&io("C:\\Users\\J. R. Smith")), "the disk is gone");
        assert_eq!(error(&io("C:\\Users\\Jane\\Documents\\ChessBase")), "the disk is gone");
        assert_eq!(error(&cbformat::Error::Format("record at 0x10: bad".into())), "format error: record at 0x10: bad");
    }
}
