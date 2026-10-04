//! `move-out` rows dropped before they ran: what they left outside the folder is tidied,
//! and nothing is sent.

use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;

use konedrive_fs::placeholder;
use konedrive_tree::outbox::{OutboxKind, OutboxRow};
use konedrive_tree::{Store, Table, TreeError, TreeStore};

use super::place::{beneath_a_root, verified_path};
use super::reach::{reach, Reach};
use super::tidy::{fate, inside_of, remove, remove_all, strip_all, tidy_dirs, Emptied, Fate, Walked};
use super::trash::{is_mount_point, real_trash, trash_of, TrashEntry};
use super::walk::{item_id_of, open_met};
use super::MoveOuts;
use crate::folder::disk::Disk;
use crate::folder::locks::{InodeKey, InodeLocks};
use crate::folder::root::SyncRoot;
use crate::upload::steps::off;

/// Drops every `move-out` row, each item and what is inside it forgetting its local object — the
/// object outside, which [`Tidy::dropped`] tidies next — so that a read-write folder's reconcile
/// places it again. The rows dropped. One whose item the base has under a
/// temporary name stays, as `outbox_drop_all` keeps it.
pub(crate) fn drop_rows(s: &mut TreeStore) -> Result<Vec<OutboxRow>, TreeError> {
    let mut rows = Vec::new();
    for row in s.outbox_move_outs()? {
        let swapping = match row.item_id.as_deref() {
            Some(id) => s.get(Table::Items, id)?.is_some_and(|item| item.name.starts_with(crate::upload::SWAP_PREFIX)),
            None => false,
        };
        if !swapping {
            s.outbox_drop(row.seq, None, row.item_id.as_deref(), None)?;
            rows.push(row);
        }
    }
    Ok(rows)
}

/// What tidies after dropped `move-out` rows, with or without a worker.
pub(crate) struct Tidy<'a> {
    pub mo: &'a MoveOuts,
    pub root: &'a SyncRoot,
    pub store: &'a Store,
    pub locks: &'a InodeLocks,
}

