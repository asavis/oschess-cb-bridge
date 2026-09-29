//! What the readers share: a file of fixed-size records numbered from 1, as a
//! database keeps its game headers and a PGN file's index its games, and the
//! span of a move or annotation file that a run of those records points at.
//! Each rule on the untrusted counts and offsets of these files is written
//! here once, for every format.

use std::ops::{Range, RangeInclusive};
use std::path::PathBuf;

use crate::file::DbFile;
use crate::game::MAX_BATCH_RECORDS;
use crate::{Error, Result};

/// A file of `N`-byte records numbered from 1, after a header of its own.
pub(crate) struct RecordFile<const N: usize> {
    file: DbFile,
    /// Where record 1 starts.
    first_at: u64,
    count: u32,
}

impl<const N: usize> RecordFile<N> {
    /// Opens `path`, a header file whose own header is as long as a record:
    /// its size must be a whole number of records, one at least. `name`
    /// names the file in errors. The record count is taken now.
    pub(crate) fn open(path: PathBuf, name: &str) -> Result<Self> {
        let file = DbFile::open(path)?;
        let len = file.len()?;
        let size = N as u64;
        if len < size || !len.is_multiple_of(size) {
            return Err(Error::Format(format!("{name} size {len} is not a multiple of {N}")));
        }
        let count = u32::try_from(len / size - 1)
            .map_err(|_| Error::Format(format!("{name} size {len} holds more than 2^32 records")))?;
        Ok(RecordFile { file, first_at: size, count })
    }

    /// `file`, whose `count` records start at `first_at`; the caller has
    /// checked that the file is long enough for them.
    pub(crate) fn new(file: DbFile, first_at: u64, count: u32) -> Self {
        RecordFile { file, first_at, count }
    }

    pub(crate) fn file(&self) -> &DbFile {
        &self.file
    }

    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    /// Where record `id` starts; `id` is 1 at least.
    fn at(&self, id: u32) -> u64 {
        self.first_at + u64::from(id - 1) * N as u64
    }

    /// The bytes of 1-based record `id`.
    pub(crate) fn record(&self, id: u32) -> Result<[u8; N]> {
        if id == 0 || id > self.count {
            return Err(Error::NoSuchGame(id));
        }
        let mut b = [0; N];
        self.file.read_into(self.at(id), &mut b)?;
        Ok(b)
    }

    /// Reads the records from `first` into `buf`, as many as it holds up to
    /// the last record, in one read, and returns how many it read: none when
    /// `first` is 0 or past the end. It allocates nothing.
    pub(crate) fn read_records(&self, first: u32, buf: &mut [u8]) -> Result<u32> {
        if first == 0 || first > self.count {
            return Ok(0);
        }
        let fits = u32::try_from(buf.len() / N).unwrap_or(u32::MAX);
        let count = fits.min(self.count - first + 1);
        self.file.read_into(self.at(first), &mut buf[..count as usize * N])?;
        Ok(count)
    }

    /// `first..=last` within the file and at most [`MAX_BATCH_RECORDS`]
    /// long; empty when `first > last`, with `first` at least 1.
    pub(crate) fn clamp(&self, first: u32, last: u32) -> (u32, u32) {
        let first = first.max(1);
        (first, last.min(self.count).min(first.saturating_add(MAX_BATCH_RECORDS - 1)))
    }

    /// Records `first..=last`, clamped as [`RecordFile::clamp`] clamps them,
    /// in one read, each as `make` makes it from its id and bytes.
    pub(crate) fn records<R>(&self, first: u32, last: u32, make: impl Fn(u32, &[u8; N]) -> R) -> Result<Vec<R>> {
        let run = self.run(first, last, false)?;
        Ok(run.records().iter().zip(run.ids()).map(|(b, id)| make(id, b)).collect())
    }

    /// Records `first..=last`, clamped as [`RecordFile::clamp`] clamps them,
    /// in one read, with the record after them when `next` asks for it and
    /// there is one.
    pub(crate) fn run(&self, first: u32, last: u32, next: bool) -> Result<Run<N>> {
        let (first, last) = self.clamp(first, last);
        if first > last {
            return Ok(Run { first, last, next: false, bytes: Vec::new() });
        }
        let upto = if next { last.saturating_add(1).min(self.count) } else { last };
        let count = (upto - first + 1) as usize;
        debug_assert!(count <= MAX_BATCH_RECORDS as usize + 1);
        let bytes = self.file.read(self.at(first), count * N)?;
        Ok(Run { first, last, next: upto > last, bytes })
    }
}

/// Records read together by [`RecordFile::run`].
pub(crate) struct Run<const N: usize> {
    first: u32,
    last: u32,
    /// Whether `bytes` end with the record after the run.
    next: bool,
    bytes: Vec<u8>,
}

