use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::SystemTime;

use tokio_util::sync::CancellationToken;

use konedrive_fs::placeholder::{self, read_state, State, LOCKED_FILE_MODE, OPEN_FILE_MODE};

use crate::folder::disk::{Disk, Probe, NEW_PREFIX};
use crate::hydration::source::{ContentSource, Downloaded};
use crate::folder::locks::{InodeKey, InodeLocks};
use konedrive_tree::Store;
use super::{holds_local_work, holds_local_work_rw, Replacement};

#[derive(Debug)]
pub enum ReplaceOutcome {
    Replaced,
    /// Nothing to do any more: the file moved, changed, was freed up or is
    /// already this version. The next cycle looks again.
    Current,
    /// The old version stays; said, and tried again after every cycle.
    Failed(Failure),
    /// Read-write mode: someone has the file open, so no write lease (write
    /// design §3.7). The old version stays, and so does its base; the next
    /// cycle tries again. Not a failure.
    Busy,
}

/// Why a replacement failed: the [reason](FailureReason), which is what tells
/// one failure from another, and the words the status says it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub reason: FailureReason,
    pub text: String,
}

/// What a replacement failed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureReason {
    /// Want of disk space: the disk cannot hold both versions, or filled up
    /// while the new one downloaded. The activity log says exactly "not
    /// enough disk space" for it.
    NoSpace,
    /// The new version could not be had from OneDrive: the errno the
    /// download ended with.
    Download(i32),
    /// A file call of the replacement failed: the errno it failed with.
    Errno(i32),
    /// A file call failed with an error that is not the system's: its kind.
    Io(std::io::ErrorKind),
    /// The folder could not be opened: it no longer carries its root id, or
    /// is not there.
    Folder,
    /// The task the replacement ran on failed.
    Task,
}

impl Failure {
    fn no_space(text: String) -> ReplaceOutcome {
        ReplaceOutcome::Failed(Failure { reason: FailureReason::NoSpace, text })
    }

    fn io(e: &std::io::Error) -> ReplaceOutcome {
        let reason = e.raw_os_error().map_or(FailureReason::Io(e.kind()), FailureReason::Errno);
        ReplaceOutcome::Failed(Failure { reason, text: e.to_string() })
    }
}

/// Read-write mode's replacement (`docs/design/writes.md` §9): the swap runs
/// under the per-root tree lock and a write lease on the old file, and the
/// new version's deferred change becomes the base as it lands.
pub struct Leased<'a> {
    pub tree_lock: &'a std::sync::Arc<tokio::sync::Mutex<()>>,
    pub store: &'a Store,
}

/// The margin `replace` keeps free beside the new version's own bytes when
/// checking whether the disk can hold both at once: room for
/// filesystem bookkeeping and whatever else is filling or freeing up
/// concurrently, not just the new version's exact byte count.
const REPLACE_SPACE_MARGIN: u64 = 64 << 20;

/// `disk.dir(parent)` for a replacement already in flight: a folder above the
/// file having moved or been removed since is not a failure to report and
/// retry forever — it is nothing left to do here; the file
/// is wherever the tree now says it is, and the next cycle looks there. Any
/// other error is still an error.
fn replacement_dir(disk: &Disk, parent: &Path) -> std::io::Result<Option<File>> {
    match disk.dir(parent) {
        Ok(dir) => Ok(Some(dir)),
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// A file's identity strong enough to survive `old` being dropped and
/// reopened later, rather than kept open for the whole download (round 1,
/// issue 3). `(dev, ino)` alone is not enough (round 2's finding): once its
/// descriptor is closed, nothing pins the inode number — it can be freed and
/// reused by an unrelated file created while the new version downloads, and
/// the two would then compare equal. [`Fingerprint`] tells a reused inode
/// apart from the file that had it before.
#[derive(Debug, Clone, Copy, PartialEq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    fingerprint: Fingerprint,
}

/// What distinguishes a file from another that happens to reuse its inode:
/// its birth time where the filesystem reports one (`statx`'s `btime` on
/// Linux, behind `Metadata::created()`), or — where it does not — its mtime
/// and size, which the stamp check already constrains for any file
/// `replace` considers taking as "the same old file".
#[derive(Debug, Clone, Copy, PartialEq)]
enum Fingerprint {
    Born(SystemTime),
    Stamp { mtime: SystemTime, size: u64 },
}

impl FileIdentity {
    fn of(file: &File) -> std::io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        let fingerprint = match meta.created() {
            Ok(created) => Fingerprint::Born(created),
            Err(_) => Fingerprint::Stamp { mtime: meta.modified()?, size: meta.len() },
        };
        Ok(Self { dev: meta.dev(), ino: meta.ino(), fingerprint })
    }
}

