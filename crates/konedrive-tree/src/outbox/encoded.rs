//! What a row holds of the content it sends ([`Snapshot`]), as the row has
//! it and as its columns keep it ([`StoredSnapshot`](crate::outbox::encoded::StoredSnapshot)), and the temporary
//! name in its `target_name` ([`OutboxRow::swap_name`]).

use super::{OutboxRow, SWAP_PREFIX};

/// What a row holds of the content it sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Snapshot {
    /// The size and the time of the content being sent (`docs/design/writes.md` §6.3). The worker
    /// writes it; the examination compares it to tell a file still being
    /// uploaded from one changed again since.
    Content { size: u64, mtime_ns: i128 },
}

const NANOSECONDS: i128 = 1_000_000_000;

impl Snapshot {
    /// The snapshot of content of `size` bytes, last changed at
    /// `mtime_sec` and `mtime_nsec`.
    pub fn content(size: u64, mtime_sec: i64, mtime_nsec: i64) -> Self {
        Self::Content { size, mtime_ns: i128::from(mtime_sec) * NANOSECONDS + i128::from(mtime_nsec) }
    }
}

/// A row's snapshot as its columns keep it: the size and the time of the
/// content being sent, in whole seconds and the nanoseconds past them (so
/// that every time a file can have fits).
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct StoredSnapshot {
    pub(super) size: Option<i64>,
    pub(super) mtime: Option<i64>,
    pub(super) mtime_nsec: Option<i64>,
}

impl StoredSnapshot {
    pub(super) fn of(snapshot: Option<Snapshot>) -> Self {
        match snapshot {
            None => Self::default(),
            Some(Snapshot::Content { size, mtime_ns }) => Self {
                size: Some(i64::try_from(size).unwrap_or(i64::MAX)),
                // A time built from seconds that fit always fits again.
                mtime: Some(i64::try_from(mtime_ns.div_euclid(NANOSECONDS)).unwrap_or(if mtime_ns < 0 { i64::MIN } else { i64::MAX })),
                mtime_nsec: Some(mtime_ns.rem_euclid(NANOSECONDS) as i64),
            },
        }
    }

    /// What the columns say.
    pub(super) fn read(self) -> Option<Snapshot> {
        match (self.size, self.mtime) {
            (Some(size), Some(mtime)) => Some(Snapshot::Content {
                size: size.max(0) as u64,
                mtime_ns: i128::from(mtime) * NANOSECONDS + i128::from(self.mtime_nsec.unwrap_or(0)),
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
        let Snapshot::Content { size, mtime_ns } = self.snapshot?;
        Some((size, i64::try_from(mtime_ns.div_euclid(NANOSECONDS)).ok()?))
    }

    /// The temporary name the row is taking its item through in OneDrive
    /// (§5.3), while its `target_name` is one.
    pub fn swap_name(&self) -> Option<&str> {
        self.target_name.as_deref().filter(|name| name.starts_with(SWAP_PREFIX))
    }
}

#[cfg(test)]
mod tests;
