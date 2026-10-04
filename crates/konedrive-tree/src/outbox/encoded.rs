//! What two of a row's text columns hold, read and written here and nowhere
//! else: `snapshot` ([`Snapshot`]) and the forms of `target_name`
//! ([`OutboxRow::swap_name`], [`OutboxRow::last_place`]). The columns keep
//! the encoding they have.

use std::fmt;
use std::path::Path;

use super::{OutboxRow, SWAP_PREFIX};

/// What a row's `snapshot` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Snapshot {
    /// `<size> <mtime_ns>` of the content being sent (§3.5). The worker
    /// writes it; the examination compares it to tell a file still being
    /// uploaded from one changed again since.
    Content { size: u64, mtime_ns: i128 },
    /// `moved-out:local`, a `move-out` row's marker: the content was proved
    /// local, so what follows — the attributes taken off, the item deleted
    /// in OneDrive — may run.
    ContentLocal,
    /// `moved-out:trash`: the same for a placeholder in the Trash, removed
    /// without a download.
    Trashed,
}

const CONTENT_LOCAL: &str = "moved-out:local";
const TRASHED: &str = "moved-out:trash";

impl Snapshot {
    /// The snapshot of content of `size` bytes, last changed at
    /// `mtime_sec` and `mtime_nsec`.
    pub fn content(size: u64, mtime_sec: i64, mtime_nsec: i64) -> Self {
        Self::Content { size, mtime_ns: i128::from(mtime_sec) * 1_000_000_000 + i128::from(mtime_nsec) }
    }

    /// What a stored snapshot says; `None` for one that is none of these.
    pub fn parse(stored: &str) -> Option<Self> {
        match stored {
            CONTENT_LOCAL => Some(Self::ContentLocal),
            TRASHED => Some(Self::Trashed),
            _ => {
                let (size, ns) = stored.split_once(' ')?;
                let mtime_ns = ns.parse().ok()?;
                Some(Self::Content { size: size.parse().ok()?, mtime_ns })
            }
        }
    }

    /// Whether it is one of a `move-out` row's two markers.
    pub fn is_marker(self) -> bool {
        matches!(self, Self::ContentLocal | Self::Trashed)
    }
}

impl fmt::Display for Snapshot {
    /// As the column holds it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Content { size, mtime_ns } => write!(f, "{size} {mtime_ns}"),
            Self::ContentLocal => f.write_str(CONTENT_LOCAL),
            Self::Trashed => f.write_str(TRASHED),
        }
    }
}

impl OutboxRow {
    /// What the row's snapshot says.
    pub fn snapshot(&self) -> Option<Snapshot> {
        self.snapshot.as_deref().and_then(Snapshot::parse)
    }

    /// Whether the row's snapshot is exactly `snapshot`, as stored.
    pub fn snapshot_is(&self, snapshot: Snapshot) -> bool {
        self.snapshot.as_deref() == Some(snapshot.to_string().as_str())
    }

    /// The size the row's snapshot says is being sent: what stands before
    /// its first space, whatever follows.
    pub fn snapshot_size(&self) -> Option<u64> {
        self.snapshot.as_deref()?.split(' ').next()?.parse().ok()
    }

    /// The size and the time (Unix seconds) the row's snapshot says were
    /// sent.
    pub fn snapshot_sent(&self) -> Option<(u64, i64)> {
        match self.snapshot()? {
            Snapshot::Content { size, mtime_ns } => Some((size, i64::try_from(mtime_ns.div_euclid(1_000_000_000)).ok()?)),
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