impl<const N: usize> Run<N> {
    /// The ids of the run; the record after it is not one of them.
    pub(crate) fn ids(&self) -> RangeInclusive<u32> {
        self.first..=self.last
    }

    /// The records of the run, in id order.
    pub(crate) fn records(&self) -> &[[u8; N]] {
        let all = self.bytes.as_chunks::<N>().0;
        &all[..all.len().saturating_sub(usize::from(self.next))]
    }

    /// The record after the run, when it was read.
    pub(crate) fn next(&self) -> Option<&[u8; N]> {
        self.bytes.as_chunks::<N>().0.last().filter(|_| self.next)
    }

    /// The bytes of record `id` of the run; `None` for an id outside it.
    pub(crate) fn get(&self, id: u32) -> Option<[u8; N]> {
        let i = id.checked_sub(self.first)?;
        self.records().get(usize::try_from(i).ok()?).copied()
    }
}

/// The span of a move or annotation file `file_len` bytes long that holds a
/// run of records, which the file keeps back to back in id order. `offsets`
/// are where the run's records start; one below `min`, the file's header,
/// names no record and is left out, and without any other there is no span.
/// `next` is where the record after the run starts, `None` at the end of the
/// database. The span runs from the lowest offset to `next`, to the end of
/// the file at the end of the database, and to the highest offset at least,
/// within the file; `None` when that is empty.
///
/// A `next` below `min` names no record either, and says nothing of where the
/// run's last record ends. The span then ends where the highest record starts,
/// so that record alone is read on its own: ending it at the end of the file
/// instead could make it gigabytes long, and a span too long for its buffer
/// is not read at all, which would leave every record of the run to be read
/// on its own.
pub(crate) fn span(
    offsets: impl IntoIterator<Item = u64>,
    next: Option<u64>,
    file_len: u64,
    min: u64,
) -> Option<Range<u64>> {
    let mut named = offsets.into_iter().filter(|&o| o >= min);
    let first = named.next()?;
    let (low, high) = named.fold((first, first), |(low, high), o| (low.min(o), high.max(o)));
    let end = match next {
        None => file_len,
        Some(o) if o >= min => o,
        Some(_) => high,
    };
    let (at, end) = (low.min(file_len), end.max(high).min(file_len));
    (end > at).then_some(at..end)
}

/// A record of `size` bytes refused as over `limit`, the one a caller set or
/// the room of its buffer, in the words every reader uses after naming the
/// record.
pub(crate) fn over_limit(size: usize, limit: usize) -> String {
    format!("{size} bytes, over the {limit}-byte limit")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 12;

    #[test]
    fn a_span_runs_from_the_first_record_to_the_next() {
        assert_eq!(span([40, 12, 70], Some(100), 1000, MIN), Some(12..100));
        // The run's records in any order; the next one lower than the run's
        // highest, as a record moved to the end of the file puts it.
        assert_eq!(span([70, 12, 40], Some(50), 1000, MIN), Some(12..70));
        // The next record is not where the span starts, even when it is lower.
        assert_eq!(span([40, 70], Some(20), 1000, MIN), Some(40..70));
    }

    #[test]
    fn at_the_end_of_the_database_a_span_runs_to_the_end_of_the_file() {
        assert_eq!(span([12, 40], None, 90, MIN), Some(12..90));
        assert_eq!(span([12, 40], Some(500), 90, MIN), Some(12..90), "within the file");
        assert_eq!(span([12, 40], None, 30, MIN), Some(12..30), "within the file");
        assert_eq!(span([95], None, 90, MIN), None, "past the file");
    }

    /// The rule for a next record whose offset names none: the span ends where
    /// the run's highest record starts, which is read on its own.
    #[test]
    fn a_next_offset_naming_no_record_ends_the_span_at_the_highest_record() {
        assert_eq!(span([12, 40, 70], Some(0), 1 << 40, MIN), Some(12..70));
        assert_eq!(span([12, 40, 70], Some(MIN - 1), 1 << 40, MIN), Some(12..70));
        assert_eq!(span([70, 12, 40], Some(3), 1 << 40, MIN), Some(12..70));
        // A run of one record then has no span.
        assert_eq!(span([40], Some(0), 1 << 40, MIN), None);
    }

    #[test]
    fn offsets_naming_no_record_are_left_out() {
        assert_eq!(span([0, 5, 40, 11], Some(90), 1000, MIN), Some(40..90));
        assert_eq!(span([0, 5, 11], Some(90), 1000, MIN), None, "no record named");
        assert_eq!(span([], None, 1000, MIN), None);
        assert_eq!(span([10, 20], Some(30), 1000, 10), Some(10..30), "the classic header");
    }
}
