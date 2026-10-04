//! The store as its read-only connection gives it: the lists and sums the
//! bus asks for, and nothing that writes.

use std::path::{Path, PathBuf};

use crate::conflicts::ConflictRow;
use crate::model::SkipReason;
use crate::outbox::{LocalSkip, LocalSkipped, OutboxGroup, OutboxRow, SkippedGroup};
use crate::{TreeError, TreeStore};

/// What a job of [`Store::read`](crate::Store::read) is handed: the store
/// for reading alone. It sees what was last committed and never waits for
/// a writer (issue #38); a call that changes the store is not among its
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

    /// [`TreeStore::skipped`].
    pub fn skipped(&self) -> Result<Vec<(PathBuf, SkipReason)>, TreeError> {
        self.store.skipped()
    }

    /// [`TreeStore::conflicts`].
    pub fn conflicts(&self) -> Result<Vec<ConflictRow>, TreeError> {
        self.store.conflicts()
    }

    /// [`TreeStore::conflicts_after`].
    pub fn conflicts_after(&self, after: &str, limit: usize) -> Result<Vec<ConflictRow>, TreeError> {
        self.store.conflicts_after(after, limit)
    }

    /// [`TreeStore::outbox_rows`].
    pub fn outbox_rows(&self) -> Result<Vec<OutboxRow>, TreeError> {
        self.store.outbox_rows()
    }

    /// [`TreeStore::outbox_first`].
    pub fn outbox_first(&self, limit: usize) -> Result<Vec<OutboxRow>, TreeError> {
        self.store.outbox_first(limit)
    }

    /// [`TreeStore::outbox_blocked`].
    pub fn outbox_blocked(&self) -> Result<Vec<OutboxRow>, TreeError> {
        self.store.outbox_blocked()
    }

    /// [`TreeStore::outbox_len`].
    pub fn outbox_len(&self) -> Result<usize, TreeError> {
        self.store.outbox_len()
    }

    /// [`TreeStore::outbox_groups`].
    pub fn outbox_groups(&self) -> Result<Vec<OutboxGroup>, TreeError> {
        self.store.outbox_groups()
    }

    /// [`TreeStore::outbox_places_of`].
    pub fn outbox_places_of(&self, groups: &[&OutboxGroup], limit: u32) -> Result<Vec<(PathBuf, usize)>, TreeError> {
        self.store.outbox_places_of(groups, limit)
    }

    /// [`TreeStore::local_skipped`].
    pub fn local_skipped(&self) -> Result<Vec<LocalSkipped>, TreeError> {
        self.store.local_skipped()
    }

    /// [`TreeStore::skipped_groups`].
    pub fn skipped_groups(&self) -> Result<Vec<SkippedGroup>, TreeError> {
        self.store.skipped_groups()
    }

    /// [`TreeStore::skipped_places_of`].
    pub fn skipped_places_of(&self, reasons: &[&LocalSkip], limit: u32) -> Result<Vec<(PathBuf, LocalSkip)>, TreeError> {
        self.store.skipped_places_of(reasons, limit)
    }
}
