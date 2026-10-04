//! Local changes, from the disk to the outbox (`docs/design/writes.md` §4, amended by
//! §17): what a read-write folder holds that the base does not.
//!
//! The watcher turns notification events into a [`Batch`] of dirty
//! places — directories and names, and object handles — and hands it over
//! once the folder has been quiet for [`QUIET`] (at the latest [`CEILING`]
//! after the first event). The [`Examiner`] compares what is on disk there
//! with the base (`items`) and records what differs as outbox rows
//! ([`konedrive_tree::outbox`]); the outbox worker sends them. A
//! [`Batch::full`] examines every directory: the Full local scan, run at
//! bring-up, after a queue overflow, after a helper reconnect and when the
//! ignore list shrinks.
//!
//! Events are hints, never the truth: every decision is made from the disk,
//! by item id, file handle and content, so a lost or merged event costs a
//! scan, never a wrong upload. Nothing here runs from the daemon yet; the watcher and
//! the outbox worker wire it in.

pub mod batch;
mod entry;
pub mod examine;
pub mod handles;
pub mod ignore;
pub mod liveness;
pub mod names;
#[cfg(test)]
mod tests;
pub mod scan;
#[cfg(test)]
pub mod testing;
pub mod watcher;

use std::ffi::OsStr;
use std::fs::File;
use std::path::Path;
use std::time::Duration;

pub use batch::{Batch, ScanReason};
pub use examine::{ExamineError, Examined, Examiner, ScanProgress};
pub use ignore::IgnoreList;
#[cfg(test)]
pub use testing::FakeLiveness;
pub use liveness::{HelperLiveness, Liveness, NoLiveness, Whereabouts};

use konedrive_fs::handle::FileHandle;

use crate::folder::disk::Disk;
use konedrive_tree::Store;

/// A batch is examined when no event came for this long (provisional).
pub const QUIET: Duration = Duration::from_secs(2);
/// ... and at the latest this long after its first event, during continuous
/// activity (provisional).
pub const CEILING: Duration = Duration::from_secs(30);
/// A file that is busy — open for writing, being filled or freed — is
/// examined again after this long (provisional).
pub const RECHECK: Duration = Duration::from_secs(30);

/// The mass-delete guard (§3.4): a batch that would remove more items than
/// this from OneDrive is held for confirmation (provisional)...
pub const MASS_DELETE_ITEMS: u64 = 500;
/// ... or more than this share of the folder's items, in percent
/// (provisional)...
pub const MASS_DELETE_PERCENT: u64 = 20;
/// ... counted only from this many items up, so that removing one file of a
/// folder of four is not a mass delete (provisional; the design is silent).
pub const MASS_DELETE_FLOOR: u64 = 10;

/// Records the inode item `id` was just placed as (`items.local_handle`),
/// by name, opening nothing. A filesystem that gives no handles leaves it
/// unrecorded: such an item is never deleted in OneDrive for being missing
/// (the examination cannot prove it gone), and the folder's watcher, which
/// needs handles, cannot run there anyway.
pub fn record_placed(store: &Store, dir: &File, name: &OsStr, id: &str) {
    match FileHandle::at(dir, name) {
        Ok(handle) => {
            let item = id.to_owned();
            if let Err(e) = store.call_blocking(move |s| s.set_local_handle(&item, Some(&handle))) {
                tracing::warn!("cannot record where {id} was placed: {e}");
            }
        }
        Err(e) => tracing::debug!("no file handle for {}: {e}", name.to_string_lossy()),
    }
}

/// [`record_replaced`] for async code.
pub async fn record_replaced_async(disk: &Disk, store: &Store, id: &str, rel: &Path) {
    let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else { return };
    let Ok(dir) = disk.dir(parent) else { return };
    let there = xattr::get(entry::proc_path(&dir).join(name), konedrive_fs::placeholder::XATTR_ITEM_ID).ok().flatten();
    if there.as_deref() != Some(id.as_bytes()) {
        return;
    }
    match FileHandle::at(&dir, name) {
        Ok(handle) => {
            let item = id.to_owned();
            if let Err(e) = store.call(move |s| s.set_local_handle(&item, Some(&handle))).await {
                tracing::warn!("cannot record where {id} was placed: {e}");
            }
        }
        Err(e) => tracing::debug!("no file handle for {}: {e}", name.to_string_lossy()),
    }
}

/// Records the inode a replacement swapped in for item `id` at `rel`: a new
/// version is a new inode, and the recorded handle must name it, or a move
/// out of the folder would be taken for a delete (its old inode is gone).
/// Only if what stands there now is still the item.
pub fn record_replaced(disk: &Disk, store: &Store, id: &str, rel: &Path) {
    let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else { return };
    let Ok(dir) = disk.dir(parent) else { return };
    let there = xattr::get(entry::proc_path(&dir).join(name), konedrive_fs::placeholder::XATTR_ITEM_ID).ok().flatten();
    if there.as_deref() == Some(id.as_bytes()) {
        record_placed(store, &dir, name, id);
    }
}
