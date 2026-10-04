//! What an object that left the folder becomes where nothing is downloaded into it: in the
//! Trash, as a `move-out` row's step leaves it before its item is deleted (`cases`), and
//! after a dropped row, where the item stays in OneDrive (`dropped`). Both follow the one
//! rule written here: a downloaded file stays, stripped, as the user's own; a file that
//! is not downloaded, or is between the two, goes; a directory of the item is unmarked, stripped, and removed if
//! left empty.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_fs::placeholder::{self, State};
use konedrive_tree::{Store, Table, TreeError};
use nix::errno::Errno;
use nix::fcntl::AtFlags;
use nix::unistd::UnlinkatFlags;

use super::place::{parent_has, proc_path, reopen_parent, verified_path};
use super::trash::TrashEntry;
use super::walk::{dir_below, open_met, reopen_dir, walk, Met};
use super::{unmark, MoveOuts};
use crate::folder::disk::Disk;
use crate::upload::steps::off;

/// The ids the base has inside folder `id` now, the folder's own included (a file's is
/// its own alone).
pub(super) async fn inside_of(store: &Store, id: &str) -> Result<HashSet<String>, TreeError> {
    let folder = id.to_owned();
    let mut inside: HashSet<String> = store.call(move |s| s.descendants(Table::Items, &folder)).await?.into_iter().collect();
    inside.insert(id.to_owned());
    Ok(inside)
}

/// A folder that left, as it stands outside: opened again by its path — through the user's
/// own lookups, never beneath a descriptor `OpenByHandle` gave (F90) — and walked, with the
/// ids the base has inside its item.
pub(super) struct Walked {
    /// Where it was proved to be.
    pub(super) path: PathBuf,
    pub(super) top: Arc<File>,
    /// Everything below it, parents before children.
    pub(super) met: Vec<Met>,
    pub(super) inside: HashSet<String>,
}

impl Walked {
    /// The folder behind `object`, walked where it was proved to be, at `path`: file calls
    /// only, for a blocking section. `None` when another directory stands there by now.
    pub(super) fn at(path: &Path, object: &File, inside: HashSet<String>) -> io::Result<Option<Self>> {
        let Some(top) = reopen_dir(path, object)? else { return Ok(None) };
        let met = walk(&top)?;
        Ok(Some(Self { path: path.to_owned(), top: Arc::new(top), met, inside }))
    }

    /// [`at`](Self::at) where `object` is now; `None` also where that is not proved.
    pub(super) fn of(object: &File, inside: HashSet<String>) -> io::Result<Option<Self>> {
        match verified_path(object) {
            Some(path) => Self::at(&path, object, inside),
            None => Ok(None),
        }
    }

    /// Whether `m` carries the id of the item, or of something the base has inside it.
    pub(super) fn ours(&self, m: &Met) -> bool {
        m.id.as_ref().is_some_and(|id| self.inside.contains(id))
    }

    /// The item's files.
    pub(super) fn files(&self) -> impl Iterator<Item = &Met> {
        self.met.iter().filter(|m| !m.is_dir && self.ours(m))
    }

    /// The item's directories below the folder itself, children before parents.
    fn dirs(&self) -> impl Iterator<Item = &Met> {
        self.met.iter().rev().filter(|m| m.is_dir && self.ours(m))
    }
}

/// What becomes of a file of an item that left, where nothing is downloaded into it.
pub(super) enum Fate {
    /// Downloaded: it stays, stripped, as the user's own file.
    Stays,
    /// Not downloaded — never filled, a fill cut short, or a free-up cut short: it goes.
    /// OneDrive has the content the daemon last knew of. A free-up cut short before its
    /// punch still holds the whole content, and an edit made in place since then, which
    /// no examination records (the state reads as unknown content), goes with the file
    /// (limitations log F237).
    Goes,
    /// No state that can be read: nothing is decided.
    Unsure,
}

