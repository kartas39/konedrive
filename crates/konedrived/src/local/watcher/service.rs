//! The watcher in the daemon: [`ExamineSink`], which hands each batch to the
//! examination, and what the mode switch's hooks in `sync::write_mode` call to start
//! and stop an account's watcher with its sync.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{Handled, Sink};
use crate::folder::disk::Disk;
use crate::helper::LinkCell;
use crate::local::ignore::SharedIgnore;
use crate::local::{Batch, ExamineError, Examiner, Liveness, ScanProgress, ScanReason};
use crate::local::scan::ScanReport;
use crate::folder::root::SyncRoot;
use crate::folder::locks::InodeLocks;
use konedrive_tree::Store;

/// The daemon's [`Sink`]: the examination of each batch against the
/// folder's base, which records outbox rows, and `MarkFile` for the
/// placeholders it finds with more than one link (Z3's way out).
pub struct ExamineSink {
    pub root: SyncRoot,
    pub store: Store,
    pub locks: InodeLocks,
    /// The account's ignore list, which `SetIgnorePatterns` changes.
    pub ignore: SharedIgnore,
    /// "Is this object alive, and where?" The daemon's asks the helper
    /// (`HelperLiveness`).
    pub liveness: Box<dyn Liveness>,
    pub link: LinkCell,
    pub runtime: tokio::runtime::Handle,
    /// Called when an examination recorded rows: wakes the outbox worker,
    /// which sends them.
    pub on_rows: Option<Arc<dyn Fn() + Send + Sync>>,
    /// The per-root tree lock (`docs/design/writes.md` §9): an examination and a
    /// cycle's reconcile, from its staging to its swap, exclude each other,
    /// so that neither sees the other's changes half made (the read-write reconcile).
    pub tree_lock: Option<Arc<tokio::sync::Mutex<()>>>,
    /// Told when the folder's filesystem had changed and its handles were taken
    /// again (`Some`, what to say), and when a Full scan found them current
    /// (`None`): `LastError` says so meanwhile.
    pub on_handles: Option<Arc<dyn Fn(Option<String>) + Send + Sync>>,
    /// Told how each Full local scan goes (issue #8); a single place examined is not.
    pub scan: Option<ScanReport>,
}

impl Sink for ExamineSink {
    fn handle(&mut self, batch: &Batch) -> Handled {
        let disk = match self.root.open_registered() {
            Ok(Some(_)) => match Disk::open(&self.root, false) {
                Ok(disk) => disk,
                Err(e) => return Handled::Failed(e.to_string()),
            },
            Ok(None) => return Handled::RootGone,
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)) => return Handled::RootGone,
            Err(e) => return Handled::Failed(e.to_string()),
        };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let ignore = self.ignore.read().unwrap_or_else(|p| p.into_inner()).clone();
        let examiner = Examiner { disk: &disk, store: &self.store, liveness: &*self.liveness, ignore: &ignore, locks: &self.locks, now };
        let run = self.scan.as_ref().filter(|_| batch.is_full()).map(|scan| scan.run(batch.reason().unwrap_or(ScanReason::Start)));
        let examined = {
            // On the examiner's own thread, never the runtime's.
            let _tree = self.tree_lock.as_ref().map(|lock| lock.blocking_lock());
            examiner.examine_reporting(batch, run.as_ref().map(|run| run as &dyn ScanProgress))
        };
        if let Some(run) = run {
            run.finish(examined.is_ok());
        }
        match examined {
            Ok(done) => {
                tracing::debug!(
                    "local changes examined: {} row(s) queued, {} removed, {} held for confirmation",
                    done.applied.queued.len(),
                    done.applied.removed.len(),
                    done.held
                );
                self.mark_files(&disk, &done.mark_files);
                if let Some(told) = self.on_handles.as_ref().filter(|_| done.renewed || batch.is_full()) {
                    told(done.renewed.then(|| {
                        "the folder is on another filesystem than its file handles were taken on (a new disk, a \
                         restored snapshot): they were taken again, and what was missing then is placed again from \
                         OneDrive rather than deleted there"
                            .to_owned()
                    }));
                }
                if !done.applied.queued.is_empty() {
                    if let Some(wake) = &self.on_rows {
                        wake();
                    }
                }
                Handled::Done { recheck: done.recheck }
            }
            Err(ExamineError::NoBase) => Handled::NotYet,
            Err(ExamineError::RootGone) => Handled::RootGone,
            Err(e) => Handled::Failed(e.to_string()),
        }
    }
}

/// A sink that says when it was first handed a batch — the watcher's bring-up hands over
/// the Full local scan first — whatever came of it (the read-write reconcile: the folder's first delta cycle
/// waits for it, `docs/design/writes.md` §3). Dropped unsent when the watcher stops first: the
/// cycle then waits no more.
pub(crate) struct FirstScan<S: Sink> {
    pub(crate) inner: S,
    pub(crate) scanned: Option<tokio::sync::watch::Sender<bool>>,
}

impl<S: Sink> Sink for FirstScan<S> {
    fn handle(&mut self, batch: &Batch) -> Handled {
        let handled = self.inner.handle(batch);
        if let Some(scanned) = self.scanned.take() {
            let _ = scanned.send(true);
        }
        handled
    }
}

impl ExamineSink {
    /// `MarkFile` for each placeholder the examination found with more than
    /// one link, so an open through a name outside every marked directory is
    /// intercepted too. The daemon's own open is not intercepted.
    fn mark_files(&self, disk: &Disk, rels: &[PathBuf]) {
        let Some(link) = self.link.lock().unwrap().clone() else { return };
        for rel in rels {
            let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else { continue };
            let marked = disk
                .dir(parent)
                .and_then(|dir| disk.open_file(&dir, name))
                .map_err(|e| e.to_string())
                .and_then(|file| self.runtime.block_on(link.mark_file(&file)).map_err(|e| e.to_string()));
            if let Err(e) = marked {
                tracing::warn!("cannot mark {} on its own: {e}", rel.display());
            }
        }
    }
}
