use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;

use konedrive_fs::placeholder::{self, State};
use nix::fcntl::AtFlags;
use crate::folder::disk::Disk;
use crate::helper::HelperError;
use crate::folder::root::SyncRoot;
use crate::folder::locks::{InodeKey, InodeLocks};
use konedrive_tree::outbox::{OutboxKind, OutboxRow};
use konedrive_tree::{Store, Table, TreeError, TreeStore};

use super::place::{beneath_a_root, parent_has, proc_path, reopen_parent, verified_path};
use super::trash::{is_mount_point, real_trash, trash_of, TrashEntry};
use super::walk::{dir_below, item_id_of, open_met, reopen_dir, strip, walk};
use super::{MoveOuts, off, unmark};

/// Removes the placeholder `file` at `path` in the Trash, and its entry's `.trashinfo` when it is
/// the entry itself: by name, in its directory opened by path, and only while that name is still
/// this inode. Whether it was unlinked.
pub(super) fn remove(file: &File, path: &Path, entry: Option<&TrashEntry>) -> io::Result<bool> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else { return Ok(false) };
    let parent = reopen_parent(parent)?;
    let removed = remove_at(file, &parent, name)?;
    if let Some(entry) = entry.filter(|e| removed && path == e.top) {
        remove_info(entry);
    }
    Ok(removed)
}

