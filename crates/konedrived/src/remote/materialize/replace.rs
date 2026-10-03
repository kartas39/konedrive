use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::SystemTime;

use konedrive_fs::placeholder::{self, read_state, State, LOCKED_FILE_MODE, OPEN_FILE_MODE};

use crate::folder::disk::{Disk, Probe, NEW_PREFIX};
use crate::hydration::source::ContentSource;
use crate::folder::locks::InodeLocks;
use konedrive_tree::Store;
use super::{holds_local_work, holds_local_work_rw, Replacement};

#[derive(Debug)]
pub enum ReplaceOutcome {
    Replaced,
    /// Nothing to do any more: the file moved, changed, was freed up or is
    /// already this version. The next cycle looks again.
    Current,
    Failed(String),
    /// Failed for want of disk space: the disk cannot hold both versions, or
    /// filled up while the new one downloaded. Said and retried as `Failed`
    /// is; told apart so that the activity log can say exactly "not enough
    /// disk space".
    NoSpace(String),
    /// Read-write mode: someone has the file open, so no write lease (write
    /// design §3.7). The old version stays, and so does its base; the next
    /// cycle tries again. Not a failure.
    Busy,
}

/// Read-write mode's replacement (`docs/design/writes.md` §9): the swap runs
/// under the per-root tree lock and a write lease on the old file, and the
/// new version's deferred change becomes the base as it lands.
pub struct Leased<'a> {
    pub tree_lock: &'a tokio::sync::Mutex<()>,
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
    match replace_inner(disk, locks, source, r, leased).await {
        Ok(outcome) => outcome,
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT)) => ReplaceOutcome::NoSpace(format!(
            "not enough space to finish the new version of {}; the old version stays",
            r.rel.display()
        )),
        Err(e) => ReplaceOutcome::Failed(e.to_string()),
    }
}

async fn replace_inner(disk: &Disk, locks: &InodeLocks, source: &dyn ContentSource, r: &Replacement, leased: Option<&Leased<'_>>) -> std::io::Result<ReplaceOutcome> {
    let parent = r.rel.parent().unwrap_or(Path::new(""));
    let Some(name) = r.rel.file_name() else { return Ok(ReplaceOutcome::Current) };
    let Some(dir) = replacement_dir(disk, parent)? else { return Ok(ReplaceOutcome::Current) };
    // Read-write mode: an emptied download holds local work too.
    let local_work = |file: &File| if leased.is_some() { holds_local_work_rw(file) } else { holds_local_work(file) };
    // Downloaded, of another version, and holding nothing only this machine
    // has: looked at through the file itself, so that it can be asked again
    // under a lease, which the daemon's own open of it would break.
    let replaceable = |old: &File| -> std::io::Result<bool> {
        let hydrated = matches!(read_state(old), Ok(Some(State::Hydrated)));
        let other_version = placeholder::read_ctag(old)?.as_deref() != Some(r.ctag.as_str());
        Ok(hydrated && other_version && !local_work(old))
    };
    let still_there = |dir: &File| -> std::io::Result<Option<File>> {
        match disk.probe(dir, name)? {
            Probe::Managed { id, is_dir: false } if id == r.id => {}
            _ => return Ok(None),
        }
        let old = match disk.open_file(dir, name) {
            Ok(old) => old,
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR)) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(replaceable(&old)?.then_some(old))
    };
    let Some(old) = still_there(&dir)? else { return Ok(ReplaceOutcome::Current) };
    // Read-write mode: open somewhere now, it would be again at the swap — no
    // download for nothing.
    if leased.is_some() && konedrive_fs::lease::WriteLease::take(&old)?.is_none() {
        return Ok(ReplaceOutcome::Busy);
    }
    // Kept only as identity from here, not as a hold on the file: a Free up
    // space must be able to take the old file's write lease while the new
    // version downloads, which it could not while `old` stayed open for the
    // whole download.
    let old_identity = FileIdentity::of(&old)?;
    let old_key = crate::folder::locks::InodeKey::of(&old)?;
    drop(old);

    // Both versions must fit at once; checked before anything is
    // downloaded, so that a full disk costs no bandwidth on every retry.
    let fs = nix::sys::statvfs::fstatvfs(&dir)?;
    let free = fs.blocks_available() as u64 * fs.fragment_size() as u64;
    if free < r.size.saturating_add(REPLACE_SPACE_MARGIN) {
        return Ok(ReplaceOutcome::NoSpace(format!(
            "not enough space to download the new version of {} beside the old one; the old version stays",
            r.rel.display()
        )));
    }

    let new = disk.tmpfile(&dir)?;
    placeholder::write_item_id(&new, &r.id)?;
    let downloaded = match crate::hydration::source::download_into(&new, &r.id, source).await {
        Ok(downloaded) => downloaded,
        Err(errno) if errno == libc::ENOSPC || errno == libc::EDQUOT => {
            return Ok(ReplaceOutcome::NoSpace(format!(
                "not enough space to download the new version of {}; the old version stays",
                r.rel.display()
            )))
        }
        Err(errno) => {
            return Ok(ReplaceOutcome::Failed(format!(
                "the new version of {} could not be downloaded ({}); the old version stays",
                r.rel.display(),
                std::io::Error::from_raw_os_error(errno)
            )))
        }
    };
    new.set_len(downloaded.size)?;
    if let Err(e) = placeholder::set_mtime(&new, downloaded.mtime) {
        tracing::warn!("{}: cannot apply the new version's mtime: {e}", r.rel.display());
    }
    new.sync_data()?;
    if let Some(version) = &downloaded.version {
        placeholder::write_ctag(&new, &version.ctag)?;
    }
    placeholder::write_stamp(&new)?;
    placeholder::write_state(&new, State::Hydrated)?;
    placeholder::set_mode(&new, if disk.locked() { LOCKED_FILE_MODE } else { OPEN_FILE_MODE })?;
    new.sync_all()?;

    // The swap, under the old file's lock, after looking again: a Free up
    // space, a fill or a local edit may have happened while this downloaded.
    // Read-write mode takes the tree lock first, the worker's order.
    let _tree = match leased {
        Some(leased) => Some(leased.tree_lock.lock().await),
        None => None,
    };
    let _guard = locks.lock(old_key).await;
    let Some(dir) = replacement_dir(disk, parent)? else { return Ok(ReplaceOutcome::Current) };
    let now = still_there(&dir)?;
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
    let _lease = match (leased, &now) {
        (Some(_), Some(now)) => match konedrive_fs::lease::WriteLease::take(now)? {
            Some(lease) => {
                let meta = now.metadata()?;
                let named = nix::sys::stat::fstatat(dir.as_fd(), name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW)
                    .is_ok_and(|at| (at.st_dev, at.st_ino) == (meta.dev(), meta.ino()));
                if !named || FileIdentity::of(now)? != old_identity || !replaceable(now)? {
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
    clear_leftover_link(disk, &dir, OsStr::new(&temp), &r.id)?;
    disk.swap_in(&dir, &new, OsStr::new(&temp), name)?;
    if let Some(leased) = leased {
        // The base takes the version the file now holds (the read-write reconcile must, items 1 and 4).
        let ctag = placeholder::read_ctag(&new).ok().flatten();
        let handle = konedrive_fs::handle::FileHandle::of(&new).ok();
        let id = r.id.clone();
        if let Err(e) = leased.store.call(move |s| s.land_deferred(&id, ctag.as_deref(), handle.as_ref())).await {
            tracing::warn!("{}: the new version is in place, and its base waits for the next cycle: {e}", r.rel.display());
        }
    }
    Ok(ReplaceOutcome::Replaced)
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
