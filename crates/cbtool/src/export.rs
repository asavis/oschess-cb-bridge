//! `cbtool pgn`: export games as PGN, rendered on every worker and written
//! in the order asked.

use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use cbformat::game::{Head, RecordKind};
use cbformat::pgn::{AnnotationStatus, Options};
use cbformat::view::{self, Base};

use crate::{AnyResult, LIMITS, USAGE, run_workers, threads};

/// Refuses an output path that is one of the database's own files, by name or
/// through any alias such as a hard link: creating it would truncate the input
/// while it is being read.
fn refuse_database_file(out: &Path, db: &Base) -> AnyResult<()> {
    if !out.exists() {
        return Ok(());
    }
    for input in db.file_paths() {
        if input.exists() && same_file::is_same_file(out, &input)? {
            return Err(format!("refusing to overwrite {}, a file of the database being read", out.display()).into());
        }
    }
    Ok(())
}

pub(crate) fn pgn(path: &str, rest: &[String]) -> AnyResult<bool> {
    let db = Base::open(path)?;
    let mut out_path = None;
    let mut options = Options::default();
    let mut ids = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--out" {
            out_path = Some(it.next().ok_or(USAGE)?.clone());
        } else if a == "--lang" {
            options = Options::with_languages(it.next().ok_or(USAGE)?.split(','));
        } else {
            ids.push(a.parse::<u32>()?);
        }
    }
    if ids.is_empty() {
        ids = game_ids(&db)?;
    }
    let sink: Box<dyn Write + Send> = match out_path {
        Some(p) => {
            refuse_database_file(Path::new(&p), &db)?;
            Box::new(std::fs::File::create(p)?)
        }
        None => Box::new(std::io::stdout()),
    };
    let mut w = BufWriter::with_capacity(1 << 20, sink);
    let ok = export_in_order(&ids, threads(), &|id, r| r.game(&db, id, &options), &mut w)?;
    w.flush()?;
    Ok(ok)
}

/// The id of every game, from headers read [`cbformat::game::MAX_BATCH_RECORDS`]
/// at a time. A failed read fails the export: a database damaged or truncated
/// under it must not give a silently incomplete one.
fn game_ids(db: &Base) -> cbformat::Result<Vec<u32>> {
    let mut ids = Vec::new();
    let mut first = 1;
    loop {
        let records = db.headers(first, db.record_count())?;
        let Some(last) = records.last() else { break };
        ids.extend(records.iter().filter(|r| r.kind() == RecordKind::Game).map(|r| r.id()));
        let Some(next) = last.id().checked_add(1) else { break };
        first = next;
    }
    Ok(ids)
}

/// Games per task, and the rendered text a task may hold before its turn to
/// write. A task over the budget waits for its turn and then streams the rest
/// of its games straight to the output, so memory stays near
/// `threads × PGN_TASK_BYTES` however large each game renders.
const PGN_CHUNK: usize = 1_024;
const PGN_TASK_BYTES: usize = 4 << 20;

/// Rendered games waiting for their turn to be written: the text, and each
/// message for stderr with the text offset it belongs at, to keep stderr in
/// game order. A message is a failure unless it only notes incomplete
/// annotations.
#[derive(Default)]
struct Rendered<'db> {
    text: String,
    errors: Vec<(usize, String, bool)>,
    /// The records around the last one rendered, read together.
    batch: Option<view::Batch<'db>>,
}

/// Records read together when rendering; ids outside the batch start a new one.
const RENDER_BATCH: u32 = 1_024;

impl<'db> Rendered<'db> {
    fn game(&mut self, db: &'db Base, id: u32, options: &Options) {
        if !self.batch.as_ref().is_some_and(|b| b.ids().contains(&id)) {
            self.batch = db.batch(id, id.saturating_add(RENDER_BATCH - 1)).ok();
        }
        let rendered = match &self.batch {
            Some(batch) => batch.pgn(id, options, LIMITS),
            None => db.pgn(id, options, LIMITS),
        };
        match rendered {
            Ok(game) => {
                if let AnnotationStatus::Incomplete { type_code } = game.annotations {
                    let note = format!("game {id}: annotations incomplete: type {type_code:#04x} of unknown layout");
                    self.errors.push((self.text.len(), note, false));
                }
                self.text.push_str(&game.pgn);
                self.text.push('\n');
            }
            Err(e) => self.errors.push((self.text.len(), format!("game {id}: {e}"), true)),
        }
    }

