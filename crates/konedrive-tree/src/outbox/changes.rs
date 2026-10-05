//! What changed in the outbox, told to those waiting for a change.

use std::collections::HashSet;

use rusqlite::Connection;

use crate::TreeError;

/// How many changed rows are remembered one by one; past it, a reader of
/// the changes looks at every row once.
const DIRTY_MAX: usize = 100_000;

/// What changed in the outbox (issue #38), told by SQLite's update hook on
/// the store's connection: a count of changes to `outbox` and `local_skipped`,
/// the `seq` of each outbox row written or removed since last asked, and a
/// signal once the change is committed ([`Store`](crate::Store), whose thread sends it).
#[derive(Debug)]
pub struct OutboxChanges {
    generation: std::sync::atomic::AtomicU64,
    dirty: std::sync::Mutex<(HashSet<i64>, bool)>,
    committed: tokio::sync::watch::Sender<u64>,
}

impl Default for OutboxChanges {
    fn default() -> Self {
        Self { generation: Default::default(), dirty: Default::default(), committed: tokio::sync::watch::channel(0).0 }
    }
}

impl OutboxChanges {
    /// Changes so far.
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The changed rows, locked; also after a holder of the lock panicked: each change of
    /// them is one insert, one flag set or one take, so they are whole either way.
    fn dirty(&self) -> std::sync::MutexGuard<'_, (HashSet<i64>, bool)> {
        self.dirty.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn touched(&self, seq: Option<i64>) {
        self.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(seq) = seq {
            let mut dirty = self.dirty();
            if dirty.0.len() < DIRTY_MAX {
                dirty.0.insert(seq);
            } else {
                dirty.1 = true;
            }
        }
    }

    /// The outbox rows written or removed since the last call: `None` when
    /// too many to remember (look at them all).
    pub fn take_dirty(&self) -> Option<HashSet<i64>> {
        let mut dirty = self.dirty();
        let (seqs, overflow) = std::mem::take(&mut *dirty);
        (!overflow).then_some(seqs)
    }

    /// Tells the subscribers a change is committed.
    pub(crate) fn committed(&self) {
        self.committed.send_replace(self.generation());
    }

    /// Changed whenever a change to the outbox is committed through the shared store.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.committed.subscribe()
    }
}

/// Hooks `changes` to the connection's writes.
pub(crate) fn watch(conn: &Connection, changes: &std::sync::Arc<OutboxChanges>) -> Result<(), TreeError> {
    let changes = std::sync::Arc::clone(changes);
    conn.update_hook(Some(move |_: rusqlite::hooks::Action, _: &str, table: &str, rowid: i64| match table {
        "outbox" => changes.touched(Some(rowid)),
        "local_skipped" => changes.touched(None),
        _ => {}
    }))?;
    Ok(())
}
