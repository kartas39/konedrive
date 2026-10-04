//! The watcher of a read-write folder (`docs/design/writes.md` §3), from the folder's side:
//! `local::watcher` started with the folder's sync and owned by it (`running`), what it
//! tells the folder, and what the folder asks of it.

use std::sync::Arc;
use std::time::Duration;

use konedrive_tree::Store;
use tokio::sync::watch;

use super::running_sync::Handles;
use super::SyncService;
use crate::folder::root::SyncRoot;
use crate::local::scan::ScanReport;
use crate::local::watcher::service::{ExamineSink, FirstScan};
use crate::local::watcher::{StatusHook, WatchConfig, WatchStatus, Watcher};
use crate::local::ScanReason;
use crate::upload::OutboxHandle;

/// How long the switch to read-only waits for the watcher to hand over what it holds.
const FLUSH_WITHIN: Duration = Duration::from_secs(30);

impl SyncService {
    /// Starts the watcher of the read-write folder at `root`, wired to the parts of the
    /// sync being built: it examines against `store`, the sync's, under its `tree_lock`,
    /// wakes `outbox` when it recorded rows, and says its status through
    /// [`watch_hook`](Self::watch_hook). Called as a read-write folder's sync is built — at
    /// bring-up, and after a switch to read-write — inside a change of the folder's state:
    /// the sync keeps the watcher, and stops it when it stops. It does not block and takes
    /// no lock: the watcher's bring-up walk (every directory marked for interception and
    /// for events, then the Full local scan) runs on its own thread, and
    /// [`Watcher::walked`] says when it is done. `scanned` is told once that first
    /// examination has been handed over, whatever came of it: the folder's first delta
    /// cycle waits for it (the bring-up order of `docs/design/writes.md` §2.2).
    ///
    /// When it cannot start, the caller runs the sync locked, as a read-only folder's, and
    /// says why: nothing is made in the folder unwatched.
    ///
    /// `reason`: why its first, Full local scan runs — the folder came up, or its account
    /// was switched to read-write.
    pub(super) fn spawn_watcher(
        &self,
        root: &SyncRoot,
        store: &Store,
        tree_lock: &Arc<tokio::sync::Mutex<()>>,
        outbox: OutboxHandle,
        scanned: Option<watch::Sender<bool>>,
        reason: ScanReason,
    ) -> Result<Watcher, String> {
        let store = store.clone();
        let runtime = tokio::runtime::Handle::try_current().map_err(|e| e.to_string())?;
        let mut config = WatchConfig::new(root.clone(), Arc::clone(&self.link), runtime.clone());
        config.on_status = Some(self.watch_hook(runtime.clone()));
        config.first_scan = reason;
        let handles = self.state.clone();
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
            // Rows were recorded: the sync's own worker looks at the outbox.
            on_rows: Some(Arc::new(move || outbox.wake())),
            // The folder's file handles were taken again on a changed filesystem, which
            // `LastError` says until a Full local scan finds them current.
            on_handles: Some(Arc::new(move |note: Option<String>| handles.update(|s| s.local.handles_note = note.clone().unwrap_or_default()))),
            tree_lock: Some(Arc::clone(tree_lock)),
            scan: Some(ScanReport::new(self.state.clone())),
        };
        let watcher = (self.wiring.watchers)(config, Box::new(FirstScan { inner: sink, scanned }))
            .map_err(|e| format!("cannot watch {} for local changes: {e}", root.path.display()))?;
        tracing::info!("watching {} for local changes", root.path.display());
        Ok(watcher)
    }

    /// The running watcher, if any, hands over and has examined what it holds (the watcher),
    /// so that a change saved a moment ago is in the outbox.
    pub(super) async fn flush_watcher(&self) {
        let handle = self.running().and_then(|running| running.watcher().cloned());
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
        if let Some(watcher) = self.running().as_ref().and_then(Handles::watcher) {
            watcher.helper_back();
        }
    }

    /// The ignore list changed (`docs/design/writes.md` §4.4): a Full local scan now, if a
    /// watcher runs, so that a name no longer ignored is uploaded.
    pub(super) fn rescan_for_ignore_list(&self) {
        if let Some(watcher) = self.running().as_ref().and_then(Handles::watcher) {
            watcher.full_scan(ScanReason::IgnoreList);
        }
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
            } else if status.stopped {
                // It ended by itself: the folder is not writable any more, and says why.
                // Once: the sync that is started again has no watcher.
                if service.running().is_some_and(|running| running.watcher().is_some()) {
                    runtime.spawn(async move { service.watcher_ended(note).await });
                } else {
                    service.state.update(|s| s.local.watch_note = note);
                }
            } else {
                service.state.update(|s| s.local.watch_note = note);
            }
        })
    }
}
