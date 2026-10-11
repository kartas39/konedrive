//! The store as its read-only connection gives it: the lists and sums the
//! bus asks for, and nothing that writes.

use std::path::Path;

use crate::conflicts::ConflictRow;
use crate::{TreeError, TreeStore};

/// What a job of [`Store::read`](crate::Store::read) is handed: the store
/// for reading alone. It sees what was last committed and never waits for
/// a writer; a call that changes the store is not among its
/// methods, so one cannot be sent to the read-only connection by mistake.
pub struct ReadStore<'a> {
    store: &'a TreeStore,
}

impl<'a> ReadStore<'a> {
    pub(crate) fn of(store: &'a TreeStore) -> Self {
        Self { store }
    }

    /// Opens the store at `path` for reading alone and asks it `f`: for a
    /// folder whose store no thread of this process owns. Nothing is
    /// created or changed.
    pub fn at<T>(path: &Path, f: impl FnOnce(&ReadStore<'_>) -> Result<T, TreeError>) -> Result<T, TreeError> {
        let store = TreeStore::open_read_only(path)?;
        f(&ReadStore::of(&store))
    }

    /// [`TreeStore::conflicts`].
    pub fn conflicts(&self) -> Result<Vec<ConflictRow>, TreeError> {
        self.store.conflicts()
    }

    /// [`TreeStore::conflicts_after`].
    pub fn conflicts_after(&self, after: &str, limit: usize) -> Result<Vec<ConflictRow>, TreeError> {
        self.store.conflicts_after(after, limit)
    }
}