/// The new version of a downloaded file is fetched into a nameless
/// file beside it, verified, labelled and swapped in with one rename. Never
/// written over the old one in place — a reader would see a mix. If the disk
/// cannot hold both, the old version stays.
pub async fn replace(disk: &Disk, locks: &InodeLocks, source: &dyn ContentSource, r: &Replacement) -> ReplaceOutcome {
    replace_leased(disk, locks, source, r, None).await
}

/// [`replace`], in read-write mode when `leased` is given ([`Leased`]): a
/// file someone has open is not downloaded again nor swapped
/// ([`ReplaceOutcome::Busy`]) — a writer would lose what it writes into the
/// unlinked inode.
pub async fn replace_leased(disk: &Disk, locks: &InodeLocks, source: &dyn ContentSource, r: &Replacement, leased: Option<&Leased<'_>>) -> ReplaceOutcome {
    let never = CancellationToken::new();
    replace_until(disk, locks, source, r, leased, &never).await.expect("nothing cancels this token")
}

/// [`replace_leased`], given up when `stop` is cancelled (the poller stops):
/// `None`, with the old version in place. Only its waits are given up — the
/// download and the waits for the two locks. A run of file calls that has
/// begun (a section) is waited for, so nothing of a replacement is still at
/// work when this returns, and a swap that began is `Replaced`.
pub async fn replace_until(
    disk: &Disk,
    locks: &InodeLocks,
    source: &dyn ContentSource,
    r: &Replacement,
    leased: Option<&Leased<'_>>,
    stop: &CancellationToken,
) -> Option<ReplaceOutcome> {
    match replace_inner(disk, locks, source, r, leased, stop).await {
        Ok(outcome) => outcome,
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT)) => Some(Failure::no_space(format!(
            "not enough space to finish the new version of {}; the old version stays",
            r.rel.display()
        ))),
        Err(e) => Some(Failure::io(&e)),
    }
}

/// A replacement's file calls wait on the disk (`fsync`, the rename, the
/// attributes), so none is made on a runtime thread: each run of them is one
/// *section* on a blocking thread, with the download between the first two.
///
/// `stop` ends the replacement only where it waits, between sections: there
/// it could be dropped before the sections were. A section is never left
/// behind by a stop (`docs/limitations/F232.md`); `None` is a replacement
/// stopped.
async fn replace_inner(
    disk: &Disk,
    locks: &InodeLocks,
    source: &dyn ContentSource,
    r: &Replacement,
    leased: Option<&Leased<'_>>,
    stop: &CancellationToken,
) -> std::io::Result<Option<ReplaceOutcome>> {
    if stop.is_cancelled() {
        return Ok(None);
    }
    let work = Work { disk: disk.clone(), r: r.clone(), leased: leased.is_some() };
    let checked = section({
        let work = work.clone();
        move || work.check()
    });
    let (new, old_identity, old_key) = match checked.await? {
        Ok(checked) => checked,
        Err(done) => return Ok(Some(done)),
    };
    let Some(downloaded) = stop.run_until_cancelled(crate::hydration::source::download_into(&new, &r.id, source)).await else {
        return Ok(None);
    };
    let downloaded = match downloaded {
        Ok(downloaded) => downloaded,
        Err(errno) if errno == libc::ENOSPC || errno == libc::EDQUOT => {
            return Ok(Some(Failure::no_space(format!(
                "not enough space to download the new version of {}; the old version stays",
                r.rel.display()
            ))))
        }
        Err(errno) => {
            return Ok(Some(ReplaceOutcome::Failed(Failure {
                reason: FailureReason::Download(errno),
                text: format!(
                    "the new version of {} could not be downloaded ({}); the old version stays",
                    r.rel.display(),
                    std::io::Error::from_raw_os_error(errno)
                ),
            })))
        }
    };
    let sealed = section({
        let work = work.clone();
        move || work.seal(new, downloaded)
    });
    let new = sealed.await?;

    // The swap, under the old file's lock, after looking again: a Free up
    // space, a fill or a local edit may have happened while this downloaded.
    // Read-write mode takes the tree lock first, the worker's order.
    let tree = match leased {
        Some(leased) => match stop.run_until_cancelled(std::sync::Arc::clone(leased.tree_lock).lock_owned()).await {
            Some(tree) => Some(tree),
            None => return Ok(None),
        },
        None => None,
    };
    let Some(guard) = stop.run_until_cancelled(locks.lock(old_key)).await else { return Ok(None) };
    let store = leased.map(|leased| leased.store.clone());
    // From here to the end no stop is heard: the swap is made and said.
    section(move || {
        // Both locks until the section is over, whatever becomes of the task
        // that waits for it.
        let (_tree, _guard) = (tree, guard);
        work.swap(new, old_identity, store)
    })
    .await
    .map(Some)
}

