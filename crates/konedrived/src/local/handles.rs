//! Which filesystem the recorded file handles belong to (`docs/design/writes.md` §8.1).
//!
//! Every failure to decode a handle is `ESTALE`, so "gone" is believed only for a handle taken
//! on the filesystem the folder is on now. The store keeps the name of that filesystem
//! ([`namespace`]). The examination makes the record right before it decides anything
//! ([`prepare`]: this is the one place that writes it); the move out only asks
//! ([`current_async`]).

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::XATTR_ITEM_ID;
use konedrive_tree::outbox::Inode;
use konedrive_tree::{Store, TreeError};
use nix::fcntl::OFlag;

use super::entry::proc_path;
use super::liveness::open_no_symlinks;

/// The filesystem the folder's handles belong to: the root directory's own
/// handle, and the filesystem's UUID where the kernel gives one
/// (`FS_IOC_GETFSUUID`). Both survive a reboot, a remount and a renumbered
/// device (`f_fsid`, the device number on XFS and F2FS, does not); both change
/// when the folder is on another filesystem or subvolume — the home moved to a
/// new disk, restored from a backup together with the store, a Btrfs snapshot
/// rolled back. A handle taken there decodes to `ESTALE` here, which then says
/// nothing about the object: the kernel answers every failure to decode a
/// handle so.
pub fn namespace(root: &File) -> io::Result<String> {
    let handle: String = FileHandle::of(root)?.encode().iter().map(|b| format!("{b:02x}")).collect();
    Ok(match fs_uuid(root) {
        Some(uuid) => format!("root:{handle};uuid:{uuid}"),
        None => format!("root:{handle}"),
    })
}

/// `FS_IOC_GETFSUUID`: `struct fsuuid2 { __u8 len; __u8 uuid[16]; }`, as hex.
fn fs_uuid(file: &File) -> Option<String> {
    const FS_IOC_GETFSUUID: libc::c_ulong = 0x8011_1500; // _IOR(0x15, 0, struct fsuuid2)
    let mut buf = [0u8; 17];
    // SAFETY: the ioctl writes at most `sizeof(struct fsuuid2)` (17) bytes into `buf`.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_GETFSUUID, buf.as_mut_ptr()) };
    let len = usize::from(buf[0]).min(16);
    (rc == 0 && len > 0).then(|| buf[1..=len].iter().map(|b| format!("{b:02x}")).collect())
}

/// What [`prepare`] left behind for an examination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prepared {
    /// The recorded handles are of the filesystem the folder is on now: an `ESTALE` for one
    /// of them may mean "deleted". False when that cannot be told, and nothing is then gone.
    pub current: bool,
    /// The filesystem had changed, and every handle was forgotten ([`renew`]): the
    /// examination must read the whole folder to take them again.
    pub renewed: bool,
}

/// Makes the store's record of the handles' filesystem right, before an examination decides
/// anything by a handle. Writes the store:
///
/// - nothing recorded yet: the filesystem `root` is on is recorded;
/// - recorded for another filesystem (a new disk, a snapshot rolled back): the handles are
///   taken off every item and the new filesystem recorded ([`renew`]);
/// - recorded for this one: nothing is written.
///
/// A root whose handle cannot be read, or a record that cannot be read or written, leaves the
/// handles not current. Only a failed [`renew`] is an error: half of it may be written, and
/// the examination must not go on.
pub fn prepare(store: &Store, root: &File) -> Result<Prepared, TreeError> {
    let unknown = Prepared { current: false, renewed: false };
    let Ok(now) = namespace(root) else { return Ok(unknown) };
    match store.call_blocking(|s| s.handles_filesystem()) {
        Ok(Some(recorded)) if recorded == now => Ok(Prepared { current: true, renewed: false }),
        Ok(Some(_)) => {
            let dropped = renew(store, &now)?;
            tracing::warn!(
                "the folder's filesystem is not the one its file handles were taken on: they are taken again, and \
                 {dropped} move(s) out of the folder whose object is not where it was are left to OneDrive"
            );
            Ok(Prepared { current: true, renewed: true })
        }
        Ok(None) => Ok(Prepared { current: store.call_blocking(move |s| s.set_handles_filesystem(&now)).is_ok(), renewed: false }),
        Err(_) => Ok(unknown),
    }
}

/// Whether the handles `store` records were taken on the filesystem `root` is on now, for
/// async code. Only a question: with nothing recorded, or nothing that can be read, the
/// answer is no, and stays no until an examination has run ([`prepare`]).
pub async fn current_async(store: &Store, root: &File) -> bool {
    let Ok(now) = namespace(root) else { return false };
    matches!(store.call(|s| s.handles_filesystem()).await, Ok(Some(recorded)) if recorded == now)
}

/// The folder's filesystem changed: its recorded handles say nothing any more. In the store,
/// in one go: every item forgets its local object — a Full local scan takes each again where
/// it is, and one missing then is placed again from OneDrive rather than deleted (WR4) — and
/// each `move-out` row takes the handle of what stands at the place it last proved if that
/// carries the row's item id ([`standing_at`]), or goes (the item stays in OneDrive and is
/// placed again). Then `now` is recorded. How many rows went.
fn renew(store: &Store, now: &str) -> Result<usize, TreeError> {
    let rows = store.call_blocking(move |s| s.outbox_move_outs())?;
    let mut dropped = 0;
    for row in rows {
        let Some(id) = row.item_id.clone() else { continue };
        match row.last_place().and_then(|path| standing_at(path, &id)) {
            Some(inode) => {
                store.call_blocking(move |s| s.outbox_amend(row.seq, |r| r.inode = Some(inode)))?;
            }
            None => {
                store.call_blocking(move |s| s.outbox_drop(row.seq, None, Some(&id), None))?;
                dropped += 1;
            }
        }
    }
    let now = now.to_owned();
    store.call_blocking(move |s| {
        s.forget_local_handles()?;
        s.set_handles_filesystem(&now)
    })?;
    Ok(dropped)
}

/// The object at `path`, if it carries item `id`. The path is walked through the user's own
/// lookups with no symbolic link in it, the last part included, and the id, the inode and the
/// handle are all read from the one object that was opened: a link put at the place since
/// names nothing.
fn standing_at(path: &Path, id: &str) -> Option<Inode> {
    let object = open_no_symlinks(path, OFlag::O_PATH | OFlag::O_NOFOLLOW).ok()?;
    let meta = object.metadata().ok()?;
    if meta.file_type().is_symlink() {
        return None;
    }
    // By the descriptor's name in `/proc`, followed: an `O_PATH` descriptor reads no attribute
    // itself, and the name is the object, not a path walked again.
    let carries = xattr::get_deref(proc_path(&object), XATTR_ITEM_ID).ok().flatten()?;
    if carries != id.as_bytes() {
        return None;
    }
    let handle = FileHandle::of(&object).ok()?;
    Some(Inode { dev: meta.dev(), ino: meta.ino(), handle: Some(handle) })
}