impl Tidy<'_> {
    /// `rows` were dropped (`RestoreDeletes`, a switch to read-only, a Forget, a Remove): nothing
    /// will download what their `move-out`s left outside the folder, and nothing marks it again
    /// after the helper restarts, so each object still outside every folder is tidied as the
    /// Trash case is (`tidy`), without the delete. Its item stays in OneDrive, and a read-write
    /// folder's reconcile places it again (the drop forgot its local object). A placeholder
    /// outside, which holds nothing whole, goes, so that it never reads as zeros (Z3); a
    /// downloaded file stays, stripped, as the user's own; a directory of the item is unmarked,
    /// stripped, and removed if left empty. Anything not proved to be outside, a placeholder with
    /// another link or being filled, and anything the helper cannot reach now, is left as it is.
    /// Local only: nothing is sent.
    pub(crate) async fn dropped(&self, rows: &[OutboxRow]) {
        let registered = self.root.clone();
        let disk = match off(move || Disk::open(&registered, false)).await {
            Ok(disk) => Arc::new(disk),
            Err(err) => {
                tracing::debug!("what left the folder is left as it is: the folder cannot be opened: {err}");
                return;
            }
        };
        let root = match disk.dir(Path::new("")) {
            Ok(root) => root,
            Err(err) => {
                tracing::debug!("what left the folder is left as it is: the folder cannot be opened: {err}");
                return;
            }
        };
        for row in rows.iter().filter(|r| r.kind == OutboxKind::MoveOut) {
            let (Some(id), Some(handle)) = (row.item_id.as_deref(), row.inode.as_ref().and_then(|i| i.handle.as_ref())) else { continue };
            let object = match reach(&*self.mo.helper, self.store, &root, handle).await {
                Reach::Open(object) => object,
                // Gone, or stripped already: nothing is left to tidy.
                Reach::Gone | Reach::Refused => continue,
                Reach::Stale => {
                    tracing::warn!("what left the folder as {} is left as it is, not reached: its handle is another filesystem's", row.rel.display());
                    continue;
                }
                Reach::Busy => {
                    tracing::warn!("what left the folder as {} is left as it is, not reached: it is leased", row.rel.display());
                    continue;
                }
                Reach::BadHandle => {
                    tracing::warn!("what left the folder as {} is left as it is, not reached: its handle cannot be read", row.rel.display());
                    continue;
                }
                Reach::Errno(errno) => {
                    tracing::warn!("what left the folder as {} is left as it is, not reached: errno {errno}", row.rel.display());
                    continue;
                }
                Reach::NoHelper(err) => {
                    tracing::warn!("what left the folder as {} is left as it is, not reached: {err}", row.rel.display());
                    continue;
                }
            };
            // Whether it is the item's, and where it is: outside every folder, in a Trash or not.
            let (mo, on, at, item) = (self.mo.clone(), Arc::clone(&disk), Arc::clone(&object), id.to_owned());
            let placed = off(move || {
                if item_id_of(&at).as_deref() != Some(item.as_str()) {
                    return Ok(None);
                }
                let Some(path) = verified_path(&at).filter(|p| !beneath_a_root(&mo, &on, p)) else { return Ok(None) };
                let entry = trash_of(&path, mo.home_trash.as_deref(), nix::unistd::geteuid().as_raw(), &is_mount_point).filter(real_trash);
                Ok(Some((path, entry)))
            })
            .await;
            let (path, entry) = match placed {
                Ok(Some(placed)) => placed,
                // Another item's by now, in a folder again, or at a place that cannot be proved.
                Ok(None) => continue,
                Err(err) => {
                    tracing::debug!("what left the folder as {} is left as it is: where it is cannot be read: {err}", row.rel.display());
                    continue;
                }
            };
            match self.tidy(&disk, id, object, &path, entry.as_ref()).await {
                Ok(()) => tracing::info!("{} stays in OneDrive: what had left the folder is tidied at {}", row.rel.display(), path.display()),
                Err(err) => tracing::warn!("what left the folder as {} is left as it is: {err}", row.rel.display()),
            }
        }
    }

    async fn tidy(&self, disk: &Arc<Disk>, id: &str, object: Arc<File>, path: &Path, entry: Option<&TrashEntry>) -> io::Result<()> {
        let inside = inside_of(self.store, id).await.map_err(|e| io::Error::other(format!("the base cannot be read: {e}")))?;
        let (locks, at, trash) = (self.locks.clone(), path.to_path_buf(), entry.cloned());
        // The files, off the runtime, each under its own lock: one a fill holds is left.
        let walked = off(move || {
            if !object.metadata()?.is_dir() {
                let Some(_inode) = locks.try_lock(InodeKey::of(&object)?) else { return Ok(None) };
                match fate(&object) {
                    Fate::Stays => placeholder::strip(&object)?,
                    Fate::Goes if object.metadata()?.nlink() == 1 => {
                        remove(&object, &at, trash.as_ref())?;
                    }
                    Fate::Goes | Fate::Unsure => {}
                }
                return Ok(None);
            }
            let Some(walked) = Walked::at(&at, &object, inside)? else { return Ok(None) };
            let mut held = Vec::new();
            let (mut stay, mut go) = (Vec::new(), Vec::new());
            for m in walked.files() {
                let file = open_met(&walked.top, m)?;
                let Some(inode) = locks.try_lock(InodeKey::of(&file)?) else { continue };
                match fate(&file) {
                    Fate::Stays => stay.push(m.clone()),
                    Fate::Goes if file.metadata()?.nlink() == 1 => go.push(m.clone()),
                    Fate::Goes | Fate::Unsure => continue,
                }
                held.push(inode);
            }
            remove_all(&walked.top, &go)?;
            strip_all(&walked.top, &stay)?;
            Ok(Some(walked))
        })
        .await?;
        let Some(walked) = walked else { return Ok(()) };
        tidy_dirs(self.mo, disk, &walked, &HashSet::new(), Emptied::Goes { entry }).await
    }
}
