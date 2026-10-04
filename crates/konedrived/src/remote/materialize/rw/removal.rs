use std::ffi::OsStr;
use std::fs::File;
use std::path::Path;

use konedrive_fs::placeholder::{self, read_state, State};

use std::os::fd::AsFd;

use crate::remote::materialize::{ApplyError, Materializer, Run};
use konedrive_fs::RESERVED_PREFIX;
use crate::status::activity::Kind as EventKind;
use crate::folder::disk::Probe;
use crate::local::names;
use super::Rw;

/// What became of something OneDrive removed ([`Materializer::remove_in_place`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::remote::materialize) enum Removal {
    /// Gone from the disk, whole.
    Gone,
    /// `resyncChangesUploadDifferences` only (§3.7): a download or local
    /// work the new listing left out stays, its attributes off, uploaded
    /// again as new — and its folder with it, made again in OneDrive.
    Kept,
}

/// How long one removal waits in all for the downloads it stopped to let go
/// of their files (a guess; the cycle waits meanwhile).
const SETTLE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// What is on disk at and below something about to be taken off it: what
/// the daemon forgets first, and the fills it stops (issue #104).
#[derive(Default)]
pub(in crate::remote::materialize) struct Survey {
    /// Item ids the objects carry.
    pub(super) ids: Vec<String>,
    /// The objects themselves.
    handles: Vec<konedrive_fs::handle::FileHandle>,
    /// The files, for their fills.
    files: Vec<crate::folder::locks::InodeKey>,
    /// The files whose fill was told to stop.
    stopped: Vec<crate::folder::locks::InodeKey>,
}

impl Survey {
    /// A survey that knows only which fills were stopped.
    pub(in crate::remote::materialize) fn stopped_only(stopped: Vec<crate::folder::locks::InodeKey>) -> Self {
        Survey { stopped, ..Survey::default() }
    }

    pub(in crate::remote::materialize) fn stopped_keys(&self) -> &[crate::folder::locks::InodeKey] {
        &self.stopped
    }
}

