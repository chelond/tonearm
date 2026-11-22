//! Crash- and race-safe file writes.
//!
//! * [`write_atomic`]: unique temp file in the target directory (`create_new`, so two writers never
//!   share one), `fsync`, then `rename` over the target. A crash leaves either the old or the new
//!   file, never half of one; concurrent writers cannot rename each other's half-written temp file.
//! * [`write_later`]: queues a write for one background writer thread. Jobs run in the order they
//!   were queued, so an older snapshot can never overwrite a newer one, and when several jobs for
//!   the same file pile up only the newest is written (coalescing).
//! * [`flush`]: waits until everything queued so far is on disk (call it before exit).

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};

/// Who may read the file: everyone with access to the directory, or the owner only (0600).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Normal,
    Private,
}

/// Atomically replaces `path` with `data`, creating the parent directory if needed.
pub fn write_atomic(path: &Path, data: &[u8], access: Access) -> io::Result<()> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    fs::create_dir_all(dir)?;
    let (tmp, mut file) = create_temp(dir, path, access)?;
    let written = file
        .write_all(data)
        .and_then(|()| file.sync_all())
        .and_then(|()| {
            drop(file);
            fs::rename(&tmp, path)
        });
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// `.<name>.<pid>.<n>.tmp` next to the target, opened with `create_new` (never follows a link
/// or reuses someone else's file).
fn create_temp(dir: &Path, target: &Path, access: Access) -> io::Result<(PathBuf, fs::File)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = target
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{name}.{}.{n}.tmp", std::process::id()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(match access {
                Access::Private => 0o600,
                Access::Normal => 0o644,
            });
        }
        #[cfg(not(unix))]
        let _ = access;
        match options.open(&tmp) {
            Ok(f) => return Ok((tmp, f)),
            // Leftover of a crashed run with the same pid: take the next number.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
}

type Make = Box<dyn FnOnce() -> Option<Vec<u8>> + Send>;

enum Job {
    Write {
        path: PathBuf,
        access: Access,
        make: Make,
    },
    Flush(mpsc::Sender<()>),
}

fn writer() -> &'static Mutex<mpsc::Sender<Job>> {
    static TX: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("tonearm-writer".into())
            .spawn(move || run_writer(&rx))
            .expect("spawn the file writer thread");
        Mutex::new(tx)
    })
}

fn run_writer(rx: &mpsc::Receiver<Job>) {
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        batch.extend(rx.try_iter());
        run_batch(batch);
    }
}

/// Runs one drained batch: for each file only its newest job, in queue order; flush markers
/// are answered after everything queued before them.
fn run_batch(batch: Vec<Job>) {
    let mut last: HashMap<PathBuf, usize> = HashMap::new();
    for (i, job) in batch.iter().enumerate() {
        if let Job::Write { path, .. } = job {
            last.insert(path.clone(), i);
        }
    }
    for (i, job) in batch.into_iter().enumerate() {
        match job {
            Job::Write { path, access, make } => {
                if last.get(&path) != Some(&i) {
                    continue; // a newer snapshot of this file follows in the same batch
                }
                let Some(data) = make() else { continue };
                if let Err(e) = write_atomic(&path, &data, access) {
                    tracing::warn!(error = %e, path = %path.display(), "cannot save file");
                }
            }
            Job::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

/// Queues an atomic write of whatever `make` returns (`None` skips it). `make` runs on the writer
/// thread, so serialising a large value does not block the caller.
pub fn write_later(
    path: PathBuf,
    access: Access,
    make: impl FnOnce() -> Option<Vec<u8>> + Send + 'static,
) {
    let job = Job::Write {
        path,
        access,
        make: Box::new(make),
    };
    let sent = writer()
        .lock()
        .map(|tx| tx.send(job).is_ok())
        .unwrap_or(false);
    if !sent {
        tracing::warn!("file writer is gone; change not saved");
    }
}

/// Blocks until every write queued before this call is finished.
pub fn flush() {
    let (done_tx, done_rx) = mpsc::channel();
    let sent = writer()
        .lock()
        .map(|tx| tx.send(Job::Flush(done_tx)).is_ok())
        .unwrap_or(false);
    if sent {
        let _ = done_rx.recv();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_files(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| {
                let n = e.unwrap().file_name().to_string_lossy().into_owned();
                n.ends_with(".tmp").then_some(n)
            })
            .collect()
    }

    #[test]
    fn writes_and_replaces_without_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("a.json");
        write_atomic(&p, b"one", Access::Normal).unwrap();
        write_atomic(&p, b"two", Access::Normal).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert!(temp_files(p.parent().unwrap()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn private_files_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("secret.toml");
        write_atomic(&p, b"x", Access::Private).unwrap();
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn concurrent_writers_never_leave_a_mixed_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.txt");
        let a = vec![b'a'; 256 * 1024];
        let b = vec![b'b'; 256 * 1024];
        std::thread::scope(|s| {
            for i in 0..8 {
                let data = if i % 2 == 0 { a.clone() } else { b.clone() };
                let p = p.clone();
                s.spawn(move || {
                    for _ in 0..10 {
                        write_atomic(&p, &data, Access::Normal).unwrap();
                    }
                });
            }
        });
        let got = fs::read(&p).unwrap();
        assert!(got == a || got == b, "file is a mix of two writers");
        assert!(temp_files(dir.path()).is_empty());
    }

    #[test]
    fn queued_writes_keep_order_and_the_newest_wins() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("q.txt");
        for i in 0..200u32 {
            let data = i.to_string().into_bytes();
            write_later(p.clone(), Access::Normal, move || Some(data));
        }
        flush();
        assert_eq!(fs::read_to_string(&p).unwrap(), "199");
    }

    #[test]
    fn coalescing_skips_stale_snapshots_in_a_batch() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("b.txt");
        let made = std::sync::Arc::new(AtomicU64::new(0));
        let mut batch = Vec::new();
        for i in 0..5u8 {
            let made = std::sync::Arc::clone(&made);
            batch.push(Job::Write {
                path: p.clone(),
                access: Access::Normal,
                make: Box::new(move || {
                    made.fetch_add(1, Ordering::Relaxed);
                    Some(vec![b'0' + i])
                }),
            });
        }
        run_batch(batch);
        assert_eq!(fs::read(&p).unwrap(), b"4");
        assert_eq!(
            made.load(Ordering::Relaxed),
            1,
            "only the newest snapshot is serialised"
        );
    }

    #[test]
    fn skipped_job_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("none.txt");
        write_later(p.clone(), Access::Normal, || None);
        flush();
        assert!(!p.exists());
    }
}
