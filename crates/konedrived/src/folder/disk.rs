//! The materializer's hands. Every change it makes to
//! the folder goes through a directory descriptor opened beneath the root —
//! never a path string a rename could redirect — and every change to a locked
//! directory through a window of its own.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::SystemTime;

use konedrive_fs::proc_path;
use konedrive_fs::placeholder::{self, LOCKED_DIR_MODE, LOCKED_FILE_MODE, OPEN_DIR_MODE, OPEN_FILE_MODE, XATTR_ITEM_ID};
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

/// Whether `name` is one the daemon uses for itself in the folder: the holding
/// directory, or a new folder before it has its name. Nothing under such a name is
/// the user's: the examination and the watcher both pass it over.
pub fn daemon_owned(name: &OsStr) -> bool {
    name == OsStr::new(HOLDING) || name.as_bytes().starts_with(NEW_PREFIX.as_bytes())
}

/// Whether `e` says that what was asked for by name is not there any more: no such
/// name, a part of the path that is no directory by now, or a symbolic link where an
/// open that follows none met one (the name holds something else).
pub fn gone(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP))
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

#[derive(Debug, Clone)]
pub struct Scanned {
    /// Relative to the root.
    pub rel: PathBuf,
    pub id: Option<String>,
    pub is_dir: bool,
    /// 1 for the root's own entries.
    pub depth: usize,
    /// The item id of the directory it is in — the drive's root id for the
    /// root's entries; `None` inside a directory without one.
    pub parent_id: Option<String>,
}

/// Cloned to go into a blocking section (`remote::materialize::replace`): the
/// clones share the root's one descriptor.
#[derive(Clone)]
pub struct Disk {
    root: Arc<File>,
    locked: bool,
    modes: Arc<Modes>,
}

/// One folder's lock on the modes of its directories, held by everything in this daemon
/// that lifts a locked directory's write bit and puts it back: the materializer's windows
/// ([`Disk::writable`]), its locking and unlocking walks, a pin written on a folder
/// (`hydration::pin::set_pin`) and the drive written on the root. Two such windows on one
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

    /// [`Modes::of`] for a root named by its path: opened as [`Disk::open`] opens it, a
    /// directory and never through a link, so that both find the same lock.
    pub fn of_root(root: &SyncRoot) -> io::Result<Arc<Modes>> {
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        Self::of(&File::from(nix::fcntl::open(&root.path, flags, Mode::empty())?))
    }

    pub fn hold(&self) -> ModesHeld<'_> {
        // The lock guards no data a panic could leave half-written.
        ModesHeld(crate::panic::lock(&self.0))
    }
}

