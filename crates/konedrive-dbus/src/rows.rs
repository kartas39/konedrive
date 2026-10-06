//! What the daemon's methods and properties answer with more than one value in a row, by
//! name. Each is the D-Bus structure its interface's XML gives (`dbus/org.konedrive.*.xml`),
//! field for field in the order written here. A client reads every one of them; the daemon
//! answers with [`Conflict`] and [`Event`], and with tuples of the same fields for the rest.

use serde::{Deserialize, Serialize};
use zbus::zvariant::{OwnedValue, Type, Value};

/// A download or an upload under way (`Transfers.Downloads`, `Transfers.Uploads`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type, Value, OwnedValue)]
pub struct Transfer {
    /// The file's full path.
    pub path: String,
    /// Bytes moved so far, and the file's size.
    pub done: u64,
    pub total: u64,
}

/// A change waiting to be uploaded (`UploadQueue.Changes`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Change {
    pub seq: u64,
    pub kind: String,
    /// The full path.
    pub path: String,
    pub state: String,
    /// Bytes sent, and bytes in all.
    pub sent: u64,
    pub total: u64,
    /// Why it waits, as stored; empty when nothing holds it.
    pub reason: String,
    /// Unix seconds of the next try; 0 when none is planned.
    pub next_try: i64,
}

/// An item OneDrive has and the folder cannot hold (`Folder.Skipped`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct NotInFolder {
    /// The full path, as OneDrive has the item.
    pub path: String,
    /// Why the folder cannot hold it, as stored.
    pub reason: String,
    /// Empty for an item that is not on this computer; for one that still
    /// is, what keeps it (`konedrive_reason::WaitsFor`, as stored, its path
    /// a full one).
    pub waits: String,
    /// Empty for an item that is not on this computer; for one that still
    /// is, its full path here.
    pub here: String,
}

/// A file kept back from OneDrive, and why (`UploadQueue.NotUploaded`, and the items of
/// `UploadQueue.NotUploadedFiles`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct KeptBack {
    /// The full path.
    pub path: String,
    /// The reason as stored.
    pub reason: String,
}

/// One reason things are kept back for (`UploadQueue.NotUploadedSummary`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct KeptBackReason {
    /// `one-action`, `per-file`, `never` or `waiting`.
    pub group: String,
    pub reason: String,
    pub count: u32,
    pub bytes: u64,
}

/// The files kept back for one reason (`UploadQueue.NotUploadedFiles`): at most as many as
/// were asked for, and how many there are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct KeptBackFiles {
    pub items: Vec<KeptBack>,
    pub total: u32,
}

/// A local version the folder kept (`Conflicts.List`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Conflict {
    /// Unix seconds.
    pub at: i64,
    /// Where the file was, and where the kept version is: full paths.
    pub original: String,
    pub kept: String,
    /// How it was kept: `rescued` (moved out of the way) or `copy` (beside OneDrive's).
    pub how: String,
}

impl Conflict {
    /// Whether the version was kept as a copy beside its original, inside the folder.
    pub fn is_copy(&self) -> bool {
        self.how == "copy"
    }
}

/// Something that happened in a folder (`ActivityLog.Recent`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Event {
    /// Unix seconds.
    pub at: i64,
    pub kind: String,
    /// The full path.
    pub path: String,
    /// Empty when there is nothing more to say.
    pub detail: String,
}

/// What `Files.FreeUp` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Freed {
    /// Files freed, and their bytes.
    pub files: u32,
    pub bytes: u64,
    /// Files kept because they were in use or changed here.
    pub busy: u32,
    /// Downloaded files kept by a pin below.
    pub pinned: u32,
}

/// What `Folder.FreeUpSpace` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct FreedSpace {
    /// Files freed, and their bytes.
    pub files: u32,
    pub bytes: u64,
    /// Files kept because they were in use.
    pub busy: u32,
}

#[cfg(test)]
mod tests;
