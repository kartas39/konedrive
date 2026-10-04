//! The one way the daemon takes a managed object off the disk
//! ([`Materializer::take_off`]): survey, forget, remove, settle.
//!
//! The store records, for each placed item, the local object it was placed
//! as. An object the daemon unlinks while that record stays is, to the next
//! examination, the user's own delete, and is deleted in OneDrive. So the
//! record is cleared before anything is unlinked, here and nowhere else
//! (issue #104, decision 5).

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::AsFd;
use std::path::Path;

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{self, read_state, State};
use konedrive_fs::RESERVED_PREFIX;
use konedrive_tree::outbox::OutboxKind;
use konedrive_tree::Table;

use super::{holds_local_work, ApplyError, Materializer, Run, Rw};
use crate::folder::disk::Probe;
use crate::folder::locks::InodeKey;
use crate::local::names;
use crate::status::activity::Kind as EventKind;

/// Why an object is taken off the disk, and so what of it may stay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Policy {
    /// Its item was removed in OneDrive. Read-write mode: what OneDrive
    /// had and the daemon placed goes — a file not downloaded, the removed
    /// items' own downloads unchanged since, a folder once nothing is left
    /// in it. What OneDrive never had stays, as the user's own, with the
    /// folders above it: a file with no item id, whatever its name (but an
    /// empty one with an ignored name); a directory with no id and an
    /// ignored name that holds anything; a download changed here (its
    /// stamp differs, it is open for writing, or an `update` waits for
    /// it); a download whose id is not of what was removed. It goes up as
    /// new, unless its name, or a folder's above it, is one nothing
    /// uploads (F243).
    /// Read-only mode: what holds local work is rescued out of the folder,
    /// and another account's object is set aside.
    Removed,
    /// Read-write mode, a `resyncChangesUploadDifferences` listing that
    /// left its item out (§3.7): as [`Policy::Removed`], and every download
    /// stays too, changed or not: the listing may have lost the item.
    Resync,
    /// Read-write mode: its item was removed in OneDrive while it, or the
    /// folder it is in, was leaving. It goes whole, whatever is in it, as
    /// decision 2 of issue #104 had it for everything: what stays in a
    /// leaving folder would be examined by that folder's own rules. Goes
    /// with the leaving rows.
    RemovedLeaving,
    /// Read-write mode: it stopped being placed and nothing in it waits any
    /// more (`rw::leaving`, which decides that; until the leaving rows go,
    /// this stands where a policy for what is no longer placed will).
    /// `placed_elsewhere`: the item's own object is another one now, so
    /// only the objects found here are forgotten, not the item.
    Leaving { placed_elsewhere: bool },
}

/// What [`Materializer::take_off`] did.
pub(super) struct TakenOff {
    pub(super) removal: Removal,
    /// The item ids the objects found there carried.
    pub(super) ids: Vec<String>,
}

/// What became of what was taken off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Removal {
    /// Gone from the place, whole.
    Gone,
    /// Something in it stays, as the user's own.
    Kept,
}

/// How long one removal waits in all for the downloads it stopped to let go
/// of their files (a guess; the cycle waits meanwhile).
const SETTLE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// What is on disk at and below something about to be taken off it.
#[derive(Default)]
struct Survey {
    /// Something is there.
    found: bool,
    /// Item ids the objects carry.
    ids: Vec<String>,
    /// The objects themselves.
    handles: Vec<FileHandle>,
    /// The files, for their fills.
    files: Vec<InodeKey>,
    /// The files that have other names — hard links the user made.
    linked: Vec<FileHandle>,
}

/// Which managed files a read-write removal keeps.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Keep {
    /// None: everything goes ([`Policy::Leaving`]).
    Nothing,
    /// Downloads changed here ([`Policy::Removed`]).
    Changed,
    /// Every download ([`Policy::Resync`]).
    Downloaded,
}

/// What a read-write removal goes by, for everything below one place.
struct Whole<'a> {
    rw: &'a Rw,
    keep: Keep,
    /// The item removed and what the base has below it: only an object
    /// carrying one of these ids is a copy of what OneDrive had.
    items: HashSet<String>,
}

/// What an object with no item id is to a removal that keeps local work.
#[derive(PartialEq, Eq)]
enum Unmanaged {
    /// The user's own, with data OneDrive never had: a file, whatever its
    /// name, but for an empty one with an ignored name; a directory with an
    /// ignored name that holds anything, which is not entered. It stays,
    /// and its folder with it.
    Theirs,
    /// Nothing to lose: a symlink, a socket, an empty file or an empty
    /// directory with an ignored name. It stays where its folder stays,
    /// and goes with a folder that goes.
    Beside,
    /// The daemon's own (a temporary name): it goes.
    Ours,
    /// A directory to look into.
    Folder,
}

