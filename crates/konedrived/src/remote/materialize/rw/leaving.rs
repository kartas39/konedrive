use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

use std::os::fd::AsFd;

use crate::remote::materialize::removal::Policy;
use crate::remote::materialize::{ApplyError, Materializer, Run};
use crate::folder::disk::{Probe, Scanned};
use konedrive_tree::reconcile::Leaving;
use konedrive_tree::{Planned, Table};
use super::{swapped, Rw};

impl Materializer {
    /// Item `id`, at `rel`, stays in OneDrive but is no longer placed here —
    /// a name too long, a reserved one, the Personal Vault, shared, OneNote
    /// (issue #104, decision 3). Its placement is the base's in this cycle
    /// whatever holds it, and its local objects are forgotten; its object
    /// stays where it is for now, and is examined, so that what waits to be
    /// uploaded from inside it gets its row and goes up into the item in
    /// OneDrive. [`Self::leaving_rw`] removes it once nothing does.
    pub(super) fn unplace(&self, rel: &Path, id: &str, is_dir: bool, run: &mut Run) -> Result<(), ApplyError> {
        let parent = rel.parent().unwrap_or(Path::new(""));
        let name = rel.file_name().ok_or_else(|| ApplyError::Io(format!("{} has no name", rel.display())))?;
        let dir = self.disk.dir(parent)?;
        let handles: Vec<_> = konedrive_fs::handle::FileHandle::at(&dir, name).ok().into_iter().collect();
        let (ids, at) = (vec![id.to_owned()], rel.to_path_buf());
        self.store.call_blocking(move |s| {
            s.forget_local_objects(&ids, &handles)?;
            s.leaving_add(&ids[0], &at, handles.first())
        })?;
        tracing::info!("{} is no longer placed here; it goes once nothing in it waits to be uploaded", rel.display());
        run.out.on_disk.examine.push((rel.to_path_buf(), is_dir));
        run.out.on_disk.taken.insert(id.to_owned());
        run.unplaced.insert(id.to_owned());
        Ok(())
    }

    /// What is leaving, by item: read once for a Full scan, and again after
    /// [`Self::unplace`] adds to it.
    pub(super) fn leaving_objects(&self) -> Result<HashMap<String, Leaving>, ApplyError> {
        let leaving = self.store.call_blocking(|s| s.leaving_with_handles())?;
        Ok(leaving.into_iter().map(|left| (left.id.clone(), left)).collect())
    }

    /// Whether the misplaced `entry` of a Full scan is the object of its
    /// item `id` that stays while it leaves, whether or not the item is
    /// placed again elsewhere since. Its place is followed, in the store
    /// and in `leaving`: a parent renamed in OneDrive or here took it along.
    ///
    /// Only the object itself: at its recorded place, or carrying its
    /// recorded file handle — never another object with its id (the copy
    /// placed again, a copy, a hard link) — and not the object the new tree
    /// places right there, nor one of an item the new tree does not have,
    /// which goes as anything removed in OneDrive.
    pub(super) fn is_leaving_object(&self, leaving: &mut HashMap<String, Leaving>, entry: &Scanned, id: &str, planned: &Planned) -> Result<bool, ApplyError> {
        if planned.base.is_none() || swapped(planned) {
            return Ok(false);
        }
        let Some(left) = leaving.get_mut(id) else { return Ok(false) };
        // At its recorded place, an object with its id is it — after an
        // editor's save by rename too, its handle then taken anew — unless the
        // item is placed elsewhere, where the copy placed again may stand
        // here by the user's move: then only its handle tells.
        let elsewhere = [planned.base_place(), planned.new_place()].into_iter().flatten().any(|placed| placed.rel != entry.rel);
        let mut renewed = None;
        let itself = match &left.handle {
            None => left.rel == entry.rel,
            // Where a handle is kept, the recorded place counts only for the
            // object carrying it; elsewhere, only an object with one link (a
            // hard link carries the same handle, and is the user's name).
            Some(kept) => {
                let (parent, name) = (entry.rel.parent().unwrap_or(Path::new("")), entry.rel.file_name());
                let dir = self.disk.dir(parent).ok();
                let here = name.zip(dir.as_ref()).and_then(|(name, dir)| konedrive_fs::handle::FileHandle::at(dir, name).ok());
                let single = name.zip(dir.as_ref()).is_some_and(|(name, dir)| {
                    nix::sys::stat::fstatat(dir.as_fd(), name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW).is_ok_and(|s| entry.is_dir || s.st_nlink <= 1)
                });
                if here.as_ref() == Some(kept) {
                    left.rel == entry.rel || single
                } else if left.rel == entry.rel && !elsewhere {
                    renewed = here;
                    true
                } else {
                    false
                }
            }
        };
        if !itself {
            return Ok(false);
        }
        if let Some(handle) = renewed {
            self.store.call_blocking({ let (id, handle) = (id.to_owned(), handle.clone()); move |s| s.leaving_set_handle(&id, &handle) })?;
            left.handle = Some(handle);
        }
        let placed_here = planned.new_place().is_some_and(|placed| placed.rel == entry.rel);
        if planned.new.is_none() || placed_here {
            return Ok(false);
        }
        if left.rel != entry.rel {
            self.store.call_blocking({ let (id, rel) = (id.to_owned(), entry.rel.clone()); move |s| s.leaving_set_rel(&id, &rel) })?;
            left.rel = entry.rel.clone();
        }
        Ok(true)
    }