/// A file's [`Fate`], by its state. For a file whose per-inode lock the caller holds: no
/// fill or free-up of it is under way.
pub(super) fn fate(file: &File) -> Fate {
    match placeholder::read_state(file) {
        Ok(Some(State::Hydrated)) => Fate::Stays,
        Ok(Some(State::OnlineOnly | State::Hydrating | State::Dehydrating)) => Fate::Goes,
        Ok(None) | Err(_) => Fate::Unsure,
    }
}

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
fn remove_at(file: &File, parent: &File, name: &OsStr) -> io::Result<bool> {
    let meta = file.metadata()?;
    match nix::sys::stat::fstatat(parent.as_fd(), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(there) if (there.st_dev, there.st_ino) == (meta.dev(), meta.ino()) => {
            nix::unistd::unlinkat(parent.as_fd(), name, UnlinkatFlags::NoRemoveDir)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Removes each of `files` (placeholders, as the walk met them below `top`) by its name, while
/// that name is still its inode. Whether every one is proved gone: unlinked, with no link left.
pub(super) fn remove_all(top: &File, files: &[Met]) -> io::Result<bool> {
    let mut all = true;
    for m in files {
        let file = open_met(top, m)?;
        if let Some(name) = m.rel.file_name() {
            all &= remove_at(&file, &dir_below(top, m.dir())?, name)? && file.metadata()?.nlink() == 0;
        }
    }
    Ok(all)
}

/// Takes konedrive's attributes off each of `files` (downloaded, as the walk met them below
/// `top`): the user's own from here on.
pub(super) fn strip_all(top: &File, files: &[Met]) -> io::Result<()> {
    for m in files {
        placeholder::strip(&open_met(top, m)?)?;
    }
    Ok(())
}

/// Removes directory `name` in `parent` if it is `dir` and empty: best effort.
fn remove_empty_dir(dir: &File, parent: &File, name: &OsStr) {
    let Ok(meta) = dir.metadata() else { return };
    if let Ok(there) = nix::sys::stat::fstatat(parent.as_fd(), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        if (there.st_dev, there.st_ino) == (meta.dev(), meta.ino()) {
            match nix::unistd::unlinkat(parent.as_fd(), name, UnlinkatFlags::RemoveDir) {
                // It still holds something of the user's: it stays.
                Ok(()) | Err(Errno::ENOTEMPTY | Errno::EEXIST) => {}
                Err(e) => tracing::debug!("an emptied directory that left the folder stays where it is: {e}"),
            }
        }
    }
}

fn remove_info(entry: &TrashEntry) {
    let (Some(dir), Some(name)) = (entry.info.parent(), entry.info.file_name()) else { return };
    let removed = reopen_parent(dir).and_then(|dir| Ok(nix::unistd::unlinkat(dir.as_fd(), name, UnlinkatFlags::NoRemoveDir)?));
    if let Err(e) = removed {
        tracing::debug!("{} stays in the Trash without its entry: {e}", entry.info.display());
    }
}

/// What becomes of a directory of the item once it is unmarked and stripped.
pub(super) enum Emptied<'a> {
    /// It stays, the user's own directory, whatever it holds (moved anywhere but the Trash).
    Stays,
    /// It is removed if nothing is left in it (the Trash; a row dropped): the folder itself
    /// too, and with it the `.trashinfo` of `entry` when the folder was that entry.
    Goes { entry: Option<&'a TrashEntry> },
}

/// The directories of a folder that left, once its files are done with: bottom up and the
/// folder itself last, each unmarked (`UnmarkDir`, never one beneath a registered folder, and
/// none of `keep_marked`, by their places below the folder), stripped, and then as `emptied`
/// says. One blocking section for each directory's file calls.
pub(super) async fn tidy_dirs(mo: &MoveOuts, disk: &Arc<Disk>, walked: &Walked, keep_marked: &HashSet<PathBuf>, emptied: Emptied<'_>) -> io::Result<()> {
    let goes = matches!(emptied, Emptied::Goes { .. });
    for m in walked.dirs() {
        let (below, met) = (Arc::clone(&walked.top), m.clone());
        let dir = Arc::new(off(move || open_met(&below, &met)).await?);
        if !keep_marked.contains(&m.rel) {
            unmark(mo, disk, &dir).await;
        }
        let (below, in_dir) = (Arc::clone(&walked.top), m.dir().to_owned());
        let name = m.rel.file_name().filter(|_| goes).map(OsStr::to_os_string);
        off(move || {
            let Some(name) = name else { return placeholder::strip(&dir) };
            let parent = dir_below(&below, &in_dir)?;
            placeholder::strip(&dir)?;
            remove_empty_dir(&dir, &parent, &name);
            Ok(())
        })
        .await?;
    }
    if !keep_marked.contains(Path::new("")) {
        unmark(mo, disk, &walked.top).await;
    }
    let (top, path) = (Arc::clone(&walked.top), walked.path.clone());
    let entry = match emptied {
        Emptied::Stays => None,
        Emptied::Goes { entry } => Some(entry.cloned()),
    };
    off(move || {
        placeholder::strip(&top)?;
        let Some(entry) = entry else { return Ok(()) };
        if std::fs::read_dir(proc_path(&top))?.next().is_some() {
            return Ok(());
        }
        if let (Some(parent), Some(name)) = (path.parent().and_then(|p| reopen_parent(p).ok()), path.file_name()) {
            remove_empty_dir(&top, &parent, name);
            // The whole entry went: its `.trashinfo` goes too.
            if let Some(entry) = entry.filter(|e| e.top == path && !parent_has(&parent, name)) {
                remove_info(&entry);
            }
        }
        Ok(())
    })
    .await
}
