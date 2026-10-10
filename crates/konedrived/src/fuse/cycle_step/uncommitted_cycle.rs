//! What a cycle of the delta listing that is not committed yet leaves to be done once it is.

use super::onedrive_change::OneDriveChange;
use crate::fuse::inotify_bridge::Announcement;

/// What the cycle now being committed leaves to be done once it is committed. Only the last
/// cycle applied leaves anything: what an earlier one left was never committed.
#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::fuse) struct UncommittedCycle {
    /// What is announced to whoever watches the folder.
    pub(in crate::fuse) announcements: Vec<Announcement>,
    /// The OneDrive ids gone from the folder: one that is gone keeps nothing on this device,
    /// so its pin is taken off.
    pub(in crate::fuse) removed_ids: Vec<String>,
    /// The OneDrive ids of the files the cycle shows anew, moved or gave another content
    /// whose whole copy the backing directory holds at the file's place once the step is
    /// done: what the cycle's commit writes as on disk in the rows of `items`.
    pub(in crate::fuse) copies_on_disk: Vec<String>,
}

/// The OneDrive ids `changes` took out of the folder.
pub(in crate::fuse) fn list_removed_ids(changes: &[OneDriveChange]) -> Vec<String> {
    let removed = changes.iter().filter_map(|change| match change {
        OneDriveChange::Vanished { onedrive_id, .. } => Some(onedrive_id.clone()),
        OneDriveChange::Appeared { .. } | OneDriveChange::Moved { .. } | OneDriveChange::Stayed { .. } => None,
    });
    removed.collect()
}