    /// What stopped being placed and stays on disk for now ([`Self::unplace`]):
    /// placed again where it is, it is the item's again; removed in OneDrive
    /// since, it goes as anything removed there ([`Self::take_off`]); otherwise it goes
    /// whole once no outbox row has a place at or below it and nothing in it
    /// waits to be examined and uploaded (a new file, a changed download).
    /// Until then it is examined again.
    pub(in crate::remote::materialize) fn leaving_rw(&self, rw: &Rw, run: &mut Run) -> Result<(), ApplyError> {
        let leaving = self.store.call_blocking(|s| s.leaving())?;
        for (id, rel) in leaving {
            self.check_cancel()?;
            if run.unplaced.contains(&id) {
                // Examined first: a later cycle decides.
                continue;
            }
            // At its path; else, gone from there (`ENOENT`) or another object
            // there, wherever its file handle finds it — a parent renamed here
            // and not examined yet — its path followed. Its row goes only when
            // neither finds it. Any other error decides nothing, and the row
            // stays.
            let found = match self.leaving_at(&rel, &id) {
                Ok(found) => found,
                Err(e) => {
                    tracing::warn!("{} is leaving and cannot be looked at ({e}); it is looked at again later", rel.display());
                    continue;
                }
            };
            let found = match found {
                Some(found) => Some((rel.clone(), found)),
                None => match self.leaving_by_handle(&id) {
                    Ok(Some(at)) => match self.leaving_at(&at, &id) {
                        Ok(Some(found)) => {
                            tracing::info!("{} is leaving and was found at {} by its handle", rel.display(), at.display());
                            self.store.call_blocking({ let (id, at) = (id.clone(), at.clone()); move |s| s.leaving_set_rel(&id, &at) })?;
                            Some((at, found))
                        }
                        Ok(None) => None,
                        Err(e) => {
                            tracing::warn!("{} is leaving and cannot be looked at ({e}); it is looked at again later", at.display());
                            continue;
                        }
                    },
                    Ok(None) => None,
                    Err(ApplyError::Cancelled) => return Err(ApplyError::Cancelled),
                    Err(e) => {
                        tracing::warn!("{} is leaving and the folder cannot be searched for it ({e}); it is looked at again later", rel.display());
                        continue;
                    }
                },
            };
            let Some((rel, (is_dir, dir))) = found else {
                // Gone, or not its object any more.
                self.store.call_blocking({ let id = id.clone(); move |s| s.leaving_drop(&id) })?;
                continue;
            };
            let name = rel.file_name().map(OsStr::to_os_string).expect("a found object has a name");
            let there = id.clone();
            debug_assert_eq!(there, id);
            let (staged, located) = self.store.call_blocking({ let id = id.clone(); move |s| Ok((s.get(Table::Staging, &id)?, s.locate(Table::Staging, &id)?)) })?;
            if located.as_ref().is_some_and(|l| l.placed && l.rel == rel) {
                // Placed again where it is: the placement found it.
                self.store.call_blocking({ let id = id.clone(); move |s| s.leaving_drop(&id) })?;
                continue;
            }
            if staged.is_none() {
                self.take_off(&dir, &name, &rel, rw.removed_leaving(), run)?;
                self.store.call_blocking({ let id = id.clone(); move |s| s.leaving_drop(&id) })?;
                continue;
            }
            // The daemon never moves or deletes in OneDrive for what is
            // leaving: such rows from before go; only content keeps it.
            let dropped = self.store.call_blocking({ let rel = rel.clone(); move |s| s.outbox_drop_moves(&rel) })?;
            if !dropped.is_empty() {
                tracing::info!("{} is no longer placed here: {} move(s) or delete(s) waiting for it are dropped", rel.display(), dropped.len());
            }
            if is_dir {
                if !located.as_ref().is_some_and(|l| l.placed) {
                    self.store.call_blocking({ let id = id.clone(); move |s| s.leaving_refresh_items(&id) })?;
                }
                self.remove_gone_inside(rw, &dir, &name, &rel, run)?;
            }
            if self.keeps_leaving(rw, &rel, is_dir)? {
                continue;
            }
            // Placed elsewhere now, the item's own object is the new one: only
            // what is here is forgotten.
            let placed_elsewhere = located.is_some_and(|l| l.placed);
            self.take_off(&dir, &name, &rel, Policy::Leaving { placed_elsewhere }, run)?;
            tracing::info!("{} is no longer placed here, and nothing in it waits to be uploaded: removed", rel.display());
            self.store.call_blocking({ let id = id.clone(); move |s| s.leaving_drop(&id) })?;
        }
        Ok(())
    }

