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
use konedrive_tree::{ActivityKind, ActivityRow, Store, TreeError};
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
/// A root whose handle cannot be read, a record that cannot be read or written, or a place
/// outside the folder that cannot be looked at ([`Standing::Unreachable`]) leaves the handles
/// not current, and nothing is written: the next examination tries again. Only a [`renew`]
/// that failed in the store is an error: half of it may be written, and the examination must
/// not go on. `now` is the time of what is said in Activity.
pub fn prepare(store: &Store, root: &File, now: i64) -> Result<Prepared, TreeError> {
    let unknown = Prepared { current: false, renewed: false };
    let Ok(on) = namespace(root) else { return Ok(unknown) };
    match store.call_blocking(|s| s.handles_filesystem()) {
        Ok(Some(recorded)) if recorded == on => Ok(Prepared { current: true, renewed: false }),
        Ok(Some(_)) => match renew(store, root, &on, now)? {
            Some(dropped) => {
                tracing::warn!(
                    "the folder's filesystem is not the one its file handles were taken on: they are taken again, and \
                     {dropped} move(s) out of the folder whose object is not where it was are left to OneDrive"
                );
                Ok(Prepared { current: true, renewed: true })
            }
            None => Ok(unknown),
        },
        Ok(None) => Ok(Prepared { current: store.call_blocking(move |s| s.set_handles_filesystem(&on)).is_ok(), renewed: false }),
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

/// The folder's filesystem changed: its recorded handles say nothing any more. Every
/// `move-out` row is looked at first, with nothing written ([`standing_at`]); if one of the
/// places cannot be looked at, nothing is renewed (`None`) and everything waits. Then, in the
/// store: each `move-out` row whose object is found takes its handle and the place it stands
/// at now, and the user's move out goes on; a row whose object is not there, or is not the
/// item, goes — the item stays in OneDrive and is placed again, which is logged and said in
/// Activity. Every item forgets its local object — a Full local scan takes each again where
/// it is, and one missing then is placed again from OneDrive rather than deleted (WR4) — and
/// `on` is recorded. How many rows went.
fn renew(store: &Store, root: &File, on: &str, now: i64) -> Result<Option<usize>, TreeError> {
    let rows = store.call_blocking(move |s| s.outbox_move_outs())?;
    let mut found = Vec::new();
    for row in rows {
        let Some(id) = row.item_id.clone() else { continue };
        let standing = row.last_place().map_or(Standing::NotThere, |place| standing_at(place, &id));
        if let Standing::Unreachable(err) = &standing {
            tracing::warn!(
                "the folder's filesystem changed, and where {} went cannot be looked at ({err}): the handles are not taken again yet, and nothing is deleted meanwhile",
                row.rel.display()
            );
            return Ok(None);
        }
        found.push((row, id, standing));
    }
    let folder = std::fs::read_link(proc_path(root)).ok();
    let mut dropped = 0;
    for (row, id, standing) in found {
        match standing {
            Standing::Item { inode, place } => {
                store.call_blocking(move |s| {
                    s.outbox_amend(row.seq, |r| {
                        r.inode = Some(inode);
                        if place.is_some() {
                            r.target_name = place;
                        }
                    })
                })?;
            }
            Standing::NotThere | Standing::Unreachable(_) => {
                tracing::warn!(
                    "{} left the folder, and after the folder's filesystem changed it is not where it went: it stays in OneDrive and is placed in the folder again",
                    row.rel.display()
                );
                let event = ActivityRow {
                    at: now,
                    kind: ActivityKind::Restored,
                    path: folder.as_deref().map_or_else(|| row.rel.clone(), |folder| folder.join(&row.rel)).display().to_string(),
                    detail: "moved out of the folder, and not found where it went after the folder's disk changed: it stays in OneDrive".to_owned(),
                };
                store.call_blocking(move |s| s.outbox_drop(row.seq, None, Some(&id), Some(&event)))?;
                dropped += 1;
            }
        }
    }
    let on = on.to_owned();
    store.call_blocking(move |s| {
        s.forget_local_handles()?;
        s.set_handles_filesystem(&on)
    })?;
    Ok(Some(dropped))
}

/// What stands where a `move-out` row's object was last proved.
enum Standing {
    /// The object carrying the row's item id, and the path it stands at with every link in
    /// the recorded one resolved (if that can be written as the row keeps it).
    Item { inode: Inode, place: Option<String> },
    /// Nothing, or something that is not the item.
    NotThere,
    /// The place cannot be looked at for a reason that says nothing of what is there
    /// (`ENOSYS`: no `openat2` here): not a proof of anything.
    Unreachable(io::Error),
}

/// What stands at `path`, a place recorded on the filesystem the folder was on before: is it
/// item `id`? The recorded path may lead through a symbolic link by now (a home copied to a
/// new disk, and `/home` made a link to it), so it is resolved first; the resolved path is
/// then opened through the user's own lookups with no link in it, and the id, the inode and
/// the handle are all read from that one descriptor. What proves the place is the id the
/// object carries, not the path that led to it.
fn standing_at(path: &Path, id: &str) -> Standing {
    let Ok(resolved) = std::fs::canonicalize(path) else { return Standing::NotThere };
    let object = match open_no_symlinks(&resolved, OFlag::O_PATH | OFlag::O_NOFOLLOW) {
        Ok(object) => object,
        Err(err) if err.raw_os_error() == Some(libc::ENOSYS) => return Standing::Unreachable(err),
        Err(_) => return Standing::NotThere,
    };
    let Ok(meta) = object.metadata() else { return Standing::NotThere };
    if meta.file_type().is_symlink() {
        return Standing::NotThere;
    }
    // By the descriptor's name in `/proc`, followed: an `O_PATH` descriptor reads no attribute
    // itself, and the name is the object, not a path walked again.
    let carries = xattr::get_deref(proc_path(&object), XATTR_ITEM_ID).ok().flatten();
    let Ok(handle) = FileHandle::of(&object) else { return Standing::NotThere };
    if carries.as_deref() != Some(id.as_bytes()) {
        return Standing::NotThere;
    }
    Standing::Item { inode: Inode { dev: meta.dev(), ino: meta.ino(), handle: Some(handle) }, place: resolved.to_str().map(str::to_owned) }
}