    fn failed(&self) -> bool {
        self.errors.iter().any(|e| e.2)
    }

    fn write_to(&mut self, out: &mut dyn Write) -> std::io::Result<()> {
        let mut at = 0;
        for (offset, error, _) in &self.errors {
            out.write_all(&self.text.as_bytes()[at..*offset])?;
            eprintln!("{error}");
            at = *offset;
        }
        out.write_all(&self.text.as_bytes()[at..])?;
        self.text.clear();
        self.errors.clear();
        Ok(())
    }
}

/// Renders `ids` with `render` on `threads` workers and writes them in the
/// order given. Returns whether every game rendered, with failures reported on
/// stderr in the same order. A write error stops every worker: no chunk is
/// claimed and no game rendered after it, and waiting workers are woken.
fn export_in_order<'db>(
    ids: &[u32],
    threads: usize,
    render: &(dyn Fn(u32, &mut Rendered<'db>) + Sync),
    out: &mut (dyn Write + Send),
) -> AnyResult<bool> {
    struct Turn<'w> {
        next: usize,
        out: &'w mut (dyn Write + Send),
        ok: bool,
        failed: Option<std::io::Error>,
    }
    let chunks: Vec<&[u32]> = ids.chunks(PGN_CHUNK).collect();
    let claimed = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let turn = Mutex::new(Turn { next: 0, out, ok: true, failed: None });
    let your_turn = Condvar::new();
    run_workers(threads.min(chunks.len()), &|| {
        let mut r = Rendered::default();
        while !stop.load(Ordering::Relaxed) {
            let k = claimed.fetch_add(1, Ordering::Relaxed);
            let Some(chunk) = chunks.get(k) else { break };
            let mut done = 0;
            while done < chunk.len() && r.text.len() < PGN_TASK_BYTES && !stop.load(Ordering::Relaxed) {
                render(chunk[done], &mut r);
                done += 1;
            }
            let mut t = turn.lock().unwrap_or_else(|e| e.into_inner());
            while t.next != k && t.failed.is_none() {
                t = your_turn.wait(t).unwrap_or_else(|e| e.into_inner());
            }
            if t.failed.is_some() {
                break;
            }
            // Our turn: write what is held, then stream the rest.
            let mut chunk_ok = !r.failed();
            let mut result = r.write_to(&mut *t.out);
            for &id in &chunk[done..] {
                if result.is_err() {
                    break;
                }
                render(id, &mut r);
                chunk_ok &= !r.failed();
                result = r.write_to(&mut *t.out);
            }
            t.ok &= chunk_ok;
            match result {
                Ok(()) => t.next += 1,
                Err(e) => {
                    t.failed = Some(e);
                    stop.store(true, Ordering::Relaxed);
                }
            }
            your_turn.notify_all();
        }
    });
    let t = turn.into_inner().unwrap_or_else(|e| e.into_inner());
    match t.failed {
        Some(e) => Err(e.into()),
        None => Ok(t.ok),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink that accepts `left` bytes and then fails every write.
    struct FailAfter {
        left: usize,
    }

    impl Write for FailAfter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.left == 0 {
                return Err(std::io::Error::other("sink full"));
            }
            let n = buf.len().min(self.left);
            self.left -= n;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn fake(id: u32, r: &mut Rendered<'_>) {
        r.text.push_str(&format!("game {id}\n"));
    }

    #[test]
    fn writes_in_order() {
        let ids: Vec<u32> = (0..5_000).collect();
        let mut out = Vec::new();
        assert!(export_in_order(&ids, 4, &fake, &mut out).unwrap());
        let want: String = ids.iter().map(|id| format!("game {id}\n")).collect();
        assert_eq!(String::from_utf8(out).unwrap(), want);
    }

    #[test]
    fn a_write_error_stops_the_workers() {
        let ids: Vec<u32> = (0..1_000_000).collect();
        let rendered = AtomicUsize::new(0);
        let render = |id: u32, r: &mut Rendered| {
            rendered.fetch_add(1, Ordering::Relaxed);
            fake(id, r);
        };
        let threads = 4;
        let err = export_in_order(&ids, threads, &render, &mut FailAfter { left: 100 }).unwrap_err();
        assert!(err.to_string().contains("sink full"), "{err}");
        // At most the chunks already claimed when the write failed.
        let bound = (threads + 1) * PGN_CHUNK;
        let n = rendered.load(Ordering::Relaxed);
        assert!(n <= bound, "rendered {n} games after the output failed; bound {bound}");
    }
}
