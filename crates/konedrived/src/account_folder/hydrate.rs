//! `Hydrate`: a file downloaded now, by the folder's mount.

use std::path::Path;
use std::sync::Arc;

use crate::mount::{FilesRefusal, ItemMount};
use crate::account_folder::{SyncError, SyncService};

impl From<FilesRefusal> for SyncError {
    fn from(refused: FilesRefusal) -> Self {
        match &refused {
            FilesRefusal::OutsideFolder => SyncError::OutsideRoot,
            FilesRefusal::NotInOneDrive => SyncError::NotManaged,
            FilesRefusal::Io(why) => SyncError::Io(why.clone()),
            FilesRefusal::ThrottleWait { .. } => SyncError::Throttled(refused.to_string()),
        }
    }
}

impl SyncService {
    /// The folder's mount, for a call that needs it; refused for an account with no folder,
    /// and with why for a folder that is not mounted: it is not up, or its sync never started.
    pub(super) fn require_mount(&self) -> Result<Arc<ItemMount>, SyncError> {
        self.require_record()?;
        self.mount().ok_or_else(|| self.sync_not_running())
    }

    /// Downloads the file at `path` now, and waits for it: the download any read of the file
    /// through the mount would start, asked for by name. A file that is downloaded already is
    /// left as it is.
    pub async fn hydrate_now(&self, path: &Path) -> Result<(), SyncError> {
        Ok(self.require_mount()?.files_interface().download(path).await?)
    }
}
