//! A read-write folder's part in uploading, as its cycle sees it
//! (`docs/design/writes.md` §9): what the cycle shares with the folder's
//! outbox worker. The cycle is the same fetch, stage, reconcile and swap as a
//! read-only folder's; what staging adds for it is in `stage.rs`, and what its
//! reconcile adds in `reconcile.rs`: it records the conflict copies it made,
//! lets rows wait for a folder made again, and says the cycle went through, so
//! that the outbox worker sends (§9).

use std::sync::Arc;

use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;

use super::{cancellable, CycleError, Listing};
use konedrive_tree::outbox::OutboxRow;

/// A read-write folder's cycle: what it shares with the folder's outbox
/// worker.
pub struct Writes {
    /// The per-root tree lock (`SyncService::tree_lock`): held from staging
    /// to the swap, as the outbox worker holds it across each commit.
    pub tree_lock: Arc<tokio::sync::Mutex<()>>,
    /// For conflict copies (§7): `name-<machine>.ext`.
    pub machine_name: String,
    /// The account's ignore list: what a removal keeps under an ignored
    /// name stays on this computer only.
    pub ignore: crate::local::SharedIgnore,
    /// A cycle went through: the outbox worker may send (§9).
    pub cycled: Arc<dyn Fn() + Send + Sync>,
    /// A cycle cleared trouble that stopped the folder ([`CycleError::blocking`]) — it
    /// went through, or failed with trouble that is only said: the write gate that
    /// trouble closed is open again, and
    /// the outbox worker that met it closed is woken, rather than left to its own timer.
    pub reopened: Arc<dyn Fn() + Send + Sync>,
    /// A held or pending `delete` row was dropped because its
    /// item is already gone from OneDrive ([`TreeStore::outbox_drop_removed`]):
    /// wakes the outbox worker at once, so `HeldCount`/`PendingCount` and the
    /// bus signal count it gone without waiting for the worker's own timer.
    ///
    /// [`TreeStore::outbox_drop_removed`]: konedrive_tree::TreeStore::outbox_drop_removed
    pub dropped_removed: Arc<dyn Fn(Vec<OutboxRow>) + Send + Sync>,
}

impl Listing {
    /// The tree lock of a read-write folder (`writes`), as the cycle waits for it.
    pub(super) async fn tree_lock(&self, writes: &Writes, cancel: &CancellationToken) -> Result<OwnedMutexGuard<()>, CycleError> {
        let lock = Arc::clone(&writes.tree_lock).lock_owned();
        cancellable(cancel, lock).await
    }
}
