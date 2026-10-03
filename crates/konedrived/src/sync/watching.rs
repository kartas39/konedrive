use std::sync::Arc;

use konedrive_tree::Store;

use crate::local::watcher::{StatusHook, WatchConfig, WatchStatus, Watcher};
use crate::local::ScanReason;
use crate::local::scan::ScanReport;
use crate::folder::root::SyncRoot;
use crate::status::snapshot::RootState;
use crate::sync::SyncService;
use crate::local::watcher::service::{ExamineSink, FirstScan};

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
        config.first_scan = if self.switched_to_read_write.swap(false, std::sync::atomic::Ordering::SeqCst) {
            ScanReason::ReadWrite
        } else {
            ScanReason::Start
        };
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
