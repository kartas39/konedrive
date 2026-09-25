//! The watcher in the daemon: [`ExamineSink`], which hands each batch to the
//! examination, and what the mode switch's hooks in `sync::write_mode` call to start
//! and stop an account's watcher with its sync.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{Handled, Sink, StatusHook, WatchConfig, WatchStatus, Watcher};
use crate::sync::disk::Disk;
use crate::sync::listing::LinkCell;
use crate::sync::local::ignore::SharedIgnore;
use crate::sync::local::{Batch, ExamineError, Examiner, Liveness};
use crate::sync::root::SyncRoot;
use crate::sync::{InodeLocks, RootState, SyncService};
use crate::tree::Store;

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
        let examined = {
            // On the examiner's own thread, never the runtime's.
            let _tree = self.tree_lock.as_ref().map(|lock| lock.blocking_lock());
            examiner.examine(batch)
        };
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
struct FirstScan<S: Sink> {
    inner: S,
    scanned: Option<tokio::sync::watch::Sender<bool>>,
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

impl SyncService {
    /// The watcher of the read-write folder at `root` (`docs/design/writes.md` §3), for the mode switch's hook
    /// ([`SyncService::start_watcher`]): made and started with nothing waited for — the
    /// bring-up walk runs on the watcher's own thread ([`Watcher::walked`]) — and no lock
    /// taken; it examines against `store`, the sync's.
    pub(in crate::sync) fn spawn_watcher(&self, root: &SyncRoot, store: &Store, scanned: Option<tokio::sync::watch::Sender<bool>>) -> Result<Watcher, String> {
        let store = store.clone();
        let runtime = tokio::runtime::Handle::try_current().map_err(|e| e.to_string())?;
        let mut config = WatchConfig::new(root.clone(), Arc::clone(&self.link), runtime.clone());
        config.on_status = Some(self.watch_hook(runtime.clone()));
        let sink = ExamineSink {
            root: root.clone(),
            store,
            locks: self.locks.clone(),
            ignore: Arc::clone(&self.ignore),
            // The helper's `OpenByHandle`: gone, moved out, or undecided.
            liveness: Box::new(crate::sync::local::HelperLiveness::new(
                Arc::new(crate::sync::upload::move_out::Linked(Arc::clone(&self.link))),
                root.clone(),
                runtime.clone(),
            )),
            link: Arc::clone(&self.link),
            runtime,
            on_rows: Some(self.outbox_waker()),
            on_handles: Some(self.handles_hook()),
            tree_lock: Some(Arc::clone(&self.tree_lock)),
        };
        let watcher = Watcher::start(config, Box::new(FirstScan { inner: sink, scanned }))
            .map_err(|e| format!("cannot watch {} for local changes: {e}", root.path.display()))?;
        tracing::info!("watching {} for local changes", root.path.display());
        Ok(watcher)
    }

    /// Stops `watcher` and waits for it, off the runtime and with no lock held:
    /// the examination under way finishes first. Its note leaves `LastError`.
    pub(in crate::sync) async fn stop_spawned_watcher(&self, watcher: Watcher) {
        if let Err(e) = tokio::task::spawn_blocking(move || watcher.stop()).await {
            tracing::warn!("the task stopping the watcher failed: {e}");
        }
        self.state.update(|s| s.watch_note.clear());
    }

    /// Keeps `LastError` in step with the watcher. When the folder itself was
    /// moved or deleted, the folder shows an error, said as a registration's
    /// trouble is (it outlasts the watcher), and its sync stops (§3.3):
    /// nothing is deleted in the cloud because it went.
    fn watch_hook(&self, runtime: tokio::runtime::Handle) -> StatusHook {
        let me = self.me.clone();
        Arc::new(move |status: &WatchStatus| {
            let Some(service) = me.upgrade() else { return };
            let note = status.note().unwrap_or_default();
            let gone = status.root_gone;
            service.state.update(|s| {
                if gone {
                    s.root_state = RootState::Error;
                    s.last_error = note;
                    s.watch_note.clear();
                } else {
                    s.watch_note = note;
                }
            });
            if gone {
                runtime.spawn(async move {
                    let _lifecycle = service.lifecycle.write().await;
                    service.stop_sync().await;
                });
            }
        })
    }
}
