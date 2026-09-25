//! The materializer's hands. Every change it makes to
//! the folder goes through a directory descriptor opened beneath the root —
//! never a path string a rename could redirect — and every change to a locked
//! directory through a window of its own.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use konedrive_fs::placeholder::{self, LOCKED_DIR_MODE, LOCKED_FILE_MODE, OPEN_DIR_MODE, OPEN_FILE_MODE, XATTR_ITEM_ID};
use nix::errno::Errno;
use nix::fcntl::{openat2, AtFlags, OFlag, OpenHow, RenameFlags, ResolveFlag};
use nix::sys::stat::Mode;
use nix::unistd::UnlinkatFlags;

use super::root::SyncRoot;
use crate::tree::usable_id;

/// Where misplaced items wait during a reconcile.
pub const HOLDING: &str = ".konedrive-holding";
/// A new folder's name until it is labelled and marked.
pub const NEW_PREFIX: &str = ".konedrive-new-";

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

pub struct Disk {
    root: File,
    locked: bool,
}

/// Held by everything in this daemon that lifts a locked directory's write
/// bit and puts it back: the materializer's windows ([`Disk::writable`]),
/// its locking, and a pin written on a folder (`SyncService::pin`). Two
/// such windows on one directory used to be able to interleave — one put
/// the lock back while the other was still writing, which failed `EACCES`.
///
/// Re-entrant on one thread (windows nest: a rename opens three), and never
/// held across an `.await`.
static DIR_MODES: std::sync::Mutex<()> = std::sync::Mutex::new(());