/// Runs `work` on a blocking thread. A panic in it is the replacement's own,
/// as it was when the calls ran in place.
async fn section<T: Send + 'static>(work: impl FnOnce() -> std::io::Result<T> + Send + 'static) -> std::io::Result<T> {
    match tokio::task::spawn_blocking(work).await {
        Ok(done) => done,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(std::io::Error::other(format!("the replacement task failed: {e}"))),
    }
}

/// What the sections of one replacement share.
#[derive(Clone)]
struct Work {
    disk: Disk,
    r: Replacement,
    /// Read-write mode ([`Leased`]).
    leased: bool,
}

impl Work {
    /// Read-write mode: an emptied download holds local work too.
    fn local_work(&self, file: &File) -> bool {
        if self.leased {
            holds_local_work_rw(file)
        } else {
            holds_local_work(file)
        }
    }

    /// Downloaded, of another version, and holding nothing only this machine
    /// has: looked at through the file itself, so that it can be asked again
    /// under a lease, which the daemon's own open of it would break.
    fn replaceable(&self, old: &File) -> std::io::Result<bool> {
        let hydrated = matches!(read_state(old), Ok(Some(State::Hydrated)));
        let other_version = placeholder::read_ctag(old)?.as_deref() != Some(self.r.ctag.as_str());
        Ok(hydrated && other_version && !self.local_work(old))
    }

