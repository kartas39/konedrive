//! What a row holds of the content it sends ([`Snapshot`]), as the row has
//! it and as its columns keep it ([`StoredSnapshot`](crate::outbox::encoded::StoredSnapshot)), and the temporary
//! name in its `target_name` ([`OutboxRow::swap_name`]).

use super::{OutboxRow, SWAP_PREFIX};

/// What a row holds of the content it sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Snapshot {
    /// The size and the time of the content being sent (`docs/design/writes.md` §6.3). The worker
    /// writes it; the examination compares it to tell a file still being
    /// uploaded from one changed again since. `change_count` is that of the
    /// local content of the row's item when the snapshot was taken
    /// ([`ContentVersion::ChangedHere`](crate::ContentVersion::ChangedHere); 0 for a hashed
    /// content), which tells content of one size and time from the content before a program
    /// opened the file for writing; `None` where the snapshot was taken with no item's row
    /// at hand.
    Content { size: u64, mtime_ns: i128, change_count: Option<u64> },
}

const NANOSECONDS: i128 = 1_000_000_000;

impl Snapshot {
    /// The snapshot of content of `size` bytes, last changed at
    /// `mtime_sec` and `mtime_nsec`, with no change count.
    pub fn content(size: u64, mtime_sec: i64, mtime_nsec: i64) -> Self {
        Self::Content { size, mtime_ns: i128::from(mtime_sec) * NANOSECONDS + i128::from(mtime_nsec), change_count: None }
    }

    /// The same snapshot, of content whose change count is `change_count`.
    pub fn with_change_count(self, change_count: u64) -> Self {
        let Self::Content { size, mtime_ns, .. } = self;
        Self::Content { size, mtime_ns, change_count: Some(change_count) }
    }
}

/// A row's snapshot as its columns keep it: the size and the time of the
/// content being sent, in whole seconds and the nanoseconds past them (so
/// that every time a file can have fits), and its change count.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct StoredSnapshot {
    pub(super) size: Option<i64>,
    pub(super) mtime: Option<i64>,
    pub(super) mtime_nsec: Option<i64>,
    pub(super) change_count: Option<i64>,
}

impl StoredSnapshot {
    pub(super) fn of(snapshot: Option<Snapshot>) -> Self {
        match snapshot {
            None => Self::default(),
            Some(Snapshot::Content { size, mtime_ns, change_count }) => Self {
                size: Some(i64::try_from(size).unwrap_or(i64::MAX)),
                // A time built from seconds that fit always fits again.
                mtime: Some(i64::try_from(mtime_ns.div_euclid(NANOSECONDS)).unwrap_or(if mtime_ns < 0 { i64::MIN } else { i64::MAX })),
                mtime_nsec: Some(mtime_ns.rem_euclid(NANOSECONDS) as i64),
                change_count: change_count.map(|count| i64::try_from(count).unwrap_or(i64::MAX)),
            },
        }
    }

    /// What the columns say.
    pub(super) fn read(self) -> Option<Snapshot> {
        match (self.size, self.mtime) {
            (Some(size), Some(mtime)) => Some(Snapshot::Content {
                size: size.max(0) as u64,
                mtime_ns: i128::from(mtime) * NANOSECONDS + i128::from(self.mtime_nsec.unwrap_or(0)),
                change_count: self.change_count.map(|count| count.max(0) as u64),
            }),
            _ => None,
        }
    }
}

impl OutboxRow {
    /// What the row holds of the content it sends.
    pub fn snapshot(&self) -> Option<Snapshot> {
        self.snapshot
    }

    /// Whether the row's snapshot is exactly `snapshot`.
    pub fn snapshot_is(&self, snapshot: Snapshot) -> bool {
        self.snapshot == Some(snapshot)
    }

    /// The size the row's snapshot says is being sent.
    pub fn snapshot_size(&self) -> Option<u64> {
        let Snapshot::Content { size, .. } = self.snapshot?;
        Some(size)
    }

    /// The size and the time (Unix seconds) the row's snapshot says were
    /// sent.
    pub fn snapshot_sent(&self) -> Option<(u64, i64)> {
        let Snapshot::Content { size, mtime_ns, .. } = self.snapshot?;
        Some((size, i64::try_from(mtime_ns.div_euclid(NANOSECONDS)).ok()?))
    }

    /// The change count the local content of the row's item had when the row's snapshot
    /// was taken; `None` for a snapshot taken with none, and for no snapshot.
    pub fn snapshot_change_count(&self) -> Option<u64> {
        let Snapshot::Content { change_count, .. } = self.snapshot?;
        change_count
    }

    /// The temporary name the row is taking its item through in OneDrive
    /// (§5.3), while its `target_name` is one.
    pub fn swap_name(&self) -> Option<&str> {
        self.target_name.as_deref().filter(|name| name.starts_with(SWAP_PREFIX))
    }
}

#[cfg(test)]
mod tests;