thread_local! {
    static DIR_MODES_HELD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// This thread's hold on [`DIR_MODES`], released when dropped. The guard is
/// never read: it only has to live as long as this does.
pub struct DirModes(#[allow(dead_code)] Option<std::sync::MutexGuard<'static, ()>>);

/// Takes [`DIR_MODES`], or joins this thread's hold on it.
pub fn dir_modes() -> DirModes {
    let outermost = DIR_MODES_HELD.with(|held| {
        held.set(held.get() + 1);
        held.get() == 1
    });
    DirModes(outermost.then(|| DIR_MODES.lock().unwrap_or_else(|poisoned| poisoned.into_inner())))
}

impl Drop for DirModes {
    fn drop(&mut self) {
        DIR_MODES_HELD.with(|held| held.set(held.get() - 1));
    }
}

fn beneath() -> ResolveFlag {
    ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_SYMLINKS | ResolveFlag::RESOLVE_NO_MAGICLINKS
}

fn proc_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
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
        Ok(Self { root: dir, locked })
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
        match xattr::get(proc_path(dir).join(name), XATTR_ITEM_ID)? {
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
        let _modes = dir_modes();
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
        let in_windows = || self.writable(from_dir, || self.writable(to_dir, rename));
        let moved_dir = matches!(self.probe(from_dir, from)?, Probe::Managed { is_dir: true, .. } | Probe::Unmanaged { is_dir: true });
        if self.locked && moved_dir {
            let moved = open_subdir(from_dir, from)?;
            self.writable(&moved, in_windows)
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
            let _modes = dir_modes();
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
        let _modes = is_dir.then(dir_modes);
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
    /// descriptors (part 1's).
    pub fn scan(&self, root_item_id: &str) -> io::Result<Vec<Scanned>> {
        let mut out = Vec::new();
        let mut pending = vec![(PathBuf::new(), 0usize, Some(root_item_id.to_owned()))];
        while let Some((rel, depth, dir_id)) = pending.pop() {
            if depth >= konedrive_fs::MAX_DEPTH {
                tracing::warn!("{} is deeper than {} levels; not scanned", rel.display(), konedrive_fs::MAX_DEPTH);
                continue;
            }
            let dir = match self.dir(&rel) {
                Ok(dir) => dir,
                Err(e) => {
                    tracing::warn!("cannot scan {}: {e}", rel.display());
                    continue;
                }
            };
            for name in self.list(&dir)? {
                let (id, is_dir) = match self.probe(&dir, &name)? {
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
    /// A directory goes with everything in it.
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
    pub fn rescue(&self, dir: &File, name: &OsStr, shown: &Path, into: &Path) -> io::Result<PathBuf> {
        let (target_dir, target_name, dest) = self.move_to(dir, name, shown, into)?;
        // Out of the folder and safe; what is left is cosmetic, and the rescue must still be
        // reported.
        if let Err(e) = self.release(&target_dir, &target_name, false) {
            tracing::warn!("{} is rescued, but konedrive's marks could not all be taken off it: {e}", dest.display());
        }
        Ok(dest)
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
        let mut n = 0u64;
        loop {
            let dest = if n == 0 { first.clone() } else { into.join(format!("{}.{n}", shown.display())) };
            let target_name = dest.file_name().expect("a rescue path has a name").to_owned();
            let rename = || {
                self.writable(dir, || {
                    nix::fcntl::renameat2(dir.as_fd(), name, target_dir.as_fd(), target_name.as_os_str(), RenameFlags::RENAME_NOREPLACE)
                        .map_err(io::Error::from)
                })
            };
            let moved = match &moved_dir {
                Some(moved) => {
                    let _modes = dir_modes();
                    placeholder::with_owner_write(moved, rename)
                }
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
    /// nothing else is changed. Never through a symlink.
    fn open_modes(&self, dir: &File, name: &OsStr) -> io::Result<()> {
        match self.probe(dir, name)? {
            Probe::Managed { is_dir: true, .. } | Probe::Unmanaged { is_dir: true } => {
                let sub = open_subdir(dir, name)?;
                placeholder::set_mode(&sub, OPEN_DIR_MODE)?;
                for child in self.list(&sub)? {
                    self.open_modes(&sub, &child)?;
                }
                Ok(())
            }
            Probe::Managed { is_dir: false, .. } | Probe::Unmanaged { is_dir: false } => match self.open_file(dir, name) {
                Ok(file) => placeholder::set_mode(&file, OPEN_FILE_MODE),
                Err(_) => Ok(()),
            },
            Probe::Absent => Ok(()),
        }
    }

    /// Strips konedrive's attributes and gives ordinary modes to `name` and,
    /// for a directory, to everything in it.
    ///
    /// A file of ours inside a rescued directory (`inside`) that holds no
    /// content of its own — `online-only`, or cut off mid-fill or mid-free-up
    /// — is removed instead: stripped of its state,
    /// it read as a file of zeros in the rescue directory, and it is nothing
    /// anyone made here; the cloud still has it.
    fn release(&self, dir: &File, name: &OsStr, inside: bool) -> io::Result<()> {
        match self.probe(dir, name)? {
            Probe::Absent => Ok(()),
            Probe::Managed { is_dir: true, .. } | Probe::Unmanaged { is_dir: true } => {
                let sub = open_subdir(dir, name)?;
                placeholder::set_mode(&sub, OPEN_DIR_MODE)?;
                placeholder::strip_konedrive_xattrs(&sub)?;
                for child in self.list(&sub)? {
                    self.release(&sub, &child, true)?;
                }
                Ok(())
            }
            probe @ (Probe::Managed { is_dir: false, .. } | Probe::Unmanaged { is_dir: false }) => {
                let Ok(file) = self.open_file(dir, name) else { return Ok(()) };
                let empty = matches!(
                    placeholder::read_state(&file),
                    Ok(Some(placeholder::State::OnlineOnly | placeholder::State::Hydrating | placeholder::State::Dehydrating))
                );
                if inside && empty && matches!(probe, Probe::Managed { .. }) {
                    drop(file);
                    return nix::unistd::unlinkat(dir.as_fd(), name, UnlinkatFlags::NoRemoveDir).map_err(io::Error::from);
                }
                placeholder::strip_konedrive_xattrs(&file)?;
                placeholder::set_mode(&file, OPEN_FILE_MODE)
            }
        }
    }

    /// Takes the lock off the whole folder: `UnregisterRoot`, and a switch to read-write
    /// (`docs/design/writes.md` §2.2). An entry that cannot be changed — a file root owns, a directory
    /// set to `000` by hand — is logged and passed over, never the end of the walk (review
    /// M3). The root comes last, and only when every entry went through: a root still locked
    /// means a walk that did not finish (`SyncService::ensure_unlocked`), and the walk then
    /// says how many entries it could not change.
    pub fn unlock_tree(&self) -> io::Result<()> {
        let _modes = dir_modes();
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
        if failed > 0 {
            return Err(io::Error::other(format!("{failed} entries could not be unlocked; the folder itself stays locked")));
        }
        placeholder::set_mode(&self.root, OPEN_DIR_MODE)
    }

    /// Puts the lock back on the folder (`docs/design/writes.md` §2.2, a switch to read-only): every file
    /// and directory that carries an item id gets the lock's mode, and the root last. What
    /// carries none is left as it is — the next Full reconcile rescues it, as the read phase
    /// does — and so is a file `claim` says is busy (a fill lifts its write bit around each
    /// attribute write, [`enforce_mode`](Self::enforce_mode)); that reconcile locks it. One
    /// entry that cannot be locked is logged and passed over. Only on a locked `Disk`.
    pub fn lock_tree<G>(&self, claim: impl Fn(&File) -> io::Result<Option<G>>) -> io::Result<()> {
        if !self.locked {
            return Ok(());
        }
        let _modes = dir_modes();
        let mut pending = vec![PathBuf::new()];
        while let Some(rel) = pending.pop() {
            let dir = match self.dir(&rel) {
                Ok(dir) => dir,
                Err(e) => {
                    tracing::warn!("cannot lock {}: {e}", rel.display());
                    continue;
                }
            };
            for name in self.list(&dir)? {
                let Ok(Probe::Managed { is_dir, .. }) = self.probe(&dir, &name) else { continue };
                if is_dir {
                    pending.push(rel.join(&name));
                }
                if let Err(e) = self.enforce_mode(&dir, &name, &claim) {
                    tracing::warn!("cannot lock {}: {e}", rel.join(&name).display());
                }
            }
        }
        self.lock_dir(&self.root)
    }
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
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use konedrive_fs::placeholder::XATTR_ROOT;

    use super::*;

    /// A directory on the filesystem of the build's `target/` (on this machine
    /// `/home`, while temporary directories are on tmpfs).
    fn on_target_fs() -> tempfile::TempDir {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::tempdir_in(base.canonicalize().unwrap()).unwrap()
    }

    fn same_device(a: &Path, b: &Path) -> bool {
        std::fs::metadata(a).unwrap().dev() == std::fs::metadata(b).unwrap().dev()
    }

    /// The rescue never copies: across filesystems it fails, naming both
    /// paths, and leaves the file exactly as it was — content, attributes and
    /// mode.
    #[test]
    fn a_rescue_across_filesystems_fails_and_leaves_the_source_intact() {
        let (_dir, path, disk) = unlocked_root();
        let into = on_target_fs();
        if same_device(&path, into.path()) {
            eprintln!("skipping: {} and {} are on one filesystem", path.display(), into.path().display());
            return;
        }
        let source = path.join("f.txt");
        std::fs::write(&source, b"local work").unwrap();
        xattr::set(&source, XATTR_ITEM_ID, b"F").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o444)).unwrap();
        let root = disk.dir(Path::new("")).unwrap();
        let err = disk.rescue(&root, OsStr::new("f.txt"), Path::new("docs/f.txt"), into.path()).unwrap_err();
        let message = err.to_string();
        assert!(message.contains(&source.display().to_string()), "{message}");
        assert!(message.contains(&into.path().join("docs/f.txt").display().to_string()), "{message}");
        assert_eq!(std::fs::read(&source).unwrap(), b"local work");
        assert_eq!(xattr::get(&source, XATTR_ITEM_ID).unwrap().as_deref(), Some(&b"F"[..]));
        assert_eq!(std::fs::metadata(&source).unwrap().permissions().mode() & 0o7777, 0o444);
        assert!(!into.path().join("docs/f.txt").exists());
    }

    /// Rescues go where the user expects them whenever a rename can get them
    /// there: the preferred place, not yet made, on the root's filesystem.
    #[test]
    fn the_rescue_base_is_the_preferred_one_on_the_roots_filesystem() {
        let (_dir, path, _disk) = unlocked_root();
        let data = tempfile::tempdir().unwrap();
        if !same_device(&path, data.path()) {
            eprintln!("skipping: {} and {} are on different filesystems", path.display(), data.path().display());
            return;
        }
        let preferred = data.path().join("konedrive/rescued");
        assert_eq!(rescue_base(&path, &preferred), preferred);
    }

    /// On another filesystem a rescue would need a copy; it goes beside the
    /// root instead, where one rename reaches.
    #[test]
    fn the_rescue_base_is_beside_the_root_when_the_preferred_one_is_on_another_filesystem() {
        let parent = on_target_fs();
        let root = parent.path().join("OneDrive");
        std::fs::create_dir(&root).unwrap();
        let data = tempfile::tempdir().unwrap();
        if same_device(&root, data.path()) {
            eprintln!("skipping: {} and {} are on one filesystem", root.display(), data.path().display());
            return;
        }
        let preferred = data.path().join("konedrive/rescued");
        assert_eq!(rescue_base(&root, &preferred), parent.path().join(".konedrive-rescued-OneDrive"));
    }

    /// An item id becomes a name in the holding directory, so an id that
    /// cannot be one is never taken for ours.
    #[test]
    fn an_item_id_that_cannot_be_a_file_name_is_not_ours() {
        let (_dir, path, disk) = unlocked_root();
        let root = disk.dir(Path::new("")).unwrap();
        for (n, id) in [&b""[..], b".", b"..", b"a/b", b"a\0b"].into_iter().enumerate() {
            let name = format!("f{n}");
            std::fs::write(path.join(&name), b"").unwrap();
            xattr::set(path.join(&name), XATTR_ITEM_ID, id).unwrap();
            assert_eq!(disk.probe(&root, OsStr::new(&name)).unwrap(), Probe::Unmanaged { is_dir: false }, "{id:?}");
        }
        std::fs::create_dir(path.join("d")).unwrap();
        xattr::set(path.join("d"), XATTR_ITEM_ID, b"..").unwrap();
        assert_eq!(disk.probe(&root, OsStr::new("d")).unwrap(), Probe::Unmanaged { is_dir: true });
        std::fs::write(path.join("ok"), b"").unwrap();
        xattr::set(path.join("ok"), XATTR_ITEM_ID, b"8F6C!101").unwrap();
        assert_eq!(disk.probe(&root, OsStr::new("ok")).unwrap(), Probe::Managed { id: "8F6C!101".into(), is_dir: false });
    }

    /// An unlocked registered root, its path, and the `Disk` on it.
    fn unlocked_root() -> (tempfile::TempDir, PathBuf, Disk) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap();
        let root_id = "3a9d5c1e-7f20-4b6a-8e4d-2c1b0a9f8e7d".to_owned();
        xattr::set(&path, XATTR_ROOT, root_id.as_bytes()).unwrap();
        let disk = Disk::open(&SyncRoot { path: path.clone(), root_id }, false).unwrap();
        (dir, path, disk)
    }

    /// A rename never lands on anything: whatever appeared at the target
    /// between the materializer's probe and its rename is left alone, and the
    /// move fails instead.
    #[test]
    fn a_rename_never_replaces_what_is_at_the_target() {
        let (_dir, path, disk) = unlocked_root();
        std::fs::write(path.join("a"), b"ours").unwrap();
        std::fs::write(path.join("b"), b"the user's").unwrap();
        let root = disk.dir(Path::new("")).unwrap();
        let err = disk.rename(&root, OsStr::new("a"), &root, OsStr::new("b")).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EEXIST));
        assert_eq!(std::fs::read(path.join("b")).unwrap(), b"the user's");
        assert_eq!(std::fs::read(path.join("a")).unwrap(), b"ours");
    }

    /// A `swap_in` whose `renameat` fails after its
    /// `linkat` already succeeded does not leave the temporary link behind —
    /// a leftover a later Full reconcile would find under the same item id
    /// as the file it was meant to replace.
    #[test]
    fn swap_in_removes_its_temporary_link_when_the_rename_fails() {
        let (_dir, path, disk) = unlocked_root();
        let root = disk.dir(Path::new("")).unwrap();
        // Renaming a regular file onto an existing directory always fails
        // (EISDIR), which is enough to exercise the cleanup regardless of
        // filesystem or kernel.
        std::fs::create_dir(path.join("target")).unwrap();
        let file = disk.tmpfile(&root).unwrap();
        let err = disk.swap_in(&root, &file, OsStr::new("temp"), OsStr::new("target")).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EISDIR), "{err:?}");
        assert!(!path.join("temp").exists(), "the temporary link does not outlive a failed swap");
        assert!(path.join("target").is_dir(), "the target is untouched");
    }

    /// A rescue never replaces an earlier one: whatever is already at the
    /// destination — rescued before, or put there in a race — stays, and the
    /// file goes to the next free `.<n>` name.
    #[test]
    fn a_rescue_never_replaces_what_is_already_there() {
        let (_dir, path, disk) = unlocked_root();
        std::fs::create_dir(path.join("docs")).unwrap();
        std::fs::write(path.join("docs/f.txt"), b"third").unwrap();
        let into = tempfile::tempdir().unwrap();
        std::fs::create_dir(into.path().join("docs")).unwrap();
        std::fs::write(into.path().join("docs/f.txt"), b"first").unwrap();
        std::fs::write(into.path().join("docs/f.txt.1"), b"second").unwrap();
        let docs = disk.dir(Path::new("docs")).unwrap();
        let dest = disk.rescue(&docs, OsStr::new("f.txt"), Path::new("docs/f.txt"), into.path()).unwrap();
        assert_eq!(dest, into.path().join("docs/f.txt.2"));
        assert_eq!(std::fs::read(into.path().join("docs/f.txt")).unwrap(), b"first");
        assert_eq!(std::fs::read(into.path().join("docs/f.txt.1")).unwrap(), b"second");
        assert_eq!(std::fs::read(&dest).unwrap(), b"third");
        assert!(!path.join("docs/f.txt").exists());
    }

    #[test]
    fn a_rescue_stamp_is_the_utc_date_and_time() {
        assert_eq!(rescue_stamp(UNIX_EPOCH + Duration::from_secs(1_714_557_600)), "2024-05-01T10-00-00Z");
    }

    #[test]
    fn the_epoch_is_the_first_stamp() {
        assert_eq!(rescue_stamp(UNIX_EPOCH), "1970-01-01T00-00-00Z");
    }
}
