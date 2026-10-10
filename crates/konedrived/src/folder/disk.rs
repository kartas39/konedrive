//! The upload worker's hands on the folder. Every change it makes to
//! the folder goes through a directory descriptor opened beneath the root —
//! never a path string a rename could redirect — and every change to a locked
//! directory through a window of its own.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use konedrive_fs::proc_path;
use konedrive_fs::placeholder::{self, LOCKED_DIR_MODE, OPEN_DIR_MODE, XATTR_ITEM_ID};
use nix::errno::Errno;
use nix::fcntl::{openat2, AtFlags, OFlag, OpenHow, RenameFlags, ResolveFlag};
use nix::sys::stat::Mode;
use nix::unistd::UnlinkatFlags;

use super::root::SyncRoot;
use konedrive_tree::usable_id;

/// Where misplaced items wait during a reconcile.
pub const HOLDING: &str = ".konedrive-holding";
/// A new folder's name until it is labelled and marked.
pub const NEW_PREFIX: &str = ".konedrive-new-";
/// How the name begins that the check of the backing directory at start sets an item's row
/// aside to, in the root of the folder, while the place the item is to have is not free yet.
/// No entry of the backing directory has such a name.
pub const SET_ASIDE_PREFIX: &str = ".konedrive-check-";

/// Whether `name` is one the daemon uses for itself in the folder: the holding
/// directory, a new folder before it has its name, or a name a row is set aside to by the
/// check of the backing directory. Nothing under such a name is the user's: the mount shows
/// none of it.
pub fn daemon_owned(name: &OsStr) -> bool {
    let begins_with = |prefix: &str| name.as_bytes().starts_with(prefix.as_bytes());
    name == OsStr::new(HOLDING) || begins_with(NEW_PREFIX) || begins_with(SET_ASIDE_PREFIX)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Absent,
    /// Carries an item id: one of ours.
    Managed { id: String, is_dir: bool },
    /// Anything without an item id — or anything that is neither a file nor a
    /// directory, which is never ours.
    Unmanaged { is_dir: bool },
}

/// Cloned to go into a blocking section: the clones share the root's one descriptor.
#[derive(Clone)]
pub struct Disk {
    root: Arc<File>,
    locked: bool,
    modes: Arc<Modes>,
}

/// One folder's lock on the modes of its directories, held by everything in this daemon
/// that lifts a locked directory's write bit and puts it back: the windows of a change
/// ([`Disk::writable`]) and the drive written on the root. Two such windows on one
/// directory used to be able to interleave — one put the lock back while the other was
/// still writing, which failed `EACCES`.
///
/// A folder has one, whoever asks ([`Modes::of`]): every [`Disk`] opened on the folder
/// shares it, and another account's folder has its own. It is not re-entrant: what nests
/// windows takes it once and passes the hold on ([`Disk::window`]). Never held across an
/// `.await`.
pub struct Modes(Mutex<()>);

/// A hold on a folder's [`Modes`], released when dropped.
pub struct ModesHeld<'a>(#[allow(dead_code)] MutexGuard<'a, ()>);

/// The folders whose [`Modes`] somebody holds a share of, by the root directory's device
/// and inode. An entry goes when nobody does.
static FOLDERS: Mutex<Vec<(RootKey, Weak<Modes>)>> = Mutex::new(Vec::new());

/// A root directory: its device and inode.
type RootKey = (u64, u64);

impl Modes {
    /// The lock of the folder whose root directory `root` is open on.
    pub fn of(root: &File) -> io::Result<Arc<Modes>> {
        let meta = root.metadata()?;
        let key = (meta.dev(), meta.ino());
        let mut folders = crate::panic::lock(&FOLDERS);
        folders.retain(|(_, modes)| modes.strong_count() > 0);
        if let Some(modes) = folders.iter().find(|(k, _)| *k == key).and_then(|(_, modes)| modes.upgrade()) {
            return Ok(modes);
        }
        let modes = Arc::new(Modes(Mutex::new(())));
        folders.push((key, Arc::downgrade(&modes)));
        Ok(modes)
    }

    pub fn hold(&self) -> ModesHeld<'_> {
        // The lock guards no data a panic could leave half-written.
        ModesHeld(crate::panic::lock(&self.0))
    }
}