    /// The leaving object of item `id` at `rel`: whether it is a directory,
    /// and the directory it is in. `None` when nothing is there (`ENOENT`) or
    /// another object is; any other error is returned.
    fn leaving_at(&self, rel: &Path, id: &str) -> std::io::Result<Option<(bool, File)>> {
        let gone = |e: &std::io::Error| matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR));
        let (Some(name), parent) = (rel.file_name(), rel.parent().unwrap_or(Path::new(""))) else { return Ok(None) };
        let dir = match self.disk.dir(parent) {
            Ok(dir) => dir,
            Err(e) if gone(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        // Where its handle is kept, only the object carrying it: another
        // with its id there — the copy placed again, moved there by the
        // user — is not it.
        let kept = self.store.call_blocking({ let id = id.to_owned(); move |s| s.leaving_handle(&id) }).map_err(|e| std::io::Error::other(e.to_string()))?;
        // But for that, an object with its id at its place is it — an
        // editor's save by rename made a new inode — and its handle is
        // taken anew.
        let mut renewed = None;
        if let Some(kept) = kept {
            match konedrive_fs::handle::FileHandle::at(&dir, name) {
                Ok(there) if there == kept => {}
                Ok(there) => {
                    let elsewhere = self.store.call_blocking({ let (id, rel) = (id.to_owned(), rel.to_path_buf()); move |s| s.placed_elsewhere(&id, &rel) }).map_err(|e| std::io::Error::other(e.to_string()))?;
                    if elsewhere {
                        return Ok(None);
                    }
                    renewed = Some(there);
                }
                Err(e) if gone(&e) => return Ok(None),
                // No handle to compare (a filesystem that gives none): by
                // its id, as without one.
                Err(_) => {}
            }
        }
        match self.disk.probe(&dir, name) {
            Ok(Probe::Managed { id: there, is_dir }) if there == id => {
                if let Some(handle) = renewed {
                    self.store.call_blocking({ let id = id.to_owned(); move |s| s.leaving_set_handle(&id, &handle) }).map_err(|e| std::io::Error::other(e.to_string()))?;
                }
                Ok(Some((is_dir, dir)))
            }
            Ok(_) => Ok(None),
            Err(e) if gone(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Where in the folder the leaving object of item `id` is, by the file
    /// handle taken when it began to leave: a walk of the folder, made only
    /// when its path no longer finds it.
    fn leaving_by_handle(&self, id: &str) -> Result<Option<PathBuf>, ApplyError> {
        let Some(handle) = self.store.call_blocking({ let id = id.to_owned(); move |s| s.leaving_handle(&id) })? else { return Ok(None) };
        let root = self.disk.dir(Path::new(""))?;
        let dev = nix::sys::stat::fstat(root.as_fd()).map_err(std::io::Error::from)?.st_dev;
        let mut queue = std::collections::VecDeque::from([(PathBuf::new(), root)]);
        while let Some((rel, dir)) = queue.pop_front() {
            self.check_cancel()?;
            // A directory that cannot be read may hold it: nothing is decided.
            let names = match self.disk.list(&dir) {
                Ok(names) => names,
                Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => continue,
                Err(e) => return Err(e.into()),
            };
            for name in names {
                // A file with other links: which name is the object's cannot
                // be told, so none is taken (the user's hard link).
                let single = || nix::sys::stat::fstatat(dir.as_fd(), name.as_os_str(), nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW).is_ok_and(|s| s.st_mode & libc::S_IFMT == libc::S_IFDIR || s.st_nlink <= 1);
                if konedrive_fs::handle::FileHandle::at(&dir, &name).is_ok_and(|h| h == handle) && single() {
                    return Ok(Some(rel.join(&name)));
                }
                let is_dir = match nix::sys::stat::fstatat(dir.as_fd(), name.as_os_str(), nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW) {
                    Ok(stat) => stat.st_mode & libc::S_IFMT == libc::S_IFDIR,
                    Err(nix::errno::Errno::ENOENT) => false,
                    Err(e) => return Err(std::io::Error::from(e).into()),
                };
                if is_dir {
                    match self.disk.open_subdir(&dir, &name) {
                        Ok(sub) => {
                            if nix::sys::stat::fstat(sub.as_fd()).is_ok_and(|s| s.st_dev == dev) {
                                queue.push_back((rel.join(&name), sub));
                            }
                        }
                        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
        }
        Ok(None)
    }

    /// Objects below the leaving folder at `dir/name` (at `rel`) whose items
    /// were in it when it began to leave and are gone from OneDrive since:
    /// removed as anything OneDrive removed, and their rows dropped — never
    /// uploaded again (issue #104, decision 2). An object with no item id
    /// (made here) is left to go up.
    fn remove_gone_inside(&self, rw: &Rw, dir: &File, name: &OsStr, rel: &Path, run: &mut Run) -> Result<(), ApplyError> {
        let sub = self.disk.open_subdir(dir, name)?;
        if nix::sys::stat::fstat(sub.as_fd()).map_err(std::io::Error::from)?.st_dev != nix::sys::stat::fstat(dir.as_fd()).map_err(std::io::Error::from)?.st_dev {
            return Ok(());
        }
        for child in self.disk.list(&sub)? {
            self.check_cancel()?;
            let at = rel.join(&child);
            match self.disk.probe(&sub, &child)? {
                Probe::Managed { id, is_dir } => {
                    let gone = self.store.call_blocking({ let id = id.clone(); move |s| Ok(s.get(Table::Staging, &id)?.is_none() && s.leaving_had(&id)?) })?;
                    if gone {
                        // Whole; `resyncChangesUploadDifferences` does not mean
                        // removed: what was downloaded or changed here is kept.
                        let ids = self.take_off(&sub, &child, &at, rw.removed_leaving(), run)?.ids;
                        // And the rows of the items its objects were, wherever they stand.
                        let dropped = self.store.call_blocking(move |s| s.outbox_drop_items(&ids))?;
                        tracing::info!("{} was removed from OneDrive while its folder was leaving: removed here, {} more change(s) dropped", at.display(), dropped.len());
                    } else if is_dir {
                        self.remove_gone_inside(rw, &sub, &child, &at, run)?;
                    }
                }
                Probe::Unmanaged { is_dir: true } => self.remove_gone_inside(rw, &sub, &child, &at, run)?,
                _ => {}
            }
        }
        Ok(())
    }

    /// Whether what is leaving at `rel` stays on disk for now (review fixes
    /// 4 and 5 of issue #104): an examination of it, run here in this cycle,
    /// records or holds back something; an outbox row has a place at or
    /// below it; or it holds what cannot be told or removed — a file whose
    /// state cannot be read, another filesystem mounted inside — which the
    /// examination lists as not uploaded, with its reason.
    fn keeps_leaving(&self, rw: &Rw, rel: &Path, is_dir: bool) -> Result<bool, ApplyError> {
        let mut batch = crate::local::Batch::new();
        if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
            batch.name(parent, name);
        }
        if is_dir {
            batch.tree(rel);
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let examined = crate::local::Examiner {
            disk: &self.disk,
            store: &self.store,
            liveness: &crate::local::NoLiveness,
            ignore: &rw.ignore,
            locks: &self.locks,
            now,
        }
        .examine(&batch)
        .map_err(|e| ApplyError::Io(format!("{} could not be examined before it goes: {e}", rel.display())))?;
        if !examined.applied.queued.is_empty() || !examined.recheck.is_empty() || !examined.undecided.is_empty() {
            tracing::debug!("{} waits: its examination recorded or held back something", rel.display());
            return Ok(true);
        }
        let (rows, skipped) = self.store.call_blocking({ let rel = rel.to_path_buf(); move |s| Ok((s.outbox_at_or_under(&rel)?, s.local_skipped()?)) })?;
        if !rows.is_empty() {
            tracing::debug!("{} waits for {} upload(s) before it goes", rel.display(), rows.len());
            return Ok(true);
        }
        use konedrive_tree::outbox::LocalSkip;
        if let Some(held) = skipped.iter().find(|k| k.rel.starts_with(rel) && matches!(k.reason, LocalSkip::MountedInside | LocalSkip::UnknownState)) {
            tracing::info!("{} stays on disk: {} ({})", rel.display(), held.rel.display(), held.reason);
            return Ok(true);
        }
        Ok(false)
    }
}
