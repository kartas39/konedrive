use std::path::Path;

use crate::account_folder::SyncService;
use crate::account_folder::SyncError;
use crate::status::activity;
use crate::upload_queue;

impl SyncService {
    /// `Skipped()`: every item not in the folder whose own folder is, as a
    /// full path and a reason.
    ///
    /// The store is the folder's as last published (`View::parts`), read on its reading
    /// connection: a Forget waits, with a bound, until every clone of the store is let go
    /// of before it removes the files, this read's among them. Empty for an account with
    /// no folder and for a folder that keeps no open store.
    pub async fn skipped(&self) -> Result<Vec<(String, String, String, String)>, SyncError> {
        let Some(record) = self.record() else { return Ok(Vec::new()) };
        let Some(parts) = self.view().parts else { return Ok(Vec::new()) };
        // The answers of the last plan, taken before the read; none with no plan published yet.
        let answers = parts.published_plan.list_answers();
        let folder = record.root.path;
        let not_shown = parts
            .store
            .read_items(move |items| upload_queue::read_items_not_shown(items, &answers, &folder))
            .await
            .map_err(|e| SyncError::Store(e.to_string()))?;
        Ok(not_shown.into_iter().map(|row| (row.path, row.reason, row.waits, row.here)).collect())
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

    /// `Conflicts.List()`: (time, original, the kept copy), newest first; one whose
    /// kept copy is gone is dropped on the way.
    pub async fn conflicts(&self) -> Result<Vec<konedrive_tree::ConflictRow>, SyncError> {
        let report = self.report.clone();
        tokio::task::spawn_blocking(move || report.activity.conflicts())
            .await
            .map_err(|e| SyncError::Io(format!("the conflicts task failed: {e}")))?
            .map_err(|e| SyncError::Store(e.to_string()))
    }

    /// `Conflicts.Dismiss(kept_path)`: the conflict comes off the list, and
    /// the file stays where it is. A path that names no conflict is refused
    /// with that path in the refusal.
    pub async fn dismiss_conflict(&self, kept_copy: &str) -> Result<(), SyncError> {
        let (report, path) = (self.report.clone(), kept_copy.to_owned());
        let removed = tokio::task::spawn_blocking(move || report.activity.dismiss(&path))
            .await
            .map_err(|e| SyncError::Io(format!("the conflicts task failed: {e}")))?
            .map_err(|e| SyncError::Store(e.to_string()))?;
        if removed {
            Ok(())
        } else {
            Err(SyncError::NoConflict(kept_copy.to_owned()))
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
        // Never opened: an open through the mount would download the file.
        let status = match self.require_mount()?.files_interface().find_path_status(path).await {
            Ok(status) => status,
            Err(crate::mount::FilesRefusal::NotInOneDrive) => return Err(SyncError::NotInOneDrive(path.display().to_string())),
            Err(refused) => return Err(refused.into()),
        };
        let drive = self.drive().ok_or(SyncError::NotSignedIn)?;
        page_of(drive.item(status.onedrive_item.item_id.as_str()).await, &path.display().to_string())
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
