use std::path::Path;
use std::sync::Arc;

use crate::sync::SyncService;
use crate::sync::SyncError;
use crate::hydration::pin::state_of_path;
use crate::status::activity;

impl SyncService {
    /// `Skipped()`: every item not in the folder whose own folder is, as a
    /// full path and a reason.
    ///
    /// The store is read with `lifecycle` held for reading (see `store`), so
    /// a Forget waits for a read under way rather than remove the files
    /// under it. The lock goes into the blocking task with the store's clone,
    /// so it is held as long as the clone is, even when this call is dropped
    /// part-way.
    pub async fn skipped(&self) -> Result<Vec<(String, String)>, SyncError> {
        let lifecycle = Arc::clone(&self.lifecycle).read_owned().await;
        let Some(reg) = self.registration() else { return Ok(Vec::new()) };
        let Some(store) = self.store.lock().unwrap().clone() else { return Ok(Vec::new()) };
        let skipped = tokio::task::spawn_blocking(move || {
            let _lifecycle = lifecycle;
            // One query, on the read-only connection (issue #39): the cycle's
            // work is not held up behind it.
            store.read_blocking(|s| s.skipped())
        })
        .await
        .map_err(|e| SyncError::Io(format!("the store task failed: {e}")))?
        .map_err(|e| SyncError::Io(e.to_string()))?;
        Ok(skipped
            .into_iter()
            .map(|(rel, reason)| (reg.root.path.join(rel).display().to_string(), reason.as_str().to_owned()))
            .collect())
    }

    /// `ItemsListed`, `ItemsPlaced`, `SkippedCount`.
    pub fn items(&self) -> (u64, u64, u64) {
        let s = self.state.get();
        (s.items_listed, s.items_placed, s.skipped_count)
    }

    /// `ActivityLog.Recent(limit)`: the newest `limit` events, newest first.
    pub async fn recent_activity(&self, limit: u32) -> Result<Vec<activity::Event>, SyncError> {
        let report = self.report.clone();
        tokio::task::spawn_blocking(move || report.activity.recent(limit as usize))
            .await
            .map_err(|e| SyncError::Io(format!("the activity task failed: {e}")))?
            .map_err(|e| SyncError::Io(e.to_string()))
    }

    /// `Conflicts.List()`: (time, original, rescued), newest first; one whose
    /// rescued file is gone is dropped on the way.
    pub async fn conflicts(&self) -> Result<Vec<konedrive_tree::ConflictRow>, SyncError> {
        let report = self.report.clone();
        tokio::task::spawn_blocking(move || report.activity.conflicts())
            .await
            .map_err(|e| SyncError::Io(format!("the conflicts task failed: {e}")))?
            .map_err(|e| SyncError::Io(e.to_string()))
    }

    /// `Conflicts.Dismiss(rescued_path)`: the conflict comes off the list, and
    /// the file stays where it is. A path that names no conflict is refused
    /// with that path in the refusal.
    pub async fn dismiss_conflict(&self, rescued: &str) -> Result<(), SyncError> {
        let (report, path) = (self.report.clone(), rescued.to_owned());
        let removed = tokio::task::spawn_blocking(move || report.activity.dismiss(&path))
            .await
            .map_err(|e| SyncError::Io(format!("the conflicts task failed: {e}")))?
            .map_err(|e| SyncError::Io(e.to_string()))?;
        if removed {
            Ok(())
        } else {
            Err(SyncError::NoConflict(rescued.to_owned()))
        }
    }

    /// `LastChecked`, `LocalBytes`, `Conflicts.Count`.
    pub fn status(&self) -> (i64, u64, u32) {
        let s = self.state.get();
        (s.last_checked, s.local_bytes, s.conflict_count)
    }

    /// `Transfers`: every download under way, as (path, bytes done, total).
    pub fn transfers(&self) -> Vec<(String, u64, u64)> {
        self.report.transfers.list().into_iter().map(|t| (t.path, t.done, t.total)).collect()
    }

    /// `Transfers.LargeFiles` (issue #50): the large files the sync moves now, each once, the
    /// files being opened left out ([`activity::large_files`]).
    pub fn large_files(&self) -> u32 {
        let downloads = self.report.transfers.subscribe().borrow().clone();
        activity::large_files(&downloads, &self.state.get().uploads)
    }

    /// `Files.WebUrl`: the address of the page OneDrive's web interface has for
    /// the file or folder at `path`. The path is opened as a pin's is
    /// (`SyncRoot::open_item`: beneath the root, no link followed, nothing
    /// downloaded) for its item id, and OneDrive is asked for that item — one
    /// GET, with the drive client's own retries. Nothing is changed, here or
    /// in OneDrive, and nothing is remembered.
    ///
    /// Refused `NotInOneDrive` for an item with no id (not uploaded yet),
    /// `NotSignedIn` with no drive or no token, `Unreachable` when OneDrive
    /// does not answer.
    pub async fn web_url(&self, path: &Path) -> Result<String, SyncError> {
        let reg = self.require_registration()?;
        let (root, target) = (reg.root.clone(), path.to_path_buf());
        let (id, shown) = tokio::task::spawn_blocking(move || -> Result<_, SyncError> {
            let (item, shown) = root.open_item(&target)?;
            let id = konedrive_fs::placeholder::read_item_id(&item)
                .map_err(|e| SyncError::Io(format!("{}: {e}", shown.display())))?;
            Ok((id, shown.display().to_string()))
        })
        .await
        .map_err(|e| SyncError::Io(format!("reading the item failed: {e}")))??;
        let Some(id) = id else { return Err(SyncError::NotInOneDrive(shown)) };
        let drive = self.drive.lock().unwrap().clone().ok_or(SyncError::NotSignedIn)?;
        page_of(drive.item(&id).await, &shown)
    }

    /// `Files.WebUrl` of the account's folder itself: the address of the page
    /// of the drive's root. One GET, as [`web_url`](Self::web_url).
    pub async fn root_web_url(&self) -> Result<String, SyncError> {
        let reg = self.require_registration()?;
        let drive = self.drive.lock().unwrap().clone().ok_or(SyncError::NotSignedIn)?;
        page_of(drive.root_item().await, &reg.root.path.display().to_string())
    }

    /// The file's own state, or `not-managed` for anything that is not a
    /// plain file this daemon actually manages inside the current root —
    /// including a file outside the root altogether, per.
    ///
    /// # A query never opens the file
    ///
    /// The state is read with `lgetxattr` on the path. Opening the file
    /// instead made `ItemState` a *download*: under a marked directory, the
    /// open of an `online-only` file is intercepted and the whole file is
    /// fetched as a side effect of asking what state it is in — and where
    /// nothing can serve it, the denial makes the open fail and this answers
    /// `not-managed` for a genuinely managed placeholder, which is simply a
    /// wrong answer. Opening it also blocks: `ItemState` on a FIFO inside
    /// the folder parked the D-Bus dispatch task forever.
    pub async fn item_state(&self, path: &Path) -> String {
        const NOT_MANAGED: &str = "not-managed";
        let Some(reg) = self.registration() else {
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
        Err(DriveError::Transient(why)) => Err(SyncError::Unreachable(why)),
        Err(DriveError::NotFound) => Err(SyncError::Io(format!("{shown} is not in OneDrive any more"))),
        Err(other) => Err(SyncError::Io(format!("asking OneDrive for the page of {shown}: {other}"))),
    }
}