/// How every open below a directory of the folder resolves its path: never out of that
/// directory, never through a symbolic link, never through a `/proc` magic link.
pub(crate) fn beneath() -> ResolveFlag {
    ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_SYMLINKS | ResolveFlag::RESOLVE_NO_MAGICLINKS
}

/// The user attribute `name` of what is at `path`, read by name (`lgetxattr`).
/// A filesystem that holds no user attributes (vfat, some FUSE mounts) answers
/// `EOPNOTSUPP`: nothing on it carries one of ours, so that reads as none
/// (`LO13`). For the examination's entries and [`Disk::probe`]: the
/// reconcile's scan walks into such a mount, and probes the place of a folder
/// the mount stands over (measured in the VM: with the scan alone lenient,
/// every cycle still fails there). What is on another device is never an
/// item's object: the examination skips it before any id is used, and the
/// worker takes a directory's id only when the base records that object
/// (`upload::steps::shared::dir_id`).
pub fn attr_by_name(path: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
    match xattr::get(path, name) {
        Err(e) if e.raw_os_error() == Some(libc::EOPNOTSUPP) => Ok(None),
        read => read,
    }
}

pub fn open_subdir(dir: &File, name: &OsStr) -> io::Result<File> {
    let fd = nix::fcntl::openat(dir.as_fd(), name, OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC, Mode::empty())?;
    Ok(File::from(fd))
}

impl Disk {
    /// The registered root, proved to still be this root.
    pub fn open(root: &SyncRoot, locked: bool) -> io::Result<Self> {
        let dir = root
            .open_registered()?
            .ok_or_else(|| io::Error::other(format!("{} no longer carries its root id", root.path.display())))?;
        let modes = Modes::of(&dir)?;
        Ok(Self { root: Arc::new(dir), locked, modes })
    }

    /// The directory at `rel` beneath the root (`""` is the root itself).
    pub fn dir(&self, rel: &Path) -> io::Result<File> {
        if rel.as_os_str().is_empty() {
            return self.root.try_clone();
        }
        let how = OpenHow::new().flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC).resolve(beneath());
        Ok(File::from(openat2(self.root.as_fd(), rel, how)?))
    }

    /// What `name` in `dir` is, read by name: `fstatat` without following, and
    /// the item id with `lgetxattr` — no open, so no interception either.
    pub fn probe(&self, dir: &File, name: &OsStr) -> io::Result<Probe> {
        let stat = match nix::sys::stat::fstatat(dir.as_fd(), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::ENOENT) => return Ok(Probe::Absent),
            Err(e) => return Err(e.into()),
        };
        let kind = stat.st_mode & libc::S_IFMT;
        if kind != libc::S_IFDIR && kind != libc::S_IFREG {
            return Ok(Probe::Unmanaged { is_dir: false });
        }
        let is_dir = kind == libc::S_IFDIR;
        match attr_by_name(&proc_path(dir).join(name), XATTR_ITEM_ID)? {
            Some(id) => {
                let id = String::from_utf8_lossy(&id).into_owned();
                // An id is a name in the holding directory; one that cannot
                // be (`..`, `a/b`, ...) is no id of ours.
                if usable_id(&id) {
                    Ok(Probe::Managed { id, is_dir })
                } else {
                    Ok(Probe::Unmanaged { is_dir })
                }
            }
            None => Ok(Probe::Unmanaged { is_dir }),
        }
    }

    /// Runs `op` with write permission on `dir` when the folder is locked, and
    /// locks it again afterwards.
    pub fn writable<T>(&self, dir: &File, op: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        if !self.locked {
            return op();
        }
        self.window(&self.modes.hold(), dir, op)
    }

    /// [`writable`](Self::writable) under a hold the caller has: a window inside a window,
    /// or one of a walk that holds the folder's modes throughout.
    fn window<T>(&self, _held: &ModesHeld<'_>, dir: &File, op: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        if !self.locked {
            return op();
        }
        placeholder::set_mode(dir, OPEN_DIR_MODE)?;
        let result = op();
        let relocked = placeholder::set_mode(dir, LOCKED_DIR_MODE);
        match (result, relocked) {
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(e)) => Err(e),
            (Err(e), _) => Err(e),
        }
    }

    /// Moves `from_dir/from` to `to_dir/to`, never over anything
    /// (`RENAME_NOREPLACE`). A directory changing parents needs write
    /// permission on itself as well — its `..` changes.
    pub fn rename(&self, from_dir: &File, from: &OsStr, to_dir: &File, to: &OsStr) -> io::Result<()> {
        let rename = || {
            nix::fcntl::renameat2(from_dir.as_fd(), from, to_dir.as_fd(), to, RenameFlags::RENAME_NOREPLACE).map_err(io::Error::from)
        };
        let held = self.modes.hold();
        let in_windows = || self.window(&held, from_dir, || self.window(&held, to_dir, rename));
        let moved_dir = matches!(self.probe(from_dir, from)?, Probe::Managed { is_dir: true, .. } | Probe::Unmanaged { is_dir: true });
        if self.locked && moved_dir {
            let moved = open_subdir(from_dir, from)?;
            self.window(&held, &moved, in_windows)
        } else {
            in_windows()
        }
    }

    pub fn remove(&self, dir: &File, name: &OsStr, is_dir: bool) -> io::Result<()> {
        let flag = if is_dir { UnlinkatFlags::RemoveDir } else { UnlinkatFlags::NoRemoveDir };
        self.writable(dir, || nix::unistd::unlinkat(dir.as_fd(), name, flag).map_err(io::Error::from))
    }
}

