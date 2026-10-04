//! What a row holds of the content it sends ([`Snapshot`]), as the row has
//! it and as its columns keep it ([`StoredSnapshot`]), and the forms of its
//! `target_name` ([`OutboxRow::swap_name`], [`OutboxRow::last_place`]).

use std::path::Path;

use super::{OutboxRow, SWAP_PREFIX};

/// What a row holds of the content it sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Snapshot {
    /// The size and the time of the content being sent (§3.5). The worker
    /// writes it; the examination compares it to tell a file still being
    /// uploaded from one changed again since.
    Content { size: u64, mtime_ns: i128 },
    /// A `move-out` row's marker: the content was proved local, so what
    /// follows — the attributes taken off, the item deleted in OneDrive —
    /// may run.
    ContentLocal,
    /// The same for a placeholder in the Trash, removed without a download.
    Trashed,
}

/// `moved_out` of a row whose content was proved local.
const CONTENT_LOCAL: &str = "local";
/// `moved_out` of a row whose placeholder is in the Trash.
const TRASHED: &str = "trash";

const NANOSECONDS: i128 = 1_000_000_000;

impl Snapshot {
    /// The snapshot of content of `size` bytes, last changed at
    /// `mtime_sec` and `mtime_nsec`.
    pub fn content(size: u64, mtime_sec: i64, mtime_nsec: i64) -> Self {
        Self::Content { size, mtime_ns: i128::from(mtime_sec) * NANOSECONDS + i128::from(mtime_nsec) }
    }

    /// Whether it is one of a `move-out` row's two markers.
    pub fn is_marker(self) -> bool {
        matches!(self, Self::ContentLocal | Self::Trashed)
    }
}

/// A row's snapshot as its columns keep it: the size and the time of the
/// content being sent, in whole seconds and the nanoseconds past them (so
/// that every time a file can have fits), or a `move-out` row's marker.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct StoredSnapshot {
    pub(super) size: Option<i64>,
    pub(super) mtime: Option<i64>,
    pub(super) mtime_nsec: Option<i64>,
    pub(super) moved_out: Option<String>,
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
                moved_out: None,
            },
            Some(Snapshot::ContentLocal) => Self { moved_out: Some(CONTENT_LOCAL.into()), ..Self::default() },
            Some(Snapshot::Trashed) => Self { moved_out: Some(TRASHED.into()), ..Self::default() },
        }
    }

    /// What the columns say. A marker no konedrive writes is an error, the
    /// word itself: such a row is not run as a guess.
    pub(super) fn read(self) -> Result<Option<Snapshot>, String> {
        match (self.moved_out.as_deref(), self.size, self.mtime) {
            (Some(CONTENT_LOCAL), _, _) => Ok(Some(Snapshot::ContentLocal)),
            (Some(TRASHED), _, _) => Ok(Some(Snapshot::Trashed)),
            (Some(_), _, _) => Err(self.moved_out.unwrap_or_default()),
            (None, Some(size), Some(mtime)) => Ok(Some(Snapshot::Content {
                size: size.max(0) as u64,
                mtime_ns: i128::from(mtime) * NANOSECONDS + i128::from(self.mtime_nsec.unwrap_or(0)),
            })),
            (None, _, _) => Ok(None),
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
        match self.snapshot? {
            Snapshot::Content { size, .. } => Some(size),
            Snapshot::ContentLocal | Snapshot::Trashed => None,
        }
    }

    /// The size and the time (Unix seconds) the row's snapshot says were
    /// sent.
    pub fn snapshot_sent(&self) -> Option<(u64, i64)> {
        match self.snapshot? {
            Snapshot::Content { size, mtime_ns } => Some((size, i64::try_from(mtime_ns.div_euclid(NANOSECONDS)).ok()?)),
            Snapshot::ContentLocal | Snapshot::Trashed => None,
        }
    }

    /// The temporary name the row is taking its item through in OneDrive
    /// (§4.4, F55 (7)), while its `target_name` is one.
    pub fn swap_name(&self) -> Option<&str> {
        self.target_name.as_deref().filter(|name| name.starts_with(SWAP_PREFIX))
    }

    /// Where a `move-out` row's object was last proved to be: its
    /// `target_name`, while that is an absolute path.
    pub fn last_place(&self) -> Option<&Path> {
        self.target_name.as_deref().map(Path::new).filter(|path| path.is_absolute())
    }
}

/// A place as a `move-out` row's `target_name` keeps it
/// ([`OutboxRow::last_place`]): a path that is not UTF-8 is not kept.
pub fn place_name(place: &Path) -> Option<&str> {
    place.to_str()
}

#[cfg(test)]
mod tests;
