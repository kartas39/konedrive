use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::sync::{Arc, Mutex};


/// A file's identity, the way "per inode" means it: the `(st_dev,
/// st_ino)` pair, read from an open descriptor and never spelled as a name.
/// Two links to one inode share a key; a rename changes no
/// key at all.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct InodeKey {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
}

impl InodeKey {
    /// The identity of an already-open file. Both sides of the lock have a
    /// descriptor by construction: `serve_hydrations` is handed the event
    /// fd, and `SyncService` opens through `SyncRoot::open_inside` *before*
    /// it takes the lock.
    pub fn of(file: &File) -> io::Result<Self> {
        let meta = file.metadata()?;
        Ok(Self { dev: meta.dev(), ino: meta.ino() })
    }
}

/// One inode's slot: the mutex itself, and how many callers are holding or
/// waiting for it.
struct Slot {
    mutex: Arc<tokio::sync::Mutex<()>>,
    /// Incremented before the caller starts waiting and decremented when it
    /// lets go — whether it acquired the lock or was cancelled while parked
    /// (see [`Row`]). The row is removed when this reaches zero.
    users: usize,
}

type LockTable = Arc<Mutex<HashMap<InodeKey, Slot>>>;

/// Serializes hydration and dehydration of the same inode (see the doc
/// comment of `serve_hydrations` in `hydration/server.rs`). A file being
/// hydrated and dehydrated at the same time is a torn file.
///
/// Cheap to hold onto for the life of the daemon: each row is dropped from
/// the table the moment its last user lets go, so the table never grows past
/// the number of inodes genuinely in flight.
///
/// The bookkeeping is an explicit `users` count rather than an
/// `Arc::strong_count` heuristic. The count is exact — every caller adds
/// one before it waits and removes one when it lets go — so "is anyone else
/// using this row?" has an answer that does not depend on how many `Arc`
/// clones a particular code path happens to keep alive, on the drop order of
/// a struct's fields, or on whether a cancelled waiter's in-flight future
/// has been dropped yet. The strong-count form this replaces got the last of
/// those wrong: a waiter whose future was dropped left its row in the table
/// forever (measured), and its threshold could not be raised or lowered by
/// one without either leaking rows or handing two callers different mutexes
/// for the same inode.
#[derive(Clone, Default)]
pub struct InodeLocks {
    inner: LockTable,
}

impl InodeLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Waits for exclusive use of `key`. Held until the returned guard is
    /// dropped.
    pub async fn lock(&self, key: InodeKey) -> InodeGuard {
        let mutex = {
            let mut map = crate::panic::lock(&self.inner);
            let slot = map
                .entry(key)
                .or_insert_with(|| Slot { mutex: Arc::new(tokio::sync::Mutex::new(())), users: 0 });
            slot.users += 1;
            Arc::clone(&slot.mutex)
        };
        // Armed *before* the await, so a caller whose future is dropped
        // while it is parked below still takes itself out of the count.
        let row = Row { table: Arc::clone(&self.inner), key };
        let guard = mutex.lock_owned().await;
        InodeGuard { held: Arc::new(Held { _guard: guard, _row: row }) }
    }

    /// How many inodes the table is tracking. Tests only: the table growing
    /// without bound is the failure mode this number exists to rule out.
    #[cfg(test)]
    fn tracked(&self) -> usize {
        crate::panic::lock(&self.inner).len()
    }

    /// How many callers are holding or waiting for one inode. Tests only,
    /// and specifically so that a test about *two* callers can wait until
    /// the second one has genuinely arrived: guessing with `yield_now` makes
    /// a test that measures the cleanup rule measure nothing at all when the
    /// waiter has not started yet.
    #[cfg(test)]
    fn users(&self, key: InodeKey) -> usize {
        crate::panic::lock(&self.inner).get(&key).map_or(0, |slot| slot.users)
    }
}

/// One caller's place in the count for one inode. Removes itself — and the
/// row, if it was the last — however it goes away.
struct Row {
    table: LockTable,
    key: InodeKey,
}

impl Drop for Row {
    fn drop(&mut self) {
        let mut map = crate::panic::lock(&self.table);
        if let std::collections::hash_map::Entry::Occupied(mut slot) = map.entry(self.key) {
            debug_assert!(slot.get().users > 0, "a row cannot have fewer than one user");
            slot.get_mut().users = slot.get().users.saturating_sub(1);
            if slot.get().users == 0 {
                slot.remove();
            }
        }
    }
}

/// Holds one inode's slot in [`InodeLocks`]. Releases the lock, then leaves
/// the count, when dropped — and when every [`InodeHold`] taken from it is
/// dropped too.
pub struct InodeGuard {
    held: Arc<Held>,
}

/// The lock itself and its place in the count, let go of together.
struct Held {
    // Never read: its entire job is to stay alive, and locked, until this
    // drops. Declared first so the mutex is released before `_row`
    // leaves the count — a waiter woken by that release has already counted
    // itself, so its row cannot be removed from under it either way.
    _guard: tokio::sync::OwnedMutexGuard<()>,
    _row: Row,
}

/// A share in an [`InodeGuard`]: the inode stays locked until this is dropped
/// as well. A blocking section of a fill carries one (`hydration::source`):
/// the section runs to its end on its own thread even when the fill is
/// dropped, and whoever takes the lock next must not find it still writing.
///
/// It can carry something of its holder's that has to last as long as the
/// section does ([`holding_with`]): let go of after the lock.
#[derive(Clone)]
pub struct InodeHold {
    #[allow(dead_code)]
    held: Arc<Held>,
    #[allow(dead_code)]
    with: Option<Carried>,
}

/// What an [`InodeHold`] carries for its holder.
pub(crate) type Carried = Arc<dyn std::any::Any + Send + Sync>;

impl InodeGuard {
    /// A share in the lock, for a blocking section started under it that is
    /// not a fill's (the upload's commit and reads, `upload::steps::sections::blocking_under`).
    pub(crate) fn hold(&self) -> InodeHold {
        InodeHold { held: Arc::clone(&self.held), with: None }
    }
}

tokio::task_local! {
    /// The lock the work running now was started under ([`holding`]).
    static HELD: InodeHold;
}

/// A share in the lock the calling work runs under, if it was started under
/// one ([`holding`], [`unless_removed`]).
pub(crate) fn hold_in_force() -> Option<InodeHold> {
    HELD.try_with(InodeHold::clone).ok()
}

#[cfg(test)]
pub(crate) mod tests;
