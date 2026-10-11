// The skeleton of the switch: bodies that drove the old mechanism are `todo!()`.
#![allow(dead_code, unused_variables, unused_imports, unused_mut)]
use std::path::Path;
use std::sync::Arc;

use crate::account_folder::SyncService;
use crate::account_folder::SyncError;
use crate::status::activity;

impl SyncService {
    /// `Skipped()`: every item not in the folder whose own folder is, as a
    /// full path and a reason.
    ///
    /// The store is the running sync's, read with the folder's state held for reading, so
    /// a Forget waits for a read under way rather than remove the files
    /// under it. The lock goes into the blocking task with the store's clone,
    /// so it is held as long as the clone is, even when this call is dropped
    /// part-way.
    pub async fn skipped(&self) -> Result<Vec<(String, String, String, String)>, SyncError> {
        // TODO: `upload_queue::read_items_not_shown`, in a read of the folder's store (`View::parts`: `Store::read_items`), over `PublishedPlan::list_answers` — empty with no plan published yet.
        todo!()
    }

    /// `ItemsListed`, `ItemsPlaced`, `SkippedCount`, for the VM suite's listing of a real
    /// drive and the tests.
    pub fn items(&self) -> (u64, u64, u64) {
        let s = self.state.get();
        (s.cycle.items_listed, s.cycle.items_placed, s.cycle.skipped_count)
    }

    /// `ActivityLog.Recent(limit)`: the newest `limit` events, newest first.
    pub async fn recent_activity(&self, limit: u32) -> Result<Vec<activity::Event>, SyncError> {
        let report = self.report.clone();
        tokio::task::spawn_blocking(move || report.activity.recent(limit as usize))
            .await
            .map_err(|e| SyncError::Io(format!("the activity task failed: {e}")))?
            .map_err(|e| SyncError::Store(e.to_string()))
    }

    /// `Conflicts.List()`: (time, original, rescued), newest first; one whose
    /// rescued file is gone is dropped on the way.
    pub async fn conflicts(&self) -> Result<Vec<konedrive_tree::ConflictRow>, SyncError> {
        let report = self.report.clone();
        tokio::task::spawn_blocking(move || report.activity.conflicts())
            .await
            .map_err(|e| SyncError::Io(format!("the conflicts task failed: {e}")))?
            .map_err(|e| SyncError::Store(e.to_string()))
    }

    /// `Conflicts.Dismiss(rescued_path)`: the conflict comes off the list, and
    /// the file stays where it is. A path that names no conflict is refused
    /// with that path in the refusal.
    pub async fn dismiss_conflict(&self, rescued: &str) -> Result<(), SyncError> {
        let (report, path) = (self.report.clone(), rescued.to_owned());
        let removed = tokio::task::spawn_blocking(move || report.activity.dismiss(&path))
            .await
            .map_err(|e| SyncError::Io(format!("the conflicts task failed: {e}")))?
            .map_err(|e| SyncError::Store(e.to_string()))?;
        if removed {
            Ok(())
        } else {
            Err(SyncError::NoConflict(rescued.to_owned()))
        }
    }

    /// `Files.WebUrl`: the address of the page OneDrive's web interface has for
    /// the file or folder at `path`. The folder's mount says which
    /// item the path is — nothing is opened, so nothing is downloaded — and
    /// OneDrive is asked for that item: one GET, with the drive client's own retries. Nothing is changed, here or
    /// in OneDrive, and nothing is remembered.
    ///
    /// Refused `NotInOneDrive` for an item with no id (not uploaded yet),
    /// `NotSignedIn` with no drive or no token, `Unreachable` when OneDrive
    /// does not answer.
    pub async fn web_url(&self, path: &Path) -> Result<String, SyncError> {
        // TODO: `ItemFilesInterface::find_path_status` (the mount's `item_files_interface`): `ItemPathStatus::onedrive_item` is what OneDrive is asked by.
        todo!()
    }

    /// `Files.WebUrl` of the account's folder itself: the address of the page
    /// of the drive's root. One GET, as [`web_url`](Self::web_url).
    pub async fn root_web_url(&self) -> Result<String, SyncError> {
        let reg = self.require_record()?;
        let drive = self.drive().ok_or(SyncError::NotSignedIn)?;
        page_of(drive.root_item().await, &reg.root.path.display().to_string())
    }

    /// The file's own state, or `not-managed` for anything that is not a
    /// plain file this daemon actually manages inside the current root —
    /// including a file outside the root altogether.
    ///
    /// # A query never opens the file
    ///
    /// The state is read with `lgetxattr` on the path, which the folder's mount
    /// answers: opening the file instead would start its download, and
    /// `ItemState` on a FIFO inside the folder parked the D-Bus dispatch task
    /// forever.
    pub async fn item_state(&self, path: &Path) -> String {
        const NOT_MANAGED: &str = "not-managed";
        let Some(reg) = self.record() else {
            return NOT_MANAGED.into();
        };
        let path = path.to_path_buf();
        // `canonicalize` and `getxattr` are blocking syscalls
        // and do not belong on the zbus dispatch task.
        tokio::task::spawn_blocking(move || {
            let Ok(canonical) = std::fs::canonicalize(&path) else {
                return NOT_MANAGED.to_owned();
            };
            if !canonical.starts_with(&reg.root.path) {
                return NOT_MANAGED.to_owned();
            }
            match state_of_path(&canonical) {
                Some(state) => state.as_str().to_owned(),
                None => NOT_MANAGED.to_owned(),
            }
        })
        .await
        .unwrap_or_else(|_| NOT_MANAGED.to_owned())
    }
}

/// The page's address out of OneDrive's answer about the item shown as `shown`.
fn page_of(answer: Result<konedrive_graph::drive::DriveItem, konedrive_graph::drive::DriveError>, shown: &str) -> Result<String, SyncError> {
    use konedrive_graph::drive::DriveError;
    match answer {
        Ok(item) => item
            .web_url
            .filter(|url| !url.is_empty())
            .ok_or_else(|| SyncError::Io(format!("OneDrive gave no address for the page of {shown}"))),
        Err(DriveError::SignedOut) => Err(SyncError::NotSignedIn),
        Err(DriveError::Transient(why)) => Err(SyncError::Unreachable(why.message)),
        Err(DriveError::NotFound) => Err(SyncError::Io(format!("{shown} is not in OneDrive any more"))),
        Err(other) => Err(SyncError::Io(format!("asking OneDrive for the page of {shown}: {other}"))),
    }
}

/// One file's state read by name, with no open at all.
/// `xattr::get` is `lgetxattr`: it does not follow a final symlink, and the
/// path it is given has already been canonicalized.
fn state_of_path(path: &Path) -> Option<konedrive_fs::placeholder::State> {
    let raw = xattr::get(path, konedrive_fs::placeholder::XATTR_STATE).ok()??;
    String::from_utf8_lossy(&raw).parse().ok()
}
