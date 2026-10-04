//! The watcher of a read-write folder (`docs/design/writes.md` §3), from the folder's side:
//! `local::watcher` started with the folder's sync, kept in it and stopped with it, what it
//! tells the folder, and what the folder asks of it.

use std::sync::Arc;
use std::time::Duration;

use konedrive_tree::Store;
use tokio::sync::watch;

use super::SyncService;
use crate::config::Mode;
use crate::folder::root::SyncRoot;
use crate::local::scan::ScanReport;
use crate::local::watcher::service::{ExamineSink, FirstScan};
use crate::local::watcher::{StatusHook, WatchConfig, WatchStatus, Watcher};
use crate::local::ScanReason;

/// How long the switch to read-only waits for the watcher to hand over what it holds.
const FLUSH_WITHIN: Duration = Duration::from_secs(30);

impl SyncService {
    /// Starts the watcher of the folder at `root`, if the folder is read-write, and answers
    /// it. Called as a read-write folder's sync starts — at bring-up, and after a switch to
    /// read-write — inside a change of the folder's state and before the sync is published: the
    /// sync keeps the watcher, and whoever stops the sync gives it to
    /// [`stop_watcher`](Self::stop_watcher). It does not block and takes no lock: the
    /// watcher's bring-up walk (every directory marked for interception and for events, then
    /// the Full local scan) runs on its own thread, and [`Watcher::walked`] says when it is
    /// done. `scanned` is told once that first examination has been handed over, whatever
    /// came of it: the folder's first delta cycle waits for it (the bring-up order of
    /// `docs/design/writes.md` §2.2).
    ///
    /// A watcher that cannot start is said in `LastError`, and the caller runs the sync
    /// locked, as a read-only folder's: nothing is made in the folder unwatched.
    ///
    /// `reason`: why its first, Full local scan runs — the folder came up, or its account
    /// was switched to read-write.
    pub(crate) fn start_watcher(&self, root: &SyncRoot, store: &Store, scanned: Option<watch::Sender<bool>>, reason: ScanReason) -> Option<Watcher> {
        if self.mode() != Mode::ReadWrite {
            return None;
        }
        match self.spawn_watcher(root, store, scanned, reason) {
            Ok(watcher) => Some(watcher),
            Err(why) => {
                tracing::warn!("{why}; the folder stays locked");
                self.state.update(|s| {
                    s.local.watch_note = format!("the folder stays read-only and nothing is uploaded: local changes cannot be watched ({why})")
                });
                None
            }
        }
    }

    /// [`start_watcher`](Self::start_watcher)'s watcher itself, wired to the folder: it
    /// examines against `store`, the sync's, wakes the outbox worker when it recorded rows,
    /// and says its status through [`watch_hook`](Self::watch_hook).
    fn spawn_watcher(&self, root: &SyncRoot, store: &Store, scanned: Option<watch::Sender<bool>>, reason: ScanReason) -> Result<Watcher, String> {
        let store = store.clone();
        let runtime = tokio::runtime::Handle::try_current().map_err(|e| e.to_string())?;
        let mut config = WatchConfig::new(root.clone(), Arc::clone(&self.link), runtime.clone());
        config.on_status = Some(self.watch_hook(runtime.clone()));
        config.first_scan = reason;
        let sink = ExamineSink {
            root: root.clone(),
            store,
            locks: self.locks.clone(),
            ignore: Arc::clone(&self.ignore),
            // The helper's `OpenByHandle`: gone, moved out, or undecided.
            liveness: Box::new(crate::local::HelperLiveness::new(
                Arc::new(crate::helper::linked::Linked(Arc::clone(&self.link))),
                root.clone(),
                runtime.clone(),
            )),
            link: Arc::clone(&self.link),
            runtime,
            on_rows: Some(self.outbox_waker()),
            on_handles: Some(self.handles_hook()),
            tree_lock: Some(Arc::clone(&self.tree_lock)),
            scan: Some(ScanReport::new(self.state.clone())),
        };
        let watcher = (self.wiring.watchers)(config, Box::new(FirstScan { inner: sink, scanned }))
            .map_err(|e| format!("cannot watch {} for local changes: {e}", root.path.display()))?;
        tracing::info!("watching {} for local changes", root.path.display());
        Ok(watcher)
    }

    /// Stops `watcher` and waits for it, off the runtime and with no lock held: the
    /// examination under way finishes first. Its note leaves `LastError`. Called by whoever
    /// stopped the sync that kept it, and only by them.
    pub(crate) async fn stop_watcher(&self, watcher: Watcher) {
        if let Err(e) = tokio::task::spawn_blocking(move || watcher.stop()).await {
            tracing::warn!("the task stopping the watcher failed: {e}");
        }
        self.state.update(|s| s.local.watch_note.clear());
    }

    /// The running watcher, if any, hands over and has examined what it holds (the watcher),
    /// so that a change saved a moment ago is in the outbox.
    pub(super) async fn flush_watcher(&self) {
        let handle = self.syncing.lock().unwrap().as_ref().and_then(|s| s.watcher.as_ref()).map(Watcher::handle);
        if let Some(handle) = handle {
            let flushed = tokio::task::spawn_blocking(move || handle.flush(FLUSH_WITHIN)).await.unwrap_or(false);
            if !flushed {
                tracing::warn!("the latest local changes could not all be examined; they are not counted as waiting");
            }
        }
    }

    /// The helper is back (`docs/design/writes.md` §3): what the watcher could not have marked for
    /// interception meanwhile is asked again, and a Full local scan finds what changed.
    pub(super) fn watcher_helper_back(&self) {
        if let Some(watcher) = self.syncing.lock().unwrap().as_ref().and_then(|s| s.watcher.as_ref()) {
            watcher.helper_back();
        }
    }

    /// The ignore list changed (`docs/design/writes.md` §4.4): a Full local scan now, if a
    /// watcher runs, so that a name no longer ignored is uploaded.
    pub(super) fn rescan_for_ignore_list(&self) {
        if let Some(watcher) = self.syncing.lock().unwrap().as_ref().and_then(|s| s.watcher.as_ref()) {
            watcher.full_scan(ScanReason::IgnoreList);
        }
    }

    /// What a read-write cycle hands the running watcher for examination: what the
    /// reconcile kept or copied. Dropped when no watcher runs.
    pub(super) fn examine_hook(&self) -> Arc<dyn Fn(crate::local::Batch) + Send + Sync> {
        let me = self.me.clone();
        Arc::new(move |batch| {
            let Some(service) = me.upgrade() else { return };
            let handle = service.syncing.lock().unwrap().as_ref().and_then(|s| s.watcher.as_ref()).map(Watcher::handle);
            if let Some(handle) = handle {
                handle.examine(batch);
            }
        })
    }

    /// Keeps `LastError` in step with the watcher. When the folder itself was
    /// moved or deleted, the folder is down ([`root_gone`](Self::root_gone)): it shows an
    /// error, said as a registration's trouble is (it outlasts the watcher), and its sync
    /// stops (§3.3): nothing is deleted in the cloud because it went.
    fn watch_hook(&self, runtime: tokio::runtime::Handle) -> StatusHook {
        let me = self.me.clone();
        Arc::new(move |status: &WatchStatus| {
            let Some(service) = me.upgrade() else { return };
            let note = status.note().unwrap_or_default();
            if status.root_gone {
                service.state.update(|s| s.local.watch_note.clear());
                runtime.spawn(async move { service.root_gone(note).await });
            } else {
                service.state.update(|s| s.local.watch_note = note);
            }
        })
    }
}
