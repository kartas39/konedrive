use std::path::Path;
use std::sync::Arc;

use zbus::interface;

use crate::dbus::fault::{to_fault, SyncFault};
use crate::daemon::manager::AccountManager;

pub(crate) fn outside(path: &str) -> SyncFault {
    SyncFault::OutsideRoot(format!("{path} is in no account's folder"))
}

/// `org.konedrive.Files` (`dbus/org.konedrive.Files.xml`): the per-file calls, each
/// routed by path to the account whose folder holds it.
pub struct Files {
    pub(crate) manager: Arc<AccountManager>,
}


#[interface(name = "org.konedrive.Files")]
impl Files {
    async fn hydrate(&self, path: &str) -> Result<(), SyncFault> {
        let account = self.manager.route(Path::new(path)).await.ok_or_else(|| outside(path))?;
        account.sync.hydrate_now(Path::new(path)).await.map_err(to_fault)
    }

    async fn dehydrate(&self, path: &str) -> Result<(), SyncFault> {
        let account = self.manager.route(Path::new(path)).await.ok_or_else(|| outside(path))?;
        account.sync.dehydrate(Path::new(path)).await.map_err(to_fault)
    }

    async fn item_state(&self, path: &str) -> String {
        match self.manager.route(Path::new(path)).await {
            Some(account) => account.sync.item_state(Path::new(path)).await,
            None => "not-managed".into(),
        }
    }

    /// "Always keep on this device" for each path; how many files were queued for
    /// download, over every account.
    #[zbus(out_args("queued"))]
    async fn pin(&self, paths: Vec<String>) -> Result<u32, SyncFault> {
        let groups = self.manager.route_all(&paths).await?;
        for (account, paths) in &groups {
            account.sync.check_pinnable(paths).await.map_err(to_fault)?;
        }
        let mut queued = 0;
        for (account, paths) in groups {
            queued += account.sync.pin(&paths).await.map_err(to_fault)?;
        }
        Ok(queued)
    }

    /// Unchecking "Always keep on this device": each path's own pin comes off, and its
    /// files stay; how many pins came off. Every account's paths are checked first.
    #[zbus(out_args("unpinned"))]
    async fn unpin(&self, paths: Vec<String>) -> Result<u32, SyncFault> {
        let groups = self.manager.route_all(&paths).await?;
        for (account, paths) in &groups {
            account.sync.check_unpinnable(paths).await.map_err(to_fault)?;
        }
        let mut unpinned = 0;
        for (account, paths) in groups {
            unpinned += account.sync.unpin(&paths).await.map_err(to_fault)?;
        }
        Ok(unpinned)
    }

    /// "Free up space" for each path, taking its own pin off first. `busy` counts the
    /// files kept because they were in use or changed here. Every account's paths are
    /// checked first.
    #[zbus(out_args("files", "bytes", "busy", "skipped_pinned"))]
    async fn free_up(&self, paths: Vec<String>) -> Result<(u32, u64, u32, u32), SyncFault> {
        let groups = self.manager.route_all(&paths).await?;
        for (account, paths) in &groups {
            account.sync.check_free_up(paths).await.map_err(to_fault)?;
        }
        let mut total = (0, 0, 0, 0);
        for (account, paths) in groups {
            let freed = account.sync.free_up(&paths).await.map_err(to_fault)?;
            total.0 += freed.files;
            total.1 += freed.bytes;
            total.2 += freed.busy + freed.modified;
            total.3 += freed.pinned;
        }
        Ok(total)
    }

    /// The address of the page OneDrive's web interface has for the file or folder at
    /// `path`; of the drive's root for an account's folder itself. Asks OneDrive, and
    /// changes nothing.
    #[zbus(out_args("url"))]
    async fn web_url(&self, path: &str) -> Result<String, SyncFault> {
        if let Some(account) = self.manager.folder_itself(Path::new(path)).await {
            return account.sync.root_web_url().await.map_err(to_fault);
        }
        let account = self.manager.route(Path::new(path)).await.ok_or_else(|| outside(path))?;
        account.sync.web_url(Path::new(path)).await.map_err(to_fault)
    }
}
