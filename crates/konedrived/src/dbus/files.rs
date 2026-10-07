use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use zbus::interface;
use zbus::zvariant::Value;

use konedrive_dbus::Refusal;

use crate::dbus::fault::Fault;
use crate::daemon::manager::{AccountManager, Outside};

pub(crate) fn outside(path: &str) -> Fault {
    Fault::refused(Refusal::OutsideRoot, format!("{path} is in no account's folder"))
}

impl From<Outside> for Fault {
    fn from(Outside(path): Outside) -> Self {
        outside(&path)
    }
}

/// `org.konedrive.Files` (`dbus/org.konedrive.Files.xml`): the per-file calls, each
/// routed by path to the account whose folder holds it.
pub struct Files {
    pub(crate) manager: Arc<AccountManager>,
}


#[interface(name = "org.konedrive.Files")]
impl Files {
    async fn hydrate(&self, path: &str) -> Result<(), Fault> {
        let account = self.manager.route(Path::new(path)).await.ok_or_else(|| outside(path))?;
        Ok(account.sync.hydrate_now(Path::new(path)).await?)
    }

    async fn dehydrate(&self, path: &str) -> Result<(), Fault> {
        let account = self.manager.route(Path::new(path)).await.ok_or_else(|| outside(path))?;
        Ok(account.sync.dehydrate(Path::new(path)).await?)
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
    async fn pin(&self, paths: Vec<String>) -> Result<u32, Fault> {
        let groups = self.manager.route_all(&paths).await?;
        for (account, paths) in &groups {
            account.sync.check_pinnable(paths).await?;
        }
        let mut queued = 0;
        for (account, paths) in groups {
            queued += account.sync.pin(&paths).await?;
        }
        Ok(queued)
    }

    /// Unchecking "Always keep on this device": each path's own pin comes off, and its
    /// files stay; how many pins came off. Every account's paths are checked first.
    #[zbus(out_args("unpinned"))]
    async fn unpin(&self, paths: Vec<String>) -> Result<u32, Fault> {
        let groups = self.manager.route_all(&paths).await?;
        for (account, paths) in &groups {
            account.sync.check_unpinnable(paths).await?;
        }
        let mut unpinned = 0;
        for (account, paths) in groups {
            unpinned += account.sync.unpin(&paths).await?;
        }
        Ok(unpinned)
    }

    /// "Free up space" for each path, taking its own pin off first. `busy` counts the
    /// files kept because they were in use or changed here. Every account's paths are
    /// checked first.
    #[zbus(out_args("files", "bytes", "busy", "skipped_pinned"))]
    async fn free_up(&self, paths: Vec<String>) -> Result<(u32, u64, u32, u32), Fault> {
        let groups = self.manager.route_all(&paths).await?;
        for (account, paths) in &groups {
            account.sync.check_free_up(paths).await?;
        }
        let mut total = (0, 0, 0, 0);
        for (account, paths) in groups {
            let freed = account.sync.free_up(&paths).await?;
            total.0 += freed.files;
            total.1 += freed.bytes;
            total.2 += freed.busy + freed.modified;
            total.3 += freed.pinned;
        }
        Ok(total)
    }

    /// What the context menu may offer for the selection `paths`, under the keys of
    /// `dbus/org.konedrive.Files.xml`. Never refused for a path it does not take: such a
    /// path is not among `paths` in the answer. Changes nothing and opens no file.
    #[zbus(out_args("menu"))]
    async fn menu(&self, paths: Vec<String>) -> HashMap<&'static str, Value<'static>> {
        let menu = self.manager.menu(&paths).await;
        HashMap::from([
            ("paths", Value::from(menu.paths)),
            ("always-keep", Value::from(menu.always_keep.as_str())),
            ("free-up", Value::from(menu.free_up.as_str())),
            ("free-up-why", Value::from(menu.free_up_why.map_or("", |why| why.as_str()))),
            ("blocked-by", Value::from(menu.blocked_by)),
            ("open-online", Value::from(menu.open_online.as_str())),
            ("open-online-path", Value::from(menu.open_online_path)),
        ])
    }

    /// The address of the page OneDrive's web interface has for the file or folder at
    /// `path`; of the drive's root for an account's folder itself. Asks OneDrive, and
    /// changes nothing.
    #[zbus(out_args("url"))]
    async fn web_url(&self, path: &str) -> Result<String, Fault> {
        if let Some(account) = self.manager.folder_itself(Path::new(path)).await {
            return Ok(account.sync.root_web_url().await?);
        }
        let account = self.manager.route(Path::new(path)).await.ok_or_else(|| outside(path))?;
        Ok(account.sync.web_url(Path::new(path)).await?)
    }
}
