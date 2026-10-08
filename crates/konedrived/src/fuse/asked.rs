//! What the daemon asks of a mount for a path: download it, free it up, look at it.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_fs::placeholder::State;
use konedrive_graph::pool::Class;
use konedrive_tree::{Kind, Row, Table};
use tokio::sync::oneshot;

use super::announce::Announce;
use super::copies::remove_copy;
use super::local::Entry;
use super::mount::Mount;

/// What a free-up did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Freed {
    pub files: u64,
    pub bytes: u64,
    /// Downloaded files left as they are: pinned, or being downloaded.
    pub kept: u64,
}

/// Why the mount refuses what it was asked.
#[derive(Debug, thiserror::Error)]
pub enum Refused {
    #[error("not an item of this folder")]
    NotOurs,
    #[error("{path} is pinned by {by}: unpin it first")]
    Pinned { path: String, by: String },
    #[error("{0}")]
    Io(String),
}

/// What [`Mount::look`] saw of an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Looked {
    pub id: String,
    pub folder: bool,
    /// `None` for a folder.
    pub state: Option<State>,
    pub own_pin: bool,
    /// The folder above that pins it.
    pub pinned_above: Option<PathBuf>,
}

impl Mount {
    pub(super) fn rel(&self, path: &Path) -> Result<PathBuf, Refused> {
        path.strip_prefix(&self.shared.root.path).map(Path::to_owned).map_err(|_| Refused::NotOurs)
    }

    pub(super) fn item(&self, path: &Path) -> Result<(Row, PathBuf), Refused> {
        let rel = self.rel(path)?;
        // What has no item yet is not OneDrive's to download, free or pin.
        match self.shared.resolve(&rel) {
            Some(Entry::Item(row)) => Ok((row, rel)),
            _ => Err(Refused::NotOurs),
        }
    }

    /// Downloads the file at `path` now, and waits for it.
    pub async fn hydrate(&self, path: &Path) -> Result<(), Refused> {
        let (row, rel) = self.item(path)?;
        if row.kind != Kind::File {
            return Err(Refused::NotOurs);
        }
        if self.shared.has_copy(&rel, &row.id) {
            return Ok(());
        }
        let (tell, told) = oneshot::channel();
        self.shared.ensure(row, rel, Class::Open, Some(tell));
        match told.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(errno)) => Err(Refused::Io(format!("hydration failed: {}", io::Error::from_raw_os_error(errno)))),
            Err(_) => Err(Refused::Io("the download was given up".into())),
        }
    }

    /// Frees the space of the downloaded files at and below `path`. What a pin keeps, or a
    /// download is filling, is left and counted. Whether `path` itself may be freed is the
    /// caller's to ask first ([`look`](Self::look)).
    pub async fn free_up(&self, path: &Path) -> Result<Freed, Refused> {
        let (row, _) = self.item(path)?;
        Ok(self.free_below(row).await)
    }

    /// Frees the space of every downloaded file that no pin keeps.
    pub async fn free_up_all(&self) -> Freed {
        let Some(root) = self.shared.read(|s| s.root_item_id()).flatten().and_then(|id| self.shared.row(&id)) else { return Freed::default() };
        self.free_below(root).await
    }

    pub(super) async fn free_below(&self, row: Row) -> Freed {
        let s = Arc::clone(&self.shared);
        let freed = tokio::task::spawn_blocking(move || {
            let (mut freed, mut told) = (Freed::default(), Vec::new());
            for (file, at) in s.files_below(&row) {
                if !s.has_copy(&at, &file.id) {
                    continue;
                }
                if s.pinned_by(&file).is_some() || crate::panic::lock(&s.fills).contains_key(&file.id) {
                    freed.kept += 1;
                    continue;
                }
                match remove_copy(&s.under(&at), &file.id) {
                    Some(bytes) => {
                        freed.files += 1;
                        freed.bytes += bytes;
                        told.push(Announce::Changed(at, file.id));
                    }
                    None => freed.kept += 1,
                }
            }
            s.announce(told);
            freed
        })
        .await
        .unwrap_or_default();
        self.shared.report.space.kick();
        freed
    }

    /// The state of the item at `path`, its own pin, and the folder above that pins it.
    pub fn look(&self, path: &Path) -> Result<Looked, Refused> {
        let (row, rel) = self.item(path)?;
        let s = &self.shared;
        let own_pin = crate::panic::lock(&s.pins).contains(&row.id);
        let above = row.parent_id.as_deref().and_then(|p| s.row(p)).and_then(|above| s.pinned_by(&above));
        let pinned_above = above.and_then(|by| s.read(|store| store.locate(Table::Items, &by.id)).flatten()).map(|at| s.root.path.join(at.rel));
        let state = (row.kind == Kind::File).then(|| s.state_of(&row, &rel));
        Ok(Looked { id: row.id, folder: row.kind == Kind::Folder, state, own_pin, pinned_above })
    }

    /// How many downloaded files, and bytes, the items at and below `path` hold.
    pub fn downloaded_under(&self, path: &Path) -> Result<(u64, u64), Refused> {
        let (row, _) = self.item(path)?;
        let s = &self.shared;
        let sizes = s.files_below(&row).into_iter().filter_map(|(file, at)| s.copy(&at, &file.id));
        Ok(sizes.fold((0, 0), |(files, bytes), m| (files + 1, bytes + m.blocks() * 512)))
    }
}