/// How many entries a walk could not change, as its error.
fn passed_over(failed: usize, what: &str) -> io::Result<()> {
    match failed {
        0 => Ok(()),
        n => Err(io::Error::other(format!("{n} entries {what}"))),
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

    pub fn locked(&self) -> bool {
        self.locked
    }

    /// The directory at `rel` beneath the root (`""` is the root itself).
    pub fn dir(&self, rel: &Path) -> io::Result<File> {
        if rel.as_os_str().is_empty() {
            return self.root.try_clone();
        }
        let how = OpenHow::new().flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC).resolve(beneath());
        Ok(File::from(openat2(self.root.as_fd(), rel, how)?))
    }

    /// A file in `dir`, read-only. `O_NONBLOCK` so a name that turned into a
    /// FIFO cannot block; the daemon's own open is never intercepted.
    pub fn open_file(&self, dir: &File, name: &OsStr) -> io::Result<File> {
        let fd = nix::fcntl::openat(dir.as_fd(), name, OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC, Mode::empty())?;
        let file = File::from(fd);
        if !file.metadata()?.is_file() {
            return Err(io::Error::other(format!("{} is not a regular file", name.to_string_lossy())));
        }
        Ok(file)
    }

    pub fn open_subdir(&self, dir: &File, name: &OsStr) -> io::Result<File> {
        open_subdir(dir, name)
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

    /// An empty directory of the daemon's own (the holding directory), `0755`.
    pub fn make_dir(&self, parent: &File, name: &OsStr) -> io::Result<File> {
        self.writable(parent, || {
            nix::sys::stat::mkdirat(parent.as_fd(), name, Mode::from_bits_truncate(OPEN_DIR_MODE))?;
            open_subdir(parent, name)
        })
    }

    /// A nameless file in `dir` (`O_TMPFILE`), for a new version to be
    /// downloaded into. Nobody can open it until it is linked in.
    pub fn tmpfile(&self, dir: &File) -> io::Result<File> {
        self.writable(dir, || {
            let fd = nix::fcntl::openat(dir.as_fd(), ".", OFlag::O_TMPFILE | OFlag::O_RDWR | OFlag::O_CLOEXEC, Mode::from_bits_truncate(OPEN_FILE_MODE))?;
            Ok(File::from(fd))
        })
    }

    /// Links `file` in as `temp` and renames it over `name`: one rename, so a
    /// reader of the old file keeps it to the end and the next open gets the
    /// new one.
    pub fn swap_in(&self, dir: &File, file: &File, temp: &OsStr, name: &OsStr) -> io::Result<()> {
        self.writable(dir, || {
            nix::unistd::linkat(file.as_fd(), "", dir.as_fd(), temp, AtFlags::AT_EMPTY_PATH)?;
            if let Err(e) = nix::fcntl::renameat(dir.as_fd(), temp, dir.as_fd(), name) {
                // The link landed; only the rename over the old name failed.
                // Left alone, this becomes exactly the leftover a later Full
                // reconcile has to recognise and clear on its own — cleared
                // here instead, best effort, while its cause is still known.
                if let Err(unlink_err) = nix::unistd::unlinkat(dir.as_fd(), temp, UnlinkatFlags::NoRemoveDir) {
                    tracing::warn!(
                        "{}: the swap's rename failed ({e}), and its temporary link {} could not be removed either ({unlink_err}); a later reconcile will clear it",
                        name.to_string_lossy(),
                        temp.to_string_lossy()
                    );
                }
                return Err(e.into());
            }
            Ok(())
        })
    }

    /// Locks a directory made this cycle, now that everything is in it.
    pub fn lock_dir(&self, dir: &File) -> io::Result<()> {
        if self.locked {
            let _held = self.modes.hold();
            placeholder::set_mode(dir, LOCKED_DIR_MODE)?;
        }
        Ok(())
    }

    /// Gives `name` the lock's mode when it has another — a window a crash
    /// left open, or a mode changed by hand — unless `claim` (given the
    /// opened file) says it is busy: a file a fill is writing has its write
    /// bit lifted for a moment around each attribute write, and locking it
    /// in that window failed the fill `EACCES`.
    /// What `claim` returns is held until the mode is set.
    pub fn enforce_mode<G>(&self, dir: &File, name: &OsStr, claim: impl FnOnce(&File) -> io::Result<Option<G>>) -> io::Result<()> {
        if !self.locked {
            return Ok(());
        }
        self.enforce_mode_held(&self.modes.hold(), dir, name, claim)
    }

    fn enforce_mode_held<G>(
        &self,
        _held: &ModesHeld<'_>,
        dir: &File,
        name: &OsStr,
        claim: impl FnOnce(&File) -> io::Result<Option<G>>,
    ) -> io::Result<()> {
        let stat = nix::sys::stat::fstatat(dir.as_fd(), name, AtFlags::AT_SYMLINK_NOFOLLOW)?;
        let (want, is_dir) = match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => (LOCKED_DIR_MODE, true),
            libc::S_IFREG => (LOCKED_FILE_MODE, false),
            _ => return Ok(()),
        };
        if stat.st_mode & 0o7777 == want {
            return Ok(());
        }
        let opened = if is_dir { open_subdir(dir, name)? } else { self.open_file(dir, name)? };
        let Some(_claimed) = claim(&opened)? else { return Ok(()) };
        placeholder::set_mode(&opened, want)
    }

    pub fn list(&self, dir: &File) -> io::Result<Vec<OsString>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(proc_path(dir))? {
            names.push(entry?.file_name());
        }
        names.sort();
        Ok(names)
    }

    /// Every file and directory beneath the root with its item id, by name.
    /// One directory descriptor open at a time: the walk keeps paths, not
    /// descriptors.
    ///
    /// The scan is the reconcile's picture of the folder, so it is whole or it is an
    /// error: a directory that cannot be opened or listed, or an entry that cannot be
    /// looked at, fails it. Two things are not errors: a directory or an entry that went
    /// away while the walk was on its way to it, and what lies deeper than
    /// `konedrive_fs::MAX_DEPTH`, which the helper does not mark either.
    pub fn scan(&self, root_item_id: &str) -> io::Result<Vec<Scanned>> {
        let went = |e: &io::Error| e.raw_os_error() == Some(libc::ENOENT);
        let at = |rel: &Path, e: io::Error| {
            let shown = if rel.as_os_str().is_empty() { Path::new("the folder itself") } else { rel };
            io::Error::new(e.kind(), format!("cannot scan {}: {e}", shown.display()))
        };
        let mut out = Vec::new();
        let mut pending = vec![(PathBuf::new(), 0usize, Some(root_item_id.to_owned()))];
        while let Some((rel, depth, dir_id)) = pending.pop() {
            if depth >= konedrive_fs::MAX_DEPTH {
                tracing::warn!("{} is deeper than {} levels; not scanned", rel.display(), konedrive_fs::MAX_DEPTH);
                continue;
            }
            let listed = self.dir(&rel).and_then(|dir| Ok((self.list(&dir)?, dir)));
            let (names, dir) = match listed {
                Ok(listed) => listed,
                Err(e) if went(&e) && depth > 0 => continue,
                Err(e) => return Err(at(&rel, e)),
            };
            for name in names {
                let (id, is_dir) = match self.probe(&dir, &name).map_err(|e| at(&rel.join(&name), e))? {
                    Probe::Absent => continue,
                    Probe::Managed { id, is_dir } => (Some(id), is_dir),
                    Probe::Unmanaged { is_dir } => (None, is_dir),
                };
                let child = rel.join(&name);
                if is_dir {
                    pending.push((child.clone(), depth + 1, id.clone()));
                }
                out.push(Scanned { rel: child, id, is_dir, depth: depth + 1, parent_id: dir_id.clone() });
            }
        }
        Ok(out)
    }

    /// Moves `name` out of the folder to `into/<shown>`, then
    /// makes it the user's own there: no konedrive attributes, ordinary modes.
    /// A directory goes with everything in it. Where it is now.
    ///
    /// A file of ours that holds no content of its own — `online-only`, or cut off
    /// mid-fill or mid-free-up — is not rescued, at any depth: there is nothing in it
    /// anyone made here, the cloud still has it, and stripped of its state it would read
    /// as a file of zeros among the rescued ones. Named itself it is removed where it is,
    /// and the answer is `None`; inside a rescued directory it is removed there
    /// ([`release`](Self::release)).
    ///
    /// Always one rename, never a copy: a copy followed by a delete could
    /// delete something other than what was copied. `into` must therefore be
    /// on the root's filesystem ([`rescue_base`]); across filesystems the
    /// rescue fails, naming both paths, and the item is left exactly as it
    /// was — nothing is changed on it before the rename succeeds.
    ///
    /// Never over anything: the rename itself is the check
    /// (`RENAME_NOREPLACE`), and a name already taken — by a file rescued
    /// earlier, or by one that appeared a moment ago — sends it on to
    /// `<shown>.1`, `<shown>.2`, ...
    pub fn rescue(&self, dir: &File, name: &OsStr, shown: &Path, into: &Path) -> io::Result<Option<PathBuf>> {
        if matches!(self.probe(dir, name)?, Probe::Managed { is_dir: false, .. }) && self.open_file(dir, name).is_ok_and(|file| holds_nothing(&file)) {
            self.remove(dir, name, false)?;
            return Ok(None);
        }
        let (target_dir, target_name, dest) = self.move_to(dir, name, shown, into)?;
        // Out of the folder and safe; what is left is cosmetic, and the rescue must still be
        // reported.
        if let Err(e) = self.release(&target_dir, &target_name) {
            tracing::warn!("{} is rescued, but konedrive's marks could not all be taken off it: {e}", dest.display());
        }
        Ok(Some(dest))
    }

    /// Moves `name` out of the folder to `into/<shown>` as [`rescue`](Self::rescue) does, but
    /// keeps it as it is, attributes and all: another account's object, which that account's move
    /// out finds there by its handle (`docs/design/writes.md` §8.3). Only its modes become ordinary ones,
    /// which a read-only folder's lock had changed.
    pub fn set_aside(&self, dir: &File, name: &OsStr, shown: &Path, into: &Path) -> io::Result<PathBuf> {
        let (target_dir, target_name, dest) = self.move_to(dir, name, shown, into)?;
        if let Err(e) = self.open_modes(&target_dir, &target_name) {
            tracing::warn!("{} is set aside, but keeps some read-only modes: {e}", dest.display());
        }
        Ok(dest)
    }

    /// The one rename of [`rescue`](Self::rescue) and [`set_aside`](Self::set_aside): the
    /// directory it went to, its name there, and its path.
    fn move_to(&self, dir: &File, name: &OsStr, shown: &Path, into: &Path) -> io::Result<(File, std::ffi::OsString, PathBuf)> {
        let first = into.join(shown);
        let parent = first.parent().expect("a rescue path has a parent");
        std::fs::create_dir_all(parent)?;
        let target_dir = File::open(parent)?;
        // A directory changing parents needs write permission on itself as
        // well — its `..` changes — whatever mode it has.
        let moved_dir = match self.probe(dir, name)? {
            Probe::Managed { is_dir: true, .. } | Probe::Unmanaged { is_dir: true } => Some(open_subdir(dir, name)?),
            _ => None,
        };
        let held = self.modes.hold();
        let mut n = 0u64;
        loop {
            let dest = if n == 0 { first.clone() } else { into.join(format!("{}.{n}", shown.display())) };
            let target_name = dest.file_name().expect("a rescue path has a name").to_owned();
            let rename = || {
                self.window(&held, dir, || {
                    nix::fcntl::renameat2(dir.as_fd(), name, target_dir.as_fd(), target_name.as_os_str(), RenameFlags::RENAME_NOREPLACE)
                        .map_err(io::Error::from)
                })
            };
            let moved = match &moved_dir {
                Some(moved) => placeholder::with_owner_write(moved, rename),
                None => rename(),
            };
            match moved {
                Ok(()) => return Ok((target_dir, target_name, dest)),
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => n += 1,
                Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                    let source = std::fs::read_link(proc_path(dir)).map(|d| d.join(name)).unwrap_or_else(|_| shown.to_path_buf());
                    return Err(io::Error::new(
                        e.kind(),
                        format!(
                            "{} cannot be rescued to {}: they are on different filesystems, and a rescue is never a copy; it is left where it is",
                            source.display(),
                            dest.display()
                        ),
                    ));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Ordinary modes for `name` and, for a directory, everything in it;
    /// nothing else is changed. Never through a symlink. An entry that cannot be changed
    /// is passed over; the error says how many were.
    fn open_modes(&self, dir: &File, name: &OsStr) -> io::Result<()> {
        let mut failed = 0;
        self.outside(dir, name, false, &mut failed);
        passed_over(failed, "keep a read-only mode")
    }

    /// Strips konedrive's attributes and gives ordinary modes to `name` and,
    /// for a directory, to everything in it. A file of ours in it that holds no content
    /// of its own is removed instead ([`rescue`](Self::rescue) says why). An entry that
    /// cannot be changed is passed over; the error says how many were.
    fn release(&self, dir: &File, name: &OsStr) -> io::Result<()> {
        let mut failed = 0;
        self.outside(dir, name, true, &mut failed);
        passed_over(failed, "keep a mark of konedrive's")
    }

    /// The walk of [`open_modes`](Self::open_modes) and [`release`](Self::release), over
    /// what has left the folder: ordinary modes, and with `strip` no attributes of ours.
    /// One policy for both: a failure is counted and the walk goes on, since what it
    /// walks is already safe where it is.
    fn outside(&self, dir: &File, name: &OsStr, strip: bool, failed: &mut usize) {
        let changed = match self.probe(dir, name) {
            Ok(Probe::Absent) => Ok(()),
            Ok(Probe::Managed { is_dir: true, .. } | Probe::Unmanaged { is_dir: true }) => open_subdir(dir, name).and_then(|sub| {
                placeholder::set_mode(&sub, OPEN_DIR_MODE)?;
                if strip {
                    placeholder::strip(&sub)?;
                }
                for child in self.list(&sub)? {
                    self.outside(&sub, &child, strip, failed);
                }
                Ok(())
            }),
            Ok(probe) => match self.open_file(dir, name) {
                // Not a regular file by now, or not to be opened: nothing of ours on it.
                Err(_) => Ok(()),
                // Inside a rescued directory; or the file `rescue` was asked for, become a
                // placeholder since it looked.
                Ok(file) if strip && matches!(probe, Probe::Managed { .. }) && holds_nothing(&file) => {
                    drop(file);
                    nix::unistd::unlinkat(dir.as_fd(), name, UnlinkatFlags::NoRemoveDir).map_err(io::Error::from)
                }
                Ok(file) => {
                    if strip {
                        // The id first: what is not downloaded went above, so the content is here.
                        placeholder::strip(&file).and_then(|()| placeholder::set_mode(&file, OPEN_FILE_MODE))
                    } else {
                        placeholder::set_mode(&file, OPEN_FILE_MODE)
                    }
                }
            },
            Err(e) => Err(e),
        };
        if let Err(e) = changed {
            tracing::warn!("{}: {e}", name.to_string_lossy());
            *failed += 1;
        }
    }

    /// Takes the lock off the whole folder: `UnregisterRoot`, and a switch to read-write
    /// (`docs/design/writes.md` §2.2). An entry that cannot be changed — a file root owns, a directory
    /// set to `000` by hand — is logged and passed over, never the end of the walk (review
    /// M3). The root comes last, and only when every entry went through: a root still locked
    /// means a walk that did not finish (`SyncService::ensure_unlocked`), and the walk then
    /// says how many entries it could not change.
    pub fn unlock_tree(&self) -> io::Result<()> {
        let _held = self.modes.hold();
        let mut failed = 0usize;
        let mut pending = vec![PathBuf::new()];
        while let Some(rel) = pending.pop() {
            let listed = self.dir(&rel).and_then(|dir| {
                if !rel.as_os_str().is_empty() {
                    placeholder::set_mode(&dir, OPEN_DIR_MODE)?;
                }
                let names = self.list(&dir)?;
                Ok((dir, names))
            });
            let (dir, names) = match listed {
                Ok(listed) => listed,
                Err(e) => {
                    tracing::warn!("cannot unlock {}: {e}", rel.display());
                    failed += 1;
                    continue;
                }
            };
            for name in names {
                let unlocked = nix::sys::stat::fstatat(dir.as_fd(), name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW)
                    .map_err(io::Error::from)
                    .and_then(|stat| match stat.st_mode & libc::S_IFMT {
                        libc::S_IFDIR => {
                            pending.push(rel.join(&name));
                            Ok(())
                        }
                        libc::S_IFREG => match self.open_file(&dir, &name) {
                            Ok(file) => placeholder::set_mode(&file, OPEN_FILE_MODE),
                            Err(_) => Ok(()),
                        },
                        _ => Ok(()),
                    });
                if let Err(e) = unlocked {
                    tracing::warn!("cannot unlock {}: {e}", rel.join(&name).display());
                    failed += 1;
                }
            }
        }
        passed_over(failed, "could not be unlocked; the folder itself stays locked")?;
        placeholder::set_mode(&self.root, OPEN_DIR_MODE)
    }

    /// Puts the lock back on the folder (`docs/design/writes.md` §2.2, a switch to read-only): every file
    /// and directory that carries an item id gets the lock's mode, and the root last. What
    /// carries none is left as it is — the next Full reconcile rescues it, as the read phase
    /// does — and so is a file `claim` says is busy (a fill lifts its write bit around each
    /// attribute write, [`enforce_mode`](Self::enforce_mode)); that reconcile locks it. Only
    /// on a locked `Disk`.
    ///
    /// A directory that cannot be opened or listed and an entry that cannot be looked at or
    /// locked are each logged and passed over, never the end of the walk: the root is locked
    /// whatever was missed, and the next Full reconcile locks the rest
    /// ([`enforce_mode`](Self::enforce_mode)).
    pub fn lock_tree<G>(&self, claim: impl Fn(&File) -> io::Result<Option<G>>) -> io::Result<()> {
        if !self.locked {
            return Ok(());
        }
        let held = self.modes.hold();
        let mut pending = vec![PathBuf::new()];
        while let Some(rel) = pending.pop() {
            let (dir, names) = match self.dir(&rel).and_then(|dir| Ok((self.list(&dir)?, dir))) {
                Ok((names, dir)) => (dir, names),
                Err(e) => {
                    tracing::warn!("cannot lock {}: {e}", rel.display());
                    continue;
                }
            };
            for name in names {
                let locked = self.probe(&dir, &name).and_then(|probe| {
                    let Probe::Managed { is_dir, .. } = probe else { return Ok(()) };
                    if is_dir {
                        pending.push(rel.join(&name));
                    }
                    self.enforce_mode_held(&held, &dir, &name, &claim)
                });
                if let Err(e) = locked {
                    tracing::warn!("cannot lock {}: {e}", rel.join(&name).display());
                }
            }
        }
        placeholder::set_mode(&self.root, LOCKED_DIR_MODE)
    }
}

/// Whether a file of ours holds no content of its own: `online-only`, or cut off mid-fill
/// or mid-free-up. What it holds, if anything, is part of what the cloud has.
fn holds_nothing(file: &File) -> bool {
    matches!(
        placeholder::read_state(file),
        Ok(Some(placeholder::State::OnlineOnly | placeholder::State::Hydrating | placeholder::State::Dehydrating))
    )
}

/// Where a folder's rescued files go. A rescue is always one
/// rename, never a copy ([`Disk::rescue`]), so it has to stay on the root's
/// filesystem: `preferred` (the data directory's `rescued/`) when its nearest
/// existing ancestor is on the root's device, and otherwise
/// `<root's parent>/.konedrive-rescued-<root's name>`.
pub fn rescue_base(root: &Path, preferred: &Path) -> PathBuf {
    let Ok(root_dev) = std::fs::metadata(root).map(|meta| meta.dev()) else {
        return preferred.to_path_buf();
    };
    let mut at = Some(preferred);
    while let Some(path) = at {
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.dev() == root_dev {
                return preferred.to_path_buf();
            }
            break;
        }
        at = path.parent();
    }
    match (root.parent(), root.file_name()) {
        (Some(parent), Some(name)) => {
            let mut beside = OsString::from(".konedrive-rescued-");
            beside.push(name);
            parent.join(beside)
        }
        _ => preferred.to_path_buf(),
    }
}

/// `2026-09-24T10-00-00Z`: the name of one cycle's rescue directory.
pub fn rescue_stamp(now: SystemTime) -> String {
    let seconds = now.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{:02}-{:02}-{:02}Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

/// The date of a day number since 1970-01-01 (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests;
