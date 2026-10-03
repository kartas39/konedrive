use std::collections::HashSet;
use std::fs::File;
use std::path::Path;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use konedrive_tree::outbox::{OutboxKind, OutboxRow};
use konedrive_tree::Store;

use crate::helper::Clearance;
use crate::folder::root::SyncRoot;
use crate::hydration::source::{self, Answered, FillError};
use crate::sync::SyncService;
use crate::upload::move_out::{Filler, Linked, MoveOuts, Tidy, drop_rows, home_trash};

// ---------------------------------------------------------------------------
// the account's side
// ---------------------------------------------------------------------------

/// A fill of a moved-out object for an account: shown in `Transfers` and recorded in the activity
/// log like a fill on open.
struct AccountFill {
    sync: Weak<SyncService>,
}

#[async_trait]
impl Filler for AccountFill {
    async fn fill(&self, file: File, shown: &Path, clearance: Option<&Clearance>) -> Result<(), FillError> {
        let Some(sync) = self.sync.upgrade() else { return Err(FillError::Errno(libc::EIO)) };
        let Some(source) = sync.source.lock().unwrap().clone() else { return Err(FillError::Errno(libc::EIO)) };
        let shown = shown.display().to_string();
        let tracked = crate::hydration::tracked::Tracked::new(source, sync.report.transfers.clone(), shown.clone());
        let filled = source::hydrate_with(file.into(), &tracked, clearance).await;
        let size = tracked.fetched();
        drop(tracked);
        let answered = match filled {
            Ok(()) => Answered::Filled,
            Err(e) => Answered::Failed(e),
        };
        if let Some(event) = crate::hydration::server::fill_event(&answered, &shown, size) {
            sync.report.activity.record(vec![event]).await;
        }
        match answered {
            Answered::Failed(e) => Err(e),
            _ => Ok(()),
        }
    }
}

impl SyncService {
    /// What this account's outbox worker needs for `move-out` rows: the helper over the account's
    /// link, fills through the account's source, the hub's router told which item ids are this
    /// account's wherever they are (`docs/design/writes.md` §8, §8.3), and every account's folder.
    pub(in crate::sync) fn move_outs(&self) -> MoveOuts {
        let (hub, me) = (Arc::downgrade(&self.hub), self.me.clone());
        let every = Arc::downgrade(&self.hub);
        MoveOuts {
            helper: Arc::new(Linked(Arc::clone(&self.link))),
            filler: Arc::new(AccountFill { sync: self.me.clone() }),
            route: Some(Arc::new(move |ids| {
                if let Some(hub) = hub.upgrade() {
                    hub.set_moved_out(&me, ids);
                }
            })),
            home_trash: home_trash(),
            roots: Arc::new(move || every.upgrade().map(|hub| hub.accounts().iter().flat_map(|a| a.folders()).collect()).unwrap_or_default()),
        }
    }

    /// What the examination says of the folder's file handles: taken again on a
    /// changed filesystem, which `LastError` says until a Full local scan finds them current.
    pub(in crate::sync) fn handles_hook(&self) -> Arc<dyn Fn(Option<String>) + Send + Sync> {
        let me = self.me.clone();
        Arc::new(move |note| {
            if let Some(service) = me.upgrade() {
                service.state.update(|s| s.handles_note = note.clone().unwrap_or_default());
            }
        })
    }

    /// The helper is back (`docs/design/writes.md` §10): what the pending `move-out` rows name is marked again
    /// before anything else the worker runs.
    pub(in crate::sync) fn outbox_helper_back(&self) {
        if let Some(outbox) = self.syncing.lock().unwrap().as_ref().and_then(|s| s.outbox.as_ref()) {
            outbox.helper_back();
        }
    }

    /// `rows` of the folder at `root` were dropped: what their `move-out`s left outside the
    /// folder is tidied ([`Tidy::dropped`]), whether or not a worker runs.
    pub(in crate::sync) async fn tidy_dropped(&self, root: &SyncRoot, store: &Store, rows: &[OutboxRow]) {
        if !rows.iter().any(|r| r.kind == OutboxKind::MoveOut) {
            return;
        }
        let mo = self.move_outs();
        Tidy { mo: &mo, root, store, locks: &self.locks }.dropped(rows).await;
    }

    /// The hub routes none of this account's item ids to it any more: its `move-out` rows are
    /// dropped. A worker started later routes its own again.
    pub(in crate::sync) fn forget_moved_out(&self) {
        self.hub.set_moved_out(&self.me, HashSet::new());
    }

    /// A Forget, or a Remove, drops the tree store and every row in it: the
    /// `move-out` rows go first, their items forgetting their local objects, and what they left
    /// outside the folder is tidied while the helper still holds the folder; the hub stops
    /// routing their ids. Dropped before they are tidied: a row kept over a placeholder already
    /// removed would read as the user's delete.
    pub(in crate::sync) async fn drop_moved_out(&self, root: &SyncRoot) {
        let store = self.store.lock().unwrap().clone();
        if let Some(store) = store {
            match store.call(drop_rows).await {
                Ok(rows) => self.tidy_dropped(root, &store, &rows).await,
                Err(e) => tracing::warn!("the moves out of the folder waiting to finish cannot be read: {e}"),
            }
        }
        self.forget_moved_out();
    }

    /// Whether another account of this daemon claims an item id (`docs/design/writes.md` §8.3), for this
    /// account's reconcile: an object carrying it is never removed ([`HelperHub::claimed_elsewhere`]).
    ///
    /// [`HelperHub::claimed_elsewhere`]: crate::sync::hub::HelperHub::claimed_elsewhere
    pub(in crate::sync) fn claims(&self) -> crate::remote::materialize::Claimed {
        let (hub, me) = (Arc::downgrade(&self.hub), self.me.clone());
        Arc::new(move |id| hub.upgrade().is_some_and(|hub| hub.claimed_elsewhere(&me, id)))
    }
}