/// Whether an object the examination meets without an item id would be
/// uploaded: a file or a directory whose name is neither ignored, nor the
/// daemon's, nor one OneDrive refuses.
fn uploadable(rw: &Rw, dir: &File, name: &OsStr) -> std::io::Result<bool> {
    if rw.ignore.matches(name) || name.as_encoded_bytes().starts_with(RESERVED_PREFIX.as_bytes()) || names::refused(name).is_some() {
        return Ok(false);
    }
    let stat = match nix::sys::stat::fstatat(dir.as_fd(), name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(nix::errno::Errno::ENOENT) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    Ok(matches!(stat.st_mode & libc::S_IFMT, libc::S_IFREG | libc::S_IFDIR))
}

impl Materializer {
    /// What OneDrive removed, at `parent/name`, goes from the disk whole,
    /// in this cycle (issue #104): copies of OneDrive's content, files
    /// changed here, new files, files open in a program, ignored names,
    /// symlinks, objects from elsewhere — nothing is rescued, uploaded again
    /// or made again in OneDrive. Before anything goes, every object there
    /// is forgotten in the store ([`TreeStore::forget_local_objects`]), and
    /// a download into one of them stops. The rows that would still upload
    /// or move something there go too. Only `resyncChangesUploadDifferences`
    /// keeps what was downloaded or changed here (§3.7). An object that will
    /// not go fails the cycle.
    ///
    /// [`TreeStore::forget_local_objects`]: konedrive_tree::TreeStore::forget_local_objects
    pub(in crate::remote::materialize) fn remove_in_place(&self, rw: &Rw, parent: &Path, name: &OsStr, run: &mut Run) -> Result<Removal, ApplyError> {
        let rel = parent.join(name);
        let dir = self.disk.dir(parent)?;
        if matches!(self.disk.probe(&dir, name)?, Probe::Absent) {
            return Ok(Removal::Gone);
        }
        let survey = self.forget_before_removing(&dir, name, true)?;
        run.out.taken.extend(survey.ids.iter().cloned());
        let outcome = self.remove_whole(rw.upload_differences.then_some(rw), &dir, name, &rel, run);
        if outcome.is_err() {
            self.settle_stopped(&dir, name, &survey);
        }
        let outcome = outcome?;
        let dropped = self.store.call_blocking({ let rel = rel.clone(); move |s| s.outbox_drop_under(&rel) })?;
        if !dropped.is_empty() {
            tracing::info!("{} was removed from OneDrive: {} change(s) waiting there are dropped", rel.display(), dropped.len());
        }
        Ok(outcome)
    }

    /// Decision 5 of issue #104: before anything at `dir/name` is taken off
    /// the disk, the store forgets every local object there — and, `by_id`,
    /// the local objects of every item whose id an object there carries, and
    /// of everything the tree has below it — and fills into its files stop.
    /// What was found.
    pub(in crate::remote::materialize) fn forget_before_removing(&self, dir: &File, name: &OsStr, by_id: bool) -> Result<Survey, ApplyError> {
        let mut survey = Survey::default();
        let dev = nix::sys::stat::fstat(dir.as_fd()).map_err(std::io::Error::from)?.st_dev;
        self.survey(dir, name, dev, &mut survey)?;
        let ids = if by_id { survey.ids.clone() } else { Vec::new() };
        let handles = survey.handles.clone();
        self.store.call_blocking(move |s| s.forget_local_objects(&ids, &handles))?;
        for key in survey.files.clone() {
            if self.locks.cancel(key) {
                tracing::info!("a download into a file being removed is stopped");
                survey.stopped.push(key);
            }
        }
        Ok(survey)
    }

    fn survey(&self, dir: &File, name: &OsStr, dev: libc::dev_t, survey: &mut Survey) -> Result<(), ApplyError> {
        let stat = match nix::sys::stat::fstatat(dir.as_fd(), name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(nix::errno::Errno::ENOENT) => return Ok(()),
            Err(e) => return Err(std::io::Error::from(e).into()),
        };
        if let Ok(handle) = konedrive_fs::handle::FileHandle::at(dir, name) {
            survey.handles.push(handle);
        }
        let probed = self.disk.probe(dir, name)?;
        if let Probe::Managed { id, .. } = &probed {
            survey.ids.push(id.clone());
        }
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFREG => survey.files.push(crate::folder::locks::InodeKey { dev: stat.st_dev as u64, ino: stat.st_ino as u64 }),
            // Never into another filesystem mounted here: removing the
            // directory it is mounted on fails the cycle instead.
            libc::S_IFDIR if stat.st_dev == dev => {
                let sub = self.disk.open_subdir(dir, name)?;
                for child in self.disk.list(&sub)? {
                    self.survey(&sub, &child, dev, survey)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Takes `dir/name` (at `rel`) off the disk with everything below it.
    /// `keep` is the plan of a `resyncChangesUploadDifferences` listing,
    /// which keeps downloads and local work (§3.7).
    pub(super) fn remove_whole(&self, keep: Option<&Rw>, dir: &File, name: &OsStr, rel: &Path, run: &mut Run) -> Result<Removal, ApplyError> {
        self.check_cancel()?;
        let probed = self.disk.probe(dir, name)?;
        let id = match &probed {
            Probe::Absent => return Ok(Removal::Gone),
            Probe::Managed { id, .. } => Some(id.clone()),
            Probe::Unmanaged { .. } => None,
        };
        let is_dir = matches!(probed, Probe::Managed { is_dir: true, .. } | Probe::Unmanaged { is_dir: true });
        if is_dir {
            let sub = self.disk.open_subdir(dir, name)?;
            let mut outcome = Removal::Gone;
            if nix::sys::stat::fstat(sub.as_fd()).map_err(std::io::Error::from)?.st_dev == nix::sys::stat::fstat(dir.as_fd()).map_err(std::io::Error::from)?.st_dev {
                for child in self.disk.list(&sub)? {
                    outcome = outcome.max(self.remove_whole(keep, &sub, &child, &rel.join(&child), run)?);
                }
            }
            if outcome == Removal::Kept {
                if let Some(id) = id {
                    placeholder::strip_konedrive_xattrs(&sub)?;
                    tracing::info!("{} is not in OneDrive's new listing but holds local work: it stays, and is made again there", rel.display());
                    run.out.recreated.push(id);
                }
                run.out.examine.push((rel.to_path_buf(), true));
                return Ok(Removal::Kept);
            }
            self.disk.remove(dir, name, true)?;
        } else {
            if let Some(rw) = keep {
                if self.kept_by_resync(rw, dir, name, id.is_some())? {
                    run.out.examine.push((rel.to_path_buf(), false));
                    return Ok(Removal::Kept);
                }
            }
            if id.is_some() {
                self.release_other_names(dir, name, rel)?;
            }
            self.disk.remove(dir, name, false)?;
        }
        if id.is_some() {
            run.out.deleted += 1;
            run.note(EventKind::Removed, rel, None);
        }
        Ok(Removal::Gone)
    }

    /// A file of ours about to be taken off the disk that has other names —
    /// hard links the user made — keeps them, but not as the item: its item
    /// id goes from the inode first, so that what stays is the user's own
    /// file, never the item under another name (issue #104, decision 1). A
    /// downloaded one goes up as new; one not downloaded waits as not
    /// downloaded, its state kept, so that it is never read as zeros.
    fn release_other_names(&self, dir: &File, name: &OsStr, rel: &Path) -> Result<(), ApplyError> {
        let stat = match nix::sys::stat::fstatat(dir.as_fd(), name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(nix::errno::Errno::ENOENT) => return Ok(()),
            Err(e) => return Err(std::io::Error::from(e).into()),
        };
        if stat.st_nlink <= 1 {
            return Ok(());
        }
        let file = self.disk.open_file(dir, name)?;
        match xattr::FileExt::remove_xattr(&file, placeholder::XATTR_ITEM_ID) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::ENODATA) => {}
            Err(e) => return Err(e.into()),
        }
        tracing::info!("{} has other names, which stay as the user's own files", rel.display());
        Ok(())
    }

    /// `resyncChangesUploadDifferences` (§3.7): a download, a file with local
    /// work, or a new file the new listing left out stays, to be uploaded
    /// again as new — its konedrive attributes off.
    fn kept_by_resync(&self, rw: &Rw, dir: &File, name: &OsStr, managed: bool) -> Result<bool, ApplyError> {
        if !managed {
            return Ok(uploadable(rw, dir, name)?);
        }
        let file = self.disk.open_file(dir, name)?;
        if !(matches!(read_state(&file), Ok(Some(State::Hydrated))) || self.local_work(&file)) {
            return Ok(false);
        }
        placeholder::strip_konedrive_xattrs(&file)?;
        tracing::info!("{} is not in OneDrive's new listing and was downloaded here: it stays, and is uploaded again", name.to_string_lossy());
        Ok(true)
    }

    /// After a removal that failed: a file whose download was stopped for it
    /// and is still here is a placeholder again, never partly filled
    /// (review fix 6 of issue #104).
    pub(in crate::remote::materialize) fn settle_stopped(&self, dir: &File, name: &OsStr, survey: &Survey) {
        if survey.stopped.is_empty() {
            return;
        }
        let mut files = Vec::new();
        let dev = match nix::sys::stat::fstat(dir.as_fd()) {
            Ok(stat) => stat.st_dev,
            Err(_) => return,
        };
        self.files_below(dir, name, dev, &mut files);
        // 10 s in all for the removal, not for each file: the cycle waits.
        let deadline = std::time::Instant::now() + SETTLE_WAIT;
        for file in files {
            let Ok(key) = crate::folder::locks::InodeKey::of(&file) else { continue };
            if !survey.stopped.contains(&key) {
                continue;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            // The fill lets go of the lock once it has stopped.
            let guard = self.runtime.block_on(async { tokio::time::timeout(left, self.locks.lock(key)).await });
            match guard {
                Ok(_guard) => crate::hydration::source::back_to_placeholder(&file),
                Err(_) => tracing::warn!("a stopped download did not let go of its file in time; it is left as it is"),
            }
        }
    }

    /// The files at or below `dir/name`, opened, not into another filesystem.
    fn files_below(&self, dir: &File, name: &OsStr, dev: libc::dev_t, out: &mut Vec<File>) {
        match self.disk.probe(dir, name) {
            Ok(Probe::Managed { is_dir: false, .. } | Probe::Unmanaged { is_dir: false }) => {
                if let Ok(file) = self.disk.open_file(dir, name) {
                    out.push(file);
                }
            }
            Ok(Probe::Managed { is_dir: true, .. } | Probe::Unmanaged { is_dir: true }) => {
                let Ok(sub) = self.disk.open_subdir(dir, name) else { return };
                if nix::sys::stat::fstat(sub.as_fd()).map(|s| s.st_dev) != Ok(dev) {
                    return;
                }
                for child in self.disk.list(&sub).unwrap_or_default() {
                    self.files_below(&sub, &child, dev, out);
                }
            }
            _ => {}
        }
    }
}
