use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::proc_path;
use nix::fcntl::{openat2, AtFlags, OFlag, OpenHow, ResolveFlag};
use crate::folder::disk::Disk;
use crate::local::liveness::same_place;

use super::trash::{is_mount_point, real_trash, trash_of, TrashEntry};
use super::MoveOuts;

// ---------------------------------------------------------------------------
// where an object is
// ---------------------------------------------------------------------------

/// Where the object behind `fd` is, proved: `/proc/self/fd` read, and that path opened again
/// by the user's own lookups names the same inode. A file whose dentry the kernel could not
/// connect reads as `/`, which this refuses.
pub(super) fn verified_path(fd: &File) -> Option<PathBuf> {
    let path = std::fs::read_link(proc_path(fd)).ok()?;
    same_place(&path, fd).then_some(path)
}

/// Where a moved-out object is.
pub(super) enum Place {
    /// Beneath this account's folder, proved by its handle there: the examination's.
    Inside,
    Trash(TrashEntry),
    /// Anywhere else, at this proved path — or `None`, a place that cannot be proved (a file's
    /// dentry disconnected after a reboot): downloaded, but not stripped or deleted until proved.
    Elsewhere(Option<PathBuf>),
    /// Beneath this account's folder, but not proved: nothing is decided.
    Unknown,
}

/// The real path of the folder's root.
pub(super) fn root_path(disk: &Disk) -> Option<PathBuf> {
    std::fs::read_link(proc_path(&disk.dir(Path::new("")).ok()?)).ok()
}

pub(super) fn place_of(mo: &MoveOuts, disk: &Disk, fd: &File, handle: &FileHandle) -> Place {
    let Some(path) = verified_path(fd) else { return Place::Elsewhere(None) };
    if let Some(rel) = root_path(disk).and_then(|root| path.strip_prefix(root).ok().map(Path::to_path_buf)) {
        let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else { return Place::Unknown };
        return match disk.dir(parent).ok().and_then(|dir| FileHandle::at(&dir, name).ok()) {
            Some(there) if &there == handle => Place::Inside,
            _ => Place::Unknown,
        };
    }
    let home = mo.home_trash.as_deref();
    match trash_of(&path, home, nix::unistd::geteuid().as_raw(), &is_mount_point).filter(real_trash) {
        Some(entry) => Place::Trash(entry),
        None => Place::Elsewhere(Some(path)),
    }
}

/// Every registered folder's real path, this account's included.
fn roots(mo: &MoveOuts, disk: &Disk) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = (mo.roots)().iter().filter_map(|r| std::fs::canonicalize(r).ok()).collect();
    roots.extend(root_path(disk));
    roots
}

/// Whether `path` is beneath (or is) a registered folder.
pub(super) fn beneath_a_root(mo: &MoveOuts, disk: &Disk, path: &Path) -> bool {
    roots(mo, disk).iter().any(|root| path.starts_with(root))
}

/// Whether `path` is beneath a registered folder other than this account's: another account's.
pub(super) fn in_another_folder(mo: &MoveOuts, disk: &Disk, path: &Path) -> bool {
    let own = root_path(disk);
    (mo.roots)()
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .filter(|root| own.as_ref() != Some(root))
        .any(|root| path.starts_with(root))
}

/// The directory at `path`, by path, through the user's own lookups and no symlink.
pub(super) fn reopen_parent(path: &Path) -> io::Result<File> {
    let how = OpenHow::new()
        .flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
        .resolve(ResolveFlag::RESOLVE_NO_SYMLINKS | ResolveFlag::RESOLVE_NO_MAGICLINKS);
    Ok(File::from(openat2(nix::fcntl::AT_FDCWD, path, how)?))
}

pub(super) fn parent_has(parent: &File, name: &OsStr) -> bool {
    nix::sys::stat::fstatat(parent.as_fd(), name, AtFlags::AT_SYMLINK_NOFOLLOW).is_ok()
}