#[cfg(test)]
mod tests;

// ---- The names of the mount over the rows: no caller until the mount reads the rows. ----
//
// Two rules, with two questions, because the two kinds of name need OPPOSITE answers from
// the check of the backing directory at start. Tested without the rest:
// test: an_entry_set_aside_is_an_entry_no_row_says_and_a_hidden_name
// test: a_temporary_name_is_a_hidden_name_and_not_an_entry_no_row_says
// test: the_holding_directory_and_a_new_folder_are_both
// test: the_set_aside_name_of_an_item_is_the_same_at_every_start_and_another_items_is_another

/// How the name begins that an entry of the backing directory is set aside to, in the root,
/// before it is removed for good (`fuse::place_changes`, `PlaceChange::set_aside`): an
/// entry NO row says. After it comes the local item id of the item the entry was of.
#[allow(dead_code)]
pub const ASIDE_ENTRY_PREFIX: &str = ".konedrive-aside-";

/// How the temporary name of an exchange of names begins
/// (`konedrive_tree::make_temporary_name`): the place of a ROW, with its file or directory
/// on disk under that name.
#[allow(dead_code)]
pub const TEMPORARY_NAME_PREFIX: &str = ".konedrive-swap-";

/// The name the entry of `item` is set aside to: made of the local item id, which is never
/// given twice, so that it is the same name at every start and no other item's. One item
/// has one entry set aside at most (one step per item, under the tree lock); a leftover
/// of a death is put back by the check at start from the record of the change, and what
/// is still set aside after it removed (`PlaceChanges::remove_aside_entries`).
#[allow(dead_code, unused_variables)]
pub fn make_aside_entry_name(item: konedrive_tree::LocalItemId) -> std::ffi::OsString {
    format!("{ASIDE_ENTRY_PREFIX}{item}").into()
}

/// Whether `name`, in the root of the backing directory, is an entry no row says: a name
/// of the daemon's own ([`daemon_owned`]), or an entry set aside. What the check of the
/// backing directory at start does not read as something of the folder, and what
/// `PlaceChanges::remove_aside_entries` cleans of the last kind once the check has put
/// back what a death left. A temporary name is NOT among them: it is a row's place, and
/// skipping it would make the check find the row's file gone.
#[allow(dead_code, unused_variables)]
pub fn is_rowless_entry_name(name: &OsStr) -> bool {
    daemon_owned(name) || name.as_bytes().starts_with(ASIDE_ENTRY_PREFIX.as_bytes())
}

/// Whether the mount hides `name` and refuses it to programs (lookup `ENOENT`, make and
/// rename `EPERM`): every entry no row says ([`is_rowless_entry_name`]), and a temporary
/// name of an exchange. Asked by the mount's lookup, directory readings, make and rename,
/// and by the queue's answers, which show no such row.
#[allow(dead_code, unused_variables)]
pub fn is_hidden_name(name: &OsStr) -> bool {
    is_rowless_entry_name(name) || name.as_bytes().starts_with(TEMPORARY_NAME_PREFIX.as_bytes())
}