impl Materializer {
    fn unmanaged(&self, rw: &Rw, dir: &File, name: &OsStr) -> std::io::Result<Unmanaged> {
        if name.as_encoded_bytes().starts_with(RESERVED_PREFIX.as_bytes()) {
            return Ok(Unmanaged::Ours);
        }
        let stat = match nix::sys::stat::fstatat(dir.as_fd(), name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(nix::errno::Errno::ENOENT) => return Ok(Unmanaged::Ours),
            Err(e) => return Err(e.into()),
        };
        let ignored = rw.ignore.matches(name);
        Ok(match stat.st_mode & libc::S_IFMT {
            libc::S_IFREG if ignored && stat.st_size == 0 => Unmanaged::Beside,
            libc::S_IFREG => Unmanaged::Theirs,
            libc::S_IFDIR if !ignored => Unmanaged::Folder,
            libc::S_IFDIR if self.disk.list(&self.disk.open_subdir(dir, name)?)?.is_empty() => Unmanaged::Beside,
            libc::S_IFDIR => Unmanaged::Theirs,
            _ => Unmanaged::Beside,
        })
    }
}

/// Whether nothing uploads an object of this name once it is the user's
/// own: the examination ignores it, or OneDrive refuses it.
fn stays_local(rw: &Rw, name: &OsStr) -> bool {
    rw.ignore.matches(name) || names::refused(name).is_some()
}

fn device(dir: &File) -> std::io::Result<libc::dev_t> {
    Ok(nix::sys::stat::fstat(dir.as_fd()).map_err(std::io::Error::from)?.st_dev)
}

impl Materializer {
    /// Takes `dir/name` (at `rel`) off the disk with everything below it,
    /// as `policy` says, in four steps:
    ///
    /// 1. what is there is surveyed, not into another filesystem;
    /// 2. the store forgets every object found, and the items whose ids
    ///    they carry with everything the tree has below them, in one
    ///    transaction ([`TreeStore::forget_local_objects`]); downloads into
    ///    the files there are told to stop;
    /// 3. it is removed. An object that will not go fails the cycle, and a
    ///    file whose download was stopped and is still here is a
    ///    placeholder again first;
    /// 4. read-write mode: the outbox rows that would still upload, create
    ///    or move something at or below the place go.
    ///
    /// Nothing there is `Gone` at once.
    ///
    /// [`TreeStore::forget_local_objects`]: konedrive_tree::TreeStore::forget_local_objects
    pub(super) fn take_off(&self, dir: &File, name: &OsStr, rel: &Path, policy: Policy, run: &mut Run) -> Result<TakenOff, ApplyError> {
        let survey = self.survey(dir, name)?;
        if !survey.found {
            return Ok(TakenOff { removal: Removal::Gone, ids: Vec::new() });
        }
        let stopped = self.forget(&survey, policy, run)?;
        let before = run.kept;
        let removed = match &self.rw {
            Some(rw) => {
                let keep = match policy {
                    Policy::Removed => Keep::Changed,
                    Policy::Resync => Keep::Downloaded,
                    Policy::RemovedLeaving | Policy::Leaving { .. } => Keep::Nothing,
                };
                let items = if keep == Keep::Nothing { HashSet::new() } else { self.items_at(dir, name)? };
                self.remove_whole(&Whole { rw, keep, items }, dir, name, rel, false, run)
            }
            None => self.remove_rescuing(dir, name, rel, &stopped, run).map(|()| Removal::Gone),
        };
        // What stays is said whatever became of the rest: its attributes
        // are off already.
        let kept = run.kept.since(before);
        if !kept.is_empty() {
            tracing::info!("{} is gone from OneDrive: {} file(s) stay to be uploaded as new, {} on this computer only", rel.display(), kept.uploaded, kept.local);
            run.out.on_disk.note_kept(rel, kept);
        }
        let removal = match removed {
            Ok(removal) => removal,
            Err(e) => {
                self.settle_stopped(dir, name, &stopped);
                return Err(e);
            }
        };
        if self.rw.is_some() {
            let dropped = self.store.call_blocking({ let rel = rel.to_path_buf(); move |s| s.outbox_drop_under(&rel) })?;
            if !dropped.is_empty() {
                tracing::info!("{} was taken off the disk: {} change(s) waiting there are dropped", rel.display(), dropped.len());
            }
        }
        Ok(TakenOff { removal, ids: survey.ids })
    }