    fn still_there(&self, dir: &File, name: &OsStr) -> std::io::Result<Option<File>> {
        match self.disk.probe(dir, name)? {
            Probe::Managed { id, is_dir: false } if id == self.r.id => {}
            _ => return Ok(None),
        }
        let old = match self.disk.open_file(dir, name) {
            Ok(old) => old,
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR)) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(self.replaceable(&old)?.then_some(old))
    }

    fn parent(&self) -> &Path {
        self.r.rel.parent().unwrap_or(Path::new(""))
    }

    /// The first section, before anything is downloaded: whether there is a
    /// file to replace and room for its new version, and the nameless file
    /// the new version goes into, with the old file's identity.
    fn check(&self) -> std::io::Result<Result<(File, FileIdentity, InodeKey), ReplaceOutcome>> {
        let r = &self.r;
        let Some(name) = r.rel.file_name() else { return Ok(Err(ReplaceOutcome::Current)) };
        let Some(dir) = replacement_dir(&self.disk, self.parent())? else { return Ok(Err(ReplaceOutcome::Current)) };
        let Some(old) = self.still_there(&dir, name)? else { return Ok(Err(ReplaceOutcome::Current)) };
        // Read-write mode: open somewhere now, it would be again at the swap — no
        // download for nothing.
        if self.leased && konedrive_fs::lease::WriteLease::take(&old)?.is_none() {
            return Ok(Err(ReplaceOutcome::Busy));
        }
        // Kept only as identity from here, not as a hold on the file: a Free up
        // space must be able to take the old file's write lease while the new
        // version downloads, which it could not while `old` stayed open for the
        // whole download.
        let old_identity = FileIdentity::of(&old)?;
        let old_key = InodeKey::of(&old)?;
        drop(old);

        // Both versions must fit at once; checked before anything is
        // downloaded, so that a full disk costs no bandwidth on every retry.
        let fs = nix::sys::statvfs::fstatvfs(&dir)?;
        let free = fs.blocks_available() as u64 * fs.fragment_size() as u64;
        if free < r.size.saturating_add(REPLACE_SPACE_MARGIN) {
            return Ok(Err(Failure::no_space(format!(
                "not enough space to download the new version of {} beside the old one; the old version stays",
                r.rel.display()
            ))));
        }

        let new = self.disk.tmpfile(&dir)?;
        placeholder::write_item_id(&new, &r.id)?;
        Ok(Ok((new, old_identity, old_key)))
    }

    /// The second section, after the download: the new version made whole,
    /// labelled and on the disk, still nameless.
    fn seal(&self, new: File, downloaded: Downloaded) -> std::io::Result<File> {
        new.set_len(downloaded.size)?;
        if let Err(e) = placeholder::set_mtime(&new, downloaded.mtime) {
            tracing::warn!("{}: cannot apply the new version's mtime: {e}", self.r.rel.display());
        }
        new.sync_data()?;
        if let Some(version) = &downloaded.version {
            placeholder::write_ctag(&new, &version.ctag)?;
        }
        placeholder::write_stamp(&new)?;
        placeholder::write_state(&new, State::Hydrated)?;
        placeholder::set_mode(&new, if self.disk.locked() { LOCKED_FILE_MODE } else { OPEN_FILE_MODE })?;
        new.sync_all()?;
        Ok(new)
    }

    /// The third section, under the old file's lock and, in read-write mode
    /// (`store` given), the tree lock: the old file looked at again, the
    /// rename, and the new version's base.
    fn swap(&self, new: File, old_identity: FileIdentity, store: Option<Store>) -> std::io::Result<ReplaceOutcome> {
        let r = &self.r;
        let Some(name) = r.rel.file_name() else { return Ok(ReplaceOutcome::Current) };
        let Some(dir) = replacement_dir(&self.disk, self.parent())? else { return Ok(ReplaceOutcome::Current) };
        let now = self.still_there(&dir, name)?;
        let same_file = match &now {
            Some(now) => FileIdentity::of(now)? == old_identity,
            None => false,
        };
        if !same_file {
            return Ok(ReplaceOutcome::Current);
        }
        // Read-write mode: nobody has the old file open across the rename, or
        // what they write would land in the unlinked inode (§3.7). The lease
        // first; then, under it, the file is looked at again — through the
        // descriptor, and by name without opening it — so that a write that
        // landed before the lease is a stamp mismatch, never swapped away.
        let _lease = match (self.leased, &now) {
            (true, Some(now)) => match konedrive_fs::lease::WriteLease::take(now)? {
                Some(lease) => {
                    let meta = now.metadata()?;
                    let named = nix::sys::stat::fstatat(dir.as_fd(), name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW)
                        .is_ok_and(|at| (at.st_dev, at.st_ino) == (meta.dev(), meta.ino()));
                    if !named || FileIdentity::of(now)? != old_identity || !self.replaceable(now)? {
                        return Ok(ReplaceOutcome::Current);
                    }
                    Some(lease)
                }
                None => return Ok(ReplaceOutcome::Busy),
            },
            _ => None,
        };
        // A pin of the file's own goes with it to the new version, which is
        // another inode.
        if let Some(now) = &now {
            if placeholder::read_pin(now)? {
                placeholder::write_pin(&new)?;
            }
        }
        let temp = format!("{NEW_PREFIX}{}", r.id);
        clear_leftover_link(&self.disk, &dir, OsStr::new(&temp), &r.id)?;
        self.disk.swap_in(&dir, &new, OsStr::new(&temp), name)?;
        if let Some(store) = store {
            // The base takes the version the file now holds (the read-write reconcile must, items 1 and 4).
            let ctag = placeholder::read_ctag(&new).ok().flatten();
            let handle = konedrive_fs::handle::FileHandle::of(&new).ok();
            let id = r.id.clone();
            if let Err(e) = store.call_blocking(move |s| s.land_deferred(&id, ctag.as_deref(), handle.as_ref())) {
                tracing::warn!("{}: the new version is in place, and its base waits for the next cycle: {e}", r.rel.display());
            }
        }
        Ok(ReplaceOutcome::Replaced)
    }
}

/// Removes the temporary link an earlier swap of the same file left — a
/// crash between its link and its rename — which made every later swap fail
/// `EEXIST` until a Full reconcile cleared it. Only
/// a plain file carrying this very item id and no local work goes; anything
/// else is left, and the swap fails as before. A Full reconcile rescues one
/// that holds work (`discard_leftover_replacement`).
fn clear_leftover_link(disk: &Disk, dir: &File, temp: &OsStr, id: &str) -> std::io::Result<()> {
    if let Probe::Managed { id: found, is_dir: false } = disk.probe(dir, temp)? {
        if found == id && !holds_local_work(&disk.open_file(dir, temp)?) {
            tracing::info!("clearing the temporary link {} an earlier replacement left", temp.to_string_lossy());
            disk.remove(dir, temp, false)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