/// Unlinks `name` in `parent` if it is `file`: whether it did.
pub(super) fn remove_at(file: &File, parent: &File, name: &OsStr) -> io::Result<bool> {
    let meta = file.metadata()?;
    match nix::sys::stat::fstatat(parent.as_fd(), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(there) if (there.st_dev, there.st_ino) == (meta.dev(), meta.ino()) => {
            nix::unistd::unlinkat(parent.as_fd(), name, nix::unistd::UnlinkatFlags::NoRemoveDir)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Removes directory `name` in `parent` if it is `dir` and empty: best effort.
pub(super) fn remove_empty_dir(dir: &File, parent: &File, name: &OsStr) {
    let Ok(meta) = dir.metadata() else { return };
    if let Ok(there) = nix::sys::stat::fstatat(parent.as_fd(), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        if (there.st_dev, there.st_ino) == (meta.dev(), meta.ino()) {
            let _ = nix::unistd::unlinkat(parent.as_fd(), name, nix::unistd::UnlinkatFlags::RemoveDir);
        }
    }
}

pub(super) fn remove_info(entry: &TrashEntry) {
    if let (Some(dir), Some(name)) = (entry.info.parent(), entry.info.file_name()) {
        if let Ok(dir) = reopen_parent(dir) {
            let _ = nix::unistd::unlinkat(dir.as_fd(), name, nix::unistd::UnlinkatFlags::NoRemoveDir);
        }
    }
}

// ---------------------------------------------------------------------------
// Dropped rows 
// ---------------------------------------------------------------------------

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
    /// Trash case, without the delete (the examination). Its item stays in OneDrive, and a read-write
    /// folder's reconcile places it again (the drop forgot its local object). A placeholder
    /// outside, which holds nothing whole, goes, so that it never reads as zeros (Z3); a
    /// downloaded file stays, stripped, as the user's own; a directory of the item is unmarked,
    /// stripped, and removed if left empty. Anything not proved to be outside, a placeholder with
    /// another link or being filled, and anything the helper cannot reach now, is left as it is.
    /// Local only: nothing is sent.
    pub(crate) async fn dropped(&self, rows: &[OutboxRow]) {
        let registered = self.root.clone();
        let Ok(disk) = off(move || Disk::open(&registered, false)).await.map(Arc::new) else { return };
        let Ok(root) = disk.dir(Path::new("")) else { return };
        for row in rows.iter().filter(|r| r.kind == OutboxKind::MoveOut) {
            let (Some(id), Some(handle)) = (row.item_id.as_deref(), row.inode.as_ref().and_then(|i| i.handle.as_ref())) else { continue };
            let object = match self.mo.helper.open_by_handle(&root, handle).await {
                Ok(object) => Arc::new(File::from(object)),
                Err(HelperError::Refused(libc::ESTALE | libc::EPERM)) => continue,
                Err(err) => {
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
            let Ok(Some((path, entry))) = placed else { continue };
            match self.tidy(&disk, id, object, &path, entry.as_ref()).await {
                Ok(()) => tracing::info!("{} stays in OneDrive: what had left the folder is tidied at {}", row.rel.display(), path.display()),
                Err(err) => tracing::warn!("what left the folder as {} is left as it is: {err}", row.rel.display()),
            }
        }
    }

    async fn tidy(&self, disk: &Arc<Disk>, id: &str, object: Arc<File>, path: &Path, entry: Option<&TrashEntry>) -> io::Result<()> {
        let mut inside: HashSet<String> =
            { let folder = id.to_owned(); self.store.call(move |s| s.descendants(Table::Items, &folder)).await }.map_err(|_| io::Error::other("the base cannot be read"))?.into_iter().collect();
        inside.insert(id.to_owned());
        let (locks, at, trash) = (self.locks.clone(), path.to_path_buf(), entry.cloned());
        // The files, off the runtime.
        let walked = off(move || {
            if !object.metadata()?.is_dir() {
                let Some(_inode) = locks.try_lock(InodeKey::of(&object)?) else { return Ok(None) };
                match placeholder::read_state(&object) {
                    Ok(Some(State::Hydrated)) => strip(&object)?,
                    Ok(Some(_)) if object.metadata()?.nlink() == 1 => {
                        remove(&object, &at, trash.as_ref())?;
                    }
                    _ => {}
                }
                return Ok(None);
            }
            let Some(top) = reopen_dir(&at, &object)? else { return Ok(None) };
            let met = walk(&top)?;
            for m in met.iter().filter(|m| !m.is_dir && m.id.as_ref().is_some_and(|i| inside.contains(i))) {
                let file = open_met(&top, m)?;
                let Some(_inode) = locks.try_lock(InodeKey::of(&file)?) else { continue };
                match placeholder::read_state(&file) {
                    Ok(Some(State::Hydrated)) => strip(&file)?,
                    // Not whole: never filled, or a fill or a free-up cut short. OneDrive has it.
                    Ok(Some(_)) if file.metadata()?.nlink() == 1 => {
                        if let Some(name) = m.rel.file_name() {
                            remove_at(&file, &dir_below(&top, m.dir())?, name)?;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Some((Arc::new(top), met, inside)))
        })
        .await?;
        let Some((top, met, inside)) = walked else { return Ok(()) };
        for m in met.iter().rev().filter(|m| m.is_dir && m.id.as_ref().is_some_and(|i| inside.contains(i))) {
            let (below, met) = (Arc::clone(&top), m.clone());
            let dir = Arc::new(off(move || open_met(&below, &met)).await?);
            unmark(self.mo, disk, &dir).await;
            let (below, in_dir) = (Arc::clone(&top), m.dir().to_owned());
            let name = m.rel.file_name().map(OsStr::to_os_string);
            off(move || {
                let parent = dir_below(&below, &in_dir)?;
                strip(&dir)?;
                if let Some(name) = name {
                    remove_empty_dir(&dir, &parent, &name);
                }
                Ok(())
            })
            .await?;
        }
        unmark(self.mo, disk, &top).await;
        let (at, trash) = (path.to_path_buf(), entry.cloned());
        off(move || {
            strip(&top)?;
            if std::fs::read_dir(proc_path(&top))?.next().is_none() {
                if let (Some(parent), Some(name)) = (at.parent().and_then(|p| reopen_parent(p).ok()), at.file_name()) {
                    remove_empty_dir(&top, &parent, name);
                    if let Some(entry) = trash.filter(|e| e.top == at && !parent_has(&parent, name)) {
                        remove_info(&entry);
                    }
                }
            }
            Ok(())
        })
        .await
    }
}