    /// The item whose object stands at `dir/name`, with the items the base
    /// has below it; none when the object carries no id.
    fn items_at(&self, dir: &File, name: &OsStr) -> Result<HashSet<String>, ApplyError> {
        let Probe::Managed { id, .. } = self.disk.probe(dir, name)? else { return Ok(HashSet::new()) };
        Ok(self.store.call_blocking(move |s| {
            let mut items: HashSet<String> = s.descendants(Table::Items, &id)?.into_iter().collect();
            items.insert(id);
            Ok(items)
        })?)
    }

    fn survey(&self, dir: &File, name: &OsStr) -> Result<Survey, ApplyError> {
        let mut survey = Survey::default();
        self.survey_below(dir, name, device(dir)?, &mut survey)?;
        Ok(survey)
    }

    fn survey_below(&self, dir: &File, name: &OsStr, dev: libc::dev_t, survey: &mut Survey) -> Result<(), ApplyError> {
        let stat = match nix::sys::stat::fstatat(dir.as_fd(), name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(nix::errno::Errno::ENOENT) => return Ok(()),
            Err(e) => return Err(std::io::Error::from(e).into()),
        };
        survey.found = true;
        if let Ok(handle) = FileHandle::at(dir, name) {
            if stat.st_mode & libc::S_IFMT == libc::S_IFREG && stat.st_nlink > 1 {
                survey.linked.push(handle.clone());
            }
            survey.handles.push(handle);
        }
        if let Probe::Managed { id, .. } = self.disk.probe(dir, name)? {
            survey.ids.push(id);
        }
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFREG => survey.files.push(InodeKey { dev: stat.st_dev as u64, ino: stat.st_ino as u64 }),
            // Never into another filesystem mounted here: removing the
            // directory it is mounted on fails the cycle instead.
            libc::S_IFDIR if stat.st_dev == dev => {
                let sub = self.disk.open_subdir(dir, name)?;
                for child in self.disk.list(&sub)? {
                    self.survey_below(&sub, &child, dev, survey)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Step 2 of [`Self::take_off`]. The files whose download was told to stop.
    fn forget(&self, survey: &Survey, policy: Policy, run: &mut Run) -> Result<Vec<InodeKey>, ApplyError> {
        let ids = match policy {
            Policy::Leaving { placed_elsewhere: true } => Vec::new(),
            _ => survey.ids.clone(),
        };
        let (handles, linked) = (survey.handles.clone(), survey.linked.clone());
        self.store.call_blocking({
            let ids = ids.clone();
            move |s| {
                // A file with other names is not followed by its handle
                // once its name here may be gone: the names that stay carry
                // the same handle, and are the user's own (F238).
                s.leaving_forget_handles(&linked)?;
                s.forget_local_objects(&ids, &handles)
            }
        })?;
        if !matches!(policy, Policy::Leaving { .. }) {
            // Forgotten and on their way out: the base takes their removal
            // in this cycle, whatever holds them.
            run.out.on_disk.taken.extend(ids);
        }
        let mut stopped = Vec::new();
        for &key in &survey.files {
            if self.locks.cancel(key) {
                tracing::info!("a download into a file being removed is stopped");
                stopped.push(key);
            }
        }
        Ok(stopped)
    }

    /// Read-write mode's step 3: `dir/name` (at `rel`) goes, with what is
    /// below it, but for what [`Keep`] and [`Unmanaged`] say stays. What
    /// stays loses konedrive's attributes and is handed to the examination,
    /// which records it as new; a folder that stays is made again in
    /// OneDrive. Nothing is rescued out of the folder. `local`: a folder
    /// above it, inside what is removed, has a name nothing uploads.
    fn remove_whole(&self, all: &Whole, dir: &File, name: &OsStr, rel: &Path, local: bool, run: &mut Run) -> Result<Removal, ApplyError> {
        self.check_cancel()?;
        let probed = self.disk.probe(dir, name)?;
        let id = match &probed {
            Probe::Absent => return Ok(Removal::Gone),
            Probe::Managed { id, .. } => Some(id.clone()),
            Probe::Unmanaged { .. } => None,
        };
        let keeps = all.keep != Keep::Nothing;
        let is_dir = matches!(probed, Probe::Managed { is_dir: true, .. } | Probe::Unmanaged { is_dir: true });
        // A directory of the user's own with an ignored name is one thing,
        // kept as it is and not looked into.
        if keeps && is_dir && id.is_none() && self.unmanaged(all.rw, dir, name)? == Unmanaged::Theirs {
            run.kept.local += 1;
            return Ok(Removal::Kept);
        }
        if is_dir {
            let sub = self.disk.open_subdir(dir, name)?;
            let local = local || stays_local(all.rw, name);
            let mut outcome = Removal::Gone;
            // What stays only where its folder stays, and whether it is a directory.
            let mut beside = Vec::new();
            if device(&sub)? == device(dir)? {
                for child in self.disk.list(&sub)? {
                    let unmarked = keeps && matches!(self.disk.probe(&sub, &child)?, Probe::Unmanaged { .. });
                    if unmarked && self.unmanaged(all.rw, &sub, &child)? == Unmanaged::Beside {
                        let is_dir = matches!(self.disk.probe(&sub, &child)?, Probe::Unmanaged { is_dir: true });
                        beside.push((child, is_dir));
                    } else {
                        outcome = outcome.max(self.remove_whole(all, &sub, &child, &rel.join(&child), local, run)?);
                    }
                }
            }
            if outcome == Removal::Kept {
                if let Some(id) = id {
                    placeholder::strip_konedrive_xattrs(&sub)?;
                    tracing::info!("{} is gone from OneDrive but holds local work: it stays, and is made again there", rel.display());
                    run.out.on_disk.recreated.push(id);
                    // The user's own from now on: no later cycle takes it
                    // off again, so what stays in it is said by this one.
                    run.kept.stripped += 1;
                }
                run.out.on_disk.examine.push((rel.to_path_buf(), true));
                return Ok(Removal::Kept);
            }
            for (child, is_dir) in beside {
                self.disk.remove(&sub, &child, is_dir)?;
            }
            self.disk.remove(dir, name, true)?;
        } else {
            if let Some(stripped) = self.stays(all, dir, name, id.as_deref())? {
                if local || stays_local(all.rw, name) {
                    run.kept.local += 1;
                } else {
                    run.kept.uploaded += 1;
                }
                run.kept.stripped += u64::from(stripped);
                run.out.on_disk.examine.push((rel.to_path_buf(), false));
                return Ok(Removal::Kept);
            }
            let other_names = if id.is_some() { self.with_other_names(dir, name)? } else { None };
            self.disk.remove(dir, name, false)?;
            if let Some(file) = other_names {
                #[cfg(test)]
                if self.disk.dir(Path::new("")).is_ok_and(|root| testing::stops_after_unlink(&root)) {
                    return Err(ApplyError::Io(format!("{}: stopped after the unlink (test)", rel.display())));
                }
                release_other_names(&file, rel);
            }
        }
        if id.is_some() {
            run.out.counts.deleted += 1;
            run.note(EventKind::Removed, rel, None);
        }
        Ok(Removal::Gone)
    }

    /// The file at `dir/name`, opened, when it has other names — hard links
    /// the user made — and `None` when this is its only one.
    fn with_other_names(&self, dir: &File, name: &OsStr) -> Result<Option<File>, ApplyError> {
        use std::os::unix::fs::MetadataExt;
        let stat = match nix::sys::stat::fstatat(dir.as_fd(), name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(nix::errno::Errno::ENOENT) => return Ok(None),
            Err(e) => return Err(std::io::Error::from(e).into()),
        };
        if stat.st_nlink <= 1 {
            return Ok(None);
        }
        let file = self.disk.open_file(dir, name)?;
        // The inode looked at, not one put under the name since.
        Ok(Some(file).filter(|file| file.metadata().is_ok_and(|m| m.ino() == stat.st_ino as u64 && m.dev() == stat.st_dev as u64)))
    }

    /// Whether the file at `dir/name` stays when its place is taken off;
    /// and, when it does, whether konedrive's attributes were taken off it.
    ///
    /// With no item id: a file of the user's ([`Unmanaged::Theirs`]). With
    /// one (`id`): a download that holds what OneDrive never had — changed
    /// here, open for writing, with an `update` waiting, or not an item of
    /// what was removed at all (moved in from another folder, not examined
    /// yet) — or, for [`Keep::Downloaded`], any download. Its attributes
    /// come off: it is the user's own. A file that is not downloaded never
    /// stays: without its attributes it would be read as zeros.
    ///
    /// Between this look and the unlink of a file that does not stay a
    /// program can still open it for writing: the window every removal has.
    fn stays(&self, all: &Whole, dir: &File, name: &OsStr, id: Option<&str>) -> Result<Option<bool>, ApplyError> {
        if all.keep == Keep::Nothing {
            return Ok(None);
        }
        let Some(id) = id else {
            return Ok((self.unmanaged(all.rw, dir, name)? == Unmanaged::Theirs).then_some(false));
        };
        let file = self.disk.open_file(dir, name)?;
        let downloaded = matches!(read_state(&file), Ok(Some(State::Hydrated)));
        let stays = self.local_work(&file)
            || downloaded
                && (all.keep == Keep::Downloaded
                    || !all.items.contains(id)
                    || konedrive_fs::lease::open_for_writing(&file).unwrap_or(true)
                    || self.store.call_blocking({ let id = id.to_owned(); move |s| s.outbox_for_item(&id) })?.iter().any(|row| row.kind == OutboxKind::Update));
        if !stays {
            return Ok(None);
        }
        placeholder::strip_konedrive_xattrs(&file)?;
        tracing::info!("{} is gone from OneDrive and holds what OneDrive never had, or was downloaded here: it stays", name.to_string_lossy());
        Ok(Some(true))
    }

    /// Read-only mode's step 3: what the cloud no longer has is deleted,
    /// but anything that would lose a local byte is rescued out of the
    /// folder instead, and another account's object is set aside, alive.
    /// `stopped`: the files below whose download was told to stop.
    fn remove_rescuing(&self, dir: &File, name: &OsStr, shown: &Path, stopped: &[InodeKey], run: &mut Run) -> Result<(), ApplyError> {
        match self.disk.probe(dir, name)? {
            Probe::Absent => Ok(()),
            Probe::Unmanaged { .. } => self.rescue(dir, name, shown, run),
            Probe::Managed { id, .. } if self.claimed_elsewhere(&id)? => {
                // It survives, out of the folder: a download stopped in it is
                // a placeholder again first (issue #104).
                self.settle_stopped(dir, name, stopped);
                self.set_aside(dir, name, shown, run)
            }
            Probe::Managed { is_dir: true, .. } => {
                let sub = self.disk.open_subdir(dir, name)?;
                for child in self.disk.list(&sub)? {
                    self.remove_rescuing(&sub, &child, &shown.join(&child), stopped, run)?;
                }
                self.disk.remove(dir, name, true)?;
                run.out.counts.deleted += 1;
                Ok(())
            }
            Probe::Managed { is_dir: false, .. } => {
                let file = self.disk.open_file(dir, name)?;
                if holds_local_work(&file) {
                    self.rescue(dir, name, shown, run)
                } else {
                    self.disk.remove(dir, name, false)?;
                    run.out.counts.deleted += 1;
                    Ok(())
                }
            }
        }
    }

    /// A file whose download was stopped for a removal, and which is still
    /// at or below `dir/name` — the removal failed, or it survives out of
    /// the folder — is a placeholder again, never partly filled.
    fn settle_stopped(&self, dir: &File, name: &OsStr, stopped: &[InodeKey]) {
        if stopped.is_empty() {
            return;
        }
        let Ok(dev) = device(dir) else { return };
        let mut files = Vec::new();
        self.files_below(dir, name, dev, &mut files);
        // 10 s in all for the removal, not for each file: the cycle waits.
        let deadline = std::time::Instant::now() + SETTLE_WAIT;
        for file in files {
            let Ok(key) = InodeKey::of(&file) else { continue };
            if !stopped.contains(&key) {
                continue;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            // The fill lets go of the lock once it has stopped.
            let guard = self.runtime.block_on(async { tokio::time::timeout(left, self.locks.lock(key)).await });
            match guard {
                Ok(_guard) => crate::hydration::demote::back_to_placeholder(&file),
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
                if device(&sub).ok() != Some(dev) {
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

/// A file of ours whose name was just unlinked and which has other names —
/// hard links the user made — keeps them, but not as the item: its item id
/// goes from the inode, through the descriptor opened before the unlink, so
/// that what stays is the user's own file, never the item under another
/// name (issue #104, decision 1). A downloaded one goes up as new; one not
/// downloaded waits as not downloaded, its state kept, so that it is never
/// read as zeros.
///
/// The name goes first (issue #112): a stop between the two leaves the
/// other names with the id and no object at the item's place, never the
/// object at its place without an id, which would be uploaded as new. For
/// the same reason an id that cannot be taken off does not fail the cycle:
/// the name is gone already, and no later cycle could take it off.
fn release_other_names(file: &File, rel: &Path) {
    match placeholder::remove_item_id(file) {
        Ok(()) => tracing::info!("{} had other names, which stay as the user's own files", rel.display()),
        Err(e) => tracing::warn!("{} had other names, which keep its item id ({e})", rel.display()),
    }
}

/// Tests only: a stop of the daemon between the unlink of a file with
/// other names and the removal of its item id.
#[cfg(test)]
pub(super) mod testing;
