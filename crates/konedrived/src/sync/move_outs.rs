use std::collections::HashSet;
use std::fs::File;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use konedrive_tree::outbox::{OutboxKind, OutboxRow};
use konedrive_tree::Store;

use crate::helper::Clearance;
use crate::folder::root::SyncRoot;
use crate::hydration::source::{self, Answered, ContentSource, FillError};
use crate::status::report::Report;
use crate::sync::folder::Stopped;
use crate::sync::running_sync::Handles;
use crate::sync::SyncService;
use crate::helper::linked::Linked;
use crate::upload::move_out::{Filler, MoveOuts, Tidy, drop_rows, home_trash};

// ---------------------------------------------------------------------------
// the account's side
// ---------------------------------------------------------------------------

/// A fill of a moved-out object for an account: shown in `Transfers` and recorded in the activity
/// log like a fill on open. From the folder's own source, held directly; `None` fills nothing.
struct AccountFill {
    source: Option<Arc<dyn ContentSource>>,
    report: Report,
}

#[async_trait]
impl Filler for AccountFill {
    async fn fill(&self, file: File, shown: &Path, clearance: Option<&Clearance>) -> Result<(), FillError> {
        let Some(source) = self.source.clone() else { return Err(FillError::Errno(libc::EIO)) };
        let shown = shown.display().to_string();
        let tracked = crate::hydration::tracked::Tracked::new(source, self.report.transfers.clone(), shown.clone());
        let filled = source::hydrate_with(file.into(), &tracked, clearance).await;
        let size = tracked.fetched();
        drop(tracked);
        let answered = match filled {
            Ok(()) => Answered::Filled,
            Err(e) => Answered::Failed(e),
        };
        if let Some(event) = crate::hydration::server::fill_event(&answered, &shown, size) {
            self.report.activity.record(vec![event]).await;
        }
        match answered {
            Answered::Failed(e) => Err(e),
            _ => Ok(()),
        }
    }
}

impl SyncService {
    /// What this account's outbox worker needs for `move-out` rows: the helper over the account's
    /// link, fills from `source`, the folder's, the registry's router told which item ids are this
    /// account's wherever they are (`docs/design/writes.md` §8, §8.3), and every account's folder.
    pub(super) fn move_outs(&self, source: Option<Arc<dyn ContentSource>>) -> MoveOuts {
        let (registry, me) = (Arc::downgrade(&self.wiring.registry), self.id().clone());
        let every = Arc::downgrade(&self.wiring.registry);
        MoveOuts {
            helper: Arc::new(Linked(Arc::clone(&self.link))),
            filler: Arc::new(AccountFill { source, report: self.report.clone() }),
            route: Some(Arc::new(move |ids| {
                if let Some(registry) = registry.upgrade() {
                    registry.set_moved_out(&me, ids);
                }
            })),
            home_trash: home_trash(),
            roots: Arc::new(move || every.upgrade().map(|registry| registry.folders()).unwrap_or_default()),
        }
    }

    /// The helper is back (`docs/design/writes.md` §10): what the pending `move-out` rows name is marked again
    /// before anything else the worker runs.
    pub(super) fn outbox_helper_back(&self) {
        if let Some(outbox) = self.running().as_ref().and_then(Handles::outbox) {
            outbox.helper_back();
        }
    }

    /// `rows` of the folder at `root` were dropped: what their `move-out`s left outside the
    /// folder is tidied ([`Tidy::dropped`]), whether or not a worker runs.
    pub(super) async fn tidy_dropped(&self, root: &SyncRoot, store: &Store, source: Option<Arc<dyn ContentSource>>, rows: &[OutboxRow]) {
        if !rows.iter().any(|r| r.kind == OutboxKind::MoveOut) {
            return;
        }
        let mo = self.move_outs(source);
        Tidy { mo: &mo, root, store, locks: &self.locks }.dropped(rows).await;
    }

    /// What a cycle of the sync being built calls with the rows it dropped because their
    /// items are gone from OneDrive: what their `move-out`s left outside the folder is
    /// tidied, in a task of that sync (`tidying`, waited for when the sync stops), off the
    /// reconcile's blocking thread. It holds what it needs, and no way back to the service.
    pub(super) fn tidy_after_cycle(
        &self,
        root: &SyncRoot,
        store: &Store,
        source: &Arc<dyn ContentSource>,
        tidying: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    ) -> Arc<dyn Fn(Vec<OutboxRow>) + Send + Sync> {
        // Off the reconcile's blocking task: this runtime is captured before entering it,
        // as the materializer's fills do.
        let runtime = tokio::runtime::Handle::current();
        let (mo, root, store, locks) = (self.move_outs(Some(Arc::clone(source))), root.clone(), store.clone(), self.locks.clone());
        Arc::new(move |rows| {
            if !rows.iter().any(|r| r.kind == OutboxKind::MoveOut) {
                return;
            }
            let (mo, root, store, locks) = (mo.clone(), root.clone(), store.clone(), locks.clone());
            let task = runtime.spawn(async move { Tidy { mo: &mo, root: &root, store: &store, locks: &locks }.dropped(&rows).await });
            let mut tidying = tidying.lock().unwrap_or_else(|p| p.into_inner());
            tidying.retain(|task| !task.is_finished());
            tidying.push(task);
        })
    }

    /// The registry routes none of this account's item ids to it any more: its `move-out` rows are
    /// dropped. A worker started later routes its own again.
    pub(super) fn forget_moved_out(&self) {
        self.wiring.registry.set_moved_out(self.id(), HashSet::new());
    }

    /// A Forget, or a Remove, drops the tree store and every row in it: the
    /// `move-out` rows go first, their items forgetting their local objects, and what they left
    /// outside the folder is tidied while the helper still holds the folder; the registry stops
    /// routing their ids. Dropped before they are tidied: a row kept over a placeholder already
    /// removed would read as the user's delete.
    pub(super) async fn drop_moved_out(&self, stopped: &mut Stopped<'_>, root: &SyncRoot) {
        if let Some(store) = self.store_in(stopped).await {
            match store.call(drop_rows).await {
                Ok(rows) => self.tidy_dropped(root, &store, stopped.folder().source(), &rows).await,
                Err(e) => tracing::warn!("the moves out of the folder waiting to finish cannot be read: {e}"),
            }
        }
        self.forget_moved_out();
    }

    /// Whether another account of this daemon claims an item id (`docs/design/writes.md` §8.3), for this
    /// account's reconcile: an object carrying it is never removed ([`Registry::claimed_elsewhere`]).
    ///
    /// [`Registry::claimed_elsewhere`]: crate::sync::registry::Registry::claimed_elsewhere
    pub(super) fn claims(&self) -> crate::remote::materialize::Claimed {
        let (registry, me) = (Arc::downgrade(&self.wiring.registry), self.id().clone());
        Arc::new(move |id| registry.upgrade().is_some_and(|registry| registry.claimed_elsewhere(&me, id)))
    }
}
