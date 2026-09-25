//! The worker's hands on the folder: finding a row's local object by name,
//! reading its content under a read lease, commit step 1's attributes, the
//! conflict copy's rename, and `user.konedrive.sync`.
//!
//! Everything goes through a directory descriptor opened beneath the root,
//! never a path string. A placeholder is never opened: its state is read by
//! name first, and only a downloaded file or one konedrive does not manage is
//! read (WR1).

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{self, Stamp, State, XATTR_STATE, XATTR_SYNC};
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag};
use nix::sys::stat::Mode;
use xattr::FileExt as _;

use crate::sync::disk::{open_subdir, Disk};
use crate::tree::outbox::Inode;

/// `user.konedrive.sync` values (`docs/design/writes.md` §11).
pub const SYNC_PENDING: &str = "pending";
pub const SYNC_UPLOADING: &str = "uploading";
pub const SYNC_BLOCKED: &str = "blocked";

/// The size and time of the content being sent: the row's snapshot, and
/// the stamp the file gets at the commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Snap {
    pub size: u64,
    pub sec: i64,
    pub nsec: i64,
}

impl Snap {
    pub fn of(file: &File) -> io::Result<Self> {
        let meta = file.metadata()?;
        Ok(Self { size: meta.len(), sec: meta.mtime(), nsec: meta.mtime_nsec() })
    }

    /// As the row keeps it ([`crate::sync::local::snapshot`]).
    pub fn text(self) -> String {
        crate::sync::local::snapshot(self.size, self.sec, self.nsec)
    }

    pub fn stamp(self) -> Stamp {
        Stamp { size: self.size, mtime_sec: self.sec, mtime_nsec: self.nsec }
    }
}

/// A file or directory beneath the root, found by name.
pub(super) struct Found {
    pub rel: PathBuf,
    /// The directory it is in.
    pub dir: File,
    pub name: OsString,
    pub inode: Inode,
    pub is_dir: bool,
}

pub(super) fn proc_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn missing(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP))
}

/// What stands at `rel`, if it is a file or a directory: `fstatat` without
/// following and the handle by name, so nothing is opened or filled.
pub(super) fn find(disk: &Disk, rel: &Path) -> io::Result<Option<Found>> {
    let (Some(name), dir_rel) = (rel.file_name(), rel.parent().unwrap_or(Path::new(""))) else { return Ok(None) };
    let dir = match disk.dir(dir_rel) {
        Ok(dir) => dir,
        Err(e) if missing(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    let stat = match nix::sys::stat::fstatat(dir.as_fd(), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::ENOENT | Errno::ENOTDIR) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let kind = stat.st_mode & libc::S_IFMT;
    if kind != libc::S_IFREG && kind != libc::S_IFDIR {
        return Ok(None);
    }
    let handle = FileHandle::at(&dir, name).ok();
    Ok(Some(Found {
        rel: rel.to_owned(),
        name: name.to_owned(),
        inode: Inode { dev: stat.st_dev as u64, ino: stat.st_ino as u64, handle },
        is_dir: kind == libc::S_IFDIR,
        dir,
    }))
}

/// The size of the regular file at `rel`, by name.
pub(super) fn size_at(disk: &Disk, rel: &Path) -> Option<u64> {
    let name = rel.file_name()?;
    let dir = disk.dir(rel.parent().unwrap_or(Path::new(""))).ok()?;
    let stat = nix::sys::stat::fstatat(dir.as_fd(), name, AtFlags::AT_SYMLINK_NOFOLLOW).ok()?;
    (stat.st_mode & libc::S_IFMT == libc::S_IFREG).then_some(stat.st_size as u64)
}

impl Found {
    fn path(&self) -> PathBuf {
        proc_path(&self.dir).join(&self.name)
    }

    /// `user.konedrive.state`, read by name. `Err` for a value no konedrive
    /// writes.
    pub fn state(&self) -> io::Result<Option<State>> {
        match xattr::get(self.path(), XATTR_STATE)? {
            None => Ok(None),
            Some(raw) => String::from_utf8_lossy(&raw)
                .parse()
                .map(Some)
                .map_err(|()| io::Error::other(format!("{} carries a state konedrive cannot read", self.rel.display()))),
        }
    }

    fn same(&self, file: &File) -> io::Result<File> {
        let meta = file.metadata()?;
        if (meta.dev(), meta.ino()) != (self.inode.dev, self.inode.ino) {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("{} was replaced meanwhile", self.rel.display())));
        }
        file.try_clone()
    }

    /// The file, read-only. Only for a file the caller found downloaded or
    /// unmanaged: the daemon's own open is never intercepted, and a
    /// placeholder's would read zeros.
    pub fn open(&self) -> io::Result<File> {
        let fd = nix::fcntl::openat(
            self.dir.as_fd(),
            self.name.as_os_str(),
            OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?;
        self.same(&File::from(fd))
    }

    pub fn open_dir(&self) -> io::Result<File> {
        self.same(&open_subdir(&self.dir, &self.name)?)
    }
}

/// Holds a read lease: a writer's open waits until it is dropped (the
/// milliseconds of one read), and none is granted while anyone writes.
struct ReadLease<'a>(&'a File);

impl<'a> ReadLease<'a> {
    fn take(file: &'a File) -> io::Result<Option<Self>> {
        // The probe also makes the lease break's SIGIO harmless first.
        if konedrive_fs::lease::open_for_writing(file)? {
            return Ok(None);
        }
        // SAFETY: plain fcntl on a valid descriptor.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, libc::F_RDLCK) } == 0 {
            return Ok(Some(Self(file)));
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EAGAIN) {
            Ok(None)
        } else {
            Err(error)
        }
    }
}

impl Drop for ReadLease<'_> {
    fn drop(&mut self) {
        // SAFETY: releasing the lease this descriptor holds.
        unsafe { libc::fcntl(self.0.as_raw_fd(), libc::F_SETLEASE, libc::F_UNLCK) };
    }
}

/// What reading part of a file gave.
pub(super) enum Read {
    Bytes(Vec<u8>),
    /// Someone has it open for writing.
    Busy,
    /// Not downloaded (any more): its bytes are not the item's (WR1).
    NotLocal,
    /// Its size or time moved from the snapshot.
    Changed,
}

/// `len` bytes at `offset`, under a read lease, from a file still downloaded
/// or unmanaged and still as the snapshot says.
pub(super) fn read(file: &File, offset: u64, len: usize, snap: Snap) -> io::Result<Read> {
    match placeholder::read_state(file) {
        Ok(None | Some(State::Hydrated)) => {}
        _ => return Ok(Read::NotLocal),
    }
    let Some(lease) = ReadLease::take(file)? else { return Ok(Read::Busy) };
    let mut bytes = vec![0; len];
    let result = file.read_exact_at(&mut bytes, offset);
    drop(lease);
    match result {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(Read::Changed),
        Err(e) => return Err(e),
    }
    if Snap::of(file)? != snap {
        return Ok(Read::Changed);
    }
    Ok(Read::Bytes(bytes))
}

/// Commit step 1, first half (§3.5): the stamp from the snapshot, the cTag,
/// `hydrated`, then `fsync`.
pub(super) fn commit_attributes(file: &File, snap: Snap, ctag: Option<&str>) -> io::Result<()> {
    placeholder::write_given_stamp(file, snap.stamp())?;
    if let Some(ctag) = ctag {
        placeholder::write_ctag(file, ctag)?;
    }
    placeholder::write_state(file, State::Hydrated)?;
    file.sync_all()
}

/// Commit step 1, second half: the item id last — an id with no state is
/// the one combination the helper refuses — then `fsync`; and the file's
/// row is no longer pending.
pub(super) fn commit_id(file: &File, id: &str) -> io::Result<()> {
    placeholder::write_item_id(file, id)?;
    file.sync_all()?;
    clear_sync_of(file);
    Ok(())
}

/// A directory's commit step 1: its item id, then `fsync`. A directory
/// removed meanwhile has nothing to mark: what fails on it is no failure.
pub(super) fn commit_dir(dir: &File, id: &str) -> io::Result<()> {
    match placeholder::write_item_id(dir, id).and_then(|()| dir.sync_all()) {
        Err(e) if dir.metadata().is_ok_and(|m| m.nlink() == 0) => {
            tracing::debug!("a folder removed before its commit keeps no item id: {e}");
            Ok(())
        }
        other => other,
    }
}

/// Takes konedrive's attributes off: the object is the user's own file (a
/// copy, or content uploaded again as new).
pub(super) fn strip(file: &File) -> io::Result<()> {
    placeholder::strip_konedrive_xattrs(file)?;
    file.sync_all()
}

/// Writes `user.konedrive.sync` on `file`. Best effort: the emblem is
/// cosmetic, and a file the user made read-only for everyone keeps none.
pub(super) fn set_sync_of(file: &File, value: &str) {
    if let Err(e) = placeholder::with_owner_write(file, || file.set_xattr(XATTR_SYNC, value.as_bytes())) {
        tracing::debug!("cannot mark a file {value}: {e}");
    }
}

pub(super) fn clear_sync_of(file: &File) {
    let removed = placeholder::with_owner_write(file, || match file.remove_xattr(XATTR_SYNC) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::ENODATA) => Ok(()),
        Err(e) => Err(e),
    });
    if let Err(e) = removed {
        tracing::debug!("cannot clear a file's upload mark: {e}");
    }
}

/// Takes `user.konedrive.sync` off the object `found`, by name.
pub(super) fn clear_mark(found: &Found) {
    if found.is_dir {
        return;
    }
    match xattr::remove(found.path(), XATTR_SYNC) {
        Err(e) if e.raw_os_error() != Some(libc::ENODATA) => tracing::debug!("cannot clear the mark of {}: {e}", found.rel.display()),
        _ => {}
    }
}

/// `user.konedrive.sync` on the file at `rel`, set to `value` or taken off,
/// by name (a placeholder is never opened). Best effort.
pub(super) fn mark(disk: &Disk, rel: &Path, value: Option<&str>) {
    let Ok(Some(found)) = find(disk, rel) else { return };
    if found.is_dir {
        return;
    }
    let path = found.path();
    let result = match value {
        Some(value) => xattr::set(&path, XATTR_SYNC, value.as_bytes()),
        None => match xattr::remove(&path, XATTR_SYNC) {
            Err(e) if e.raw_os_error() == Some(libc::ENODATA) => Ok(()),
            other => other,
        },
    };
    if let Err(e) = result {
        tracing::debug!("cannot mark {}: {e}", rel.display());
    }
}

/// `name` with the machine's name added before its extension (write design
/// §6): `Report.docx` → `Report-fedora.docx`, `archive.tar.gz` →
/// `archive.tar-fedora.gz`, `.bashrc` → `.bashrc-fedora`; the `n`th try adds
/// `-n` (`Report-fedora-2.docx`). The stem is shortened to keep the name
/// within 255 bytes.
pub fn copy_name(name: &str, machine: &str, n: u32) -> String {
    let suffix = if n <= 1 { format!("-{machine}") } else { format!("-{machine}-{n}") };
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    let room = crate::drive::item::NAME_MAX.saturating_sub(suffix.len() + ext.len());
    let mut cut = stem.len().min(room);
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{suffix}{ext}", &stem[..cut])
}

/// The machine's name for conflict copies (`docs/design/writes.md` §7): the host name
/// up to its first dot, with any character OneDrive refuses replaced by
/// `-`, at most 32 characters; `linux` when there is none.
pub fn machine_name(host: &str) -> String {
    let first = host.trim().split('.').next().unwrap_or_default();
    let cleaned: String = first
        .chars()
        .map(|c| if matches!(c, '"' | '*' | ':' | '<' | '>' | '?' | '/' | '\\' | '|') || c.is_control() { '-' } else { c })
        .take(32)
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "linux".into()
    } else {
        cleaned.to_owned()
    }
}

/// [`machine_name`] of this host.
pub fn default_machine_name() -> String {
    machine_name(&std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default())
}

/// Renames `found` in its directory to the first free [`copy_name`]:
/// `RENAME_NOREPLACE`, so the copy can never land on anything. The new name.
pub(super) fn rename_to_copy(disk: &Disk, found: &Found, machine: &str) -> io::Result<String> {
    let name = found.name.to_str().ok_or_else(|| io::Error::other("a name that is not UTF-8 gets no copy"))?;
    for n in 1..=100 {
        let candidate = copy_name(name, machine, n);
        match disk.rename(&found.dir, &found.name, &found.dir, OsStr::new(&candidate)) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other(format!("no free name for a copy of {name}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_are_named_after_the_machine_before_the_extension() {
        assert_eq!(copy_name("Report.docx", "fedora", 1), "Report-fedora.docx");
        assert_eq!(copy_name("archive.tar.gz", "fedora", 1), "archive.tar-fedora.gz");
        assert_eq!(copy_name(".bashrc", "fedora", 1), ".bashrc-fedora");
        assert_eq!(copy_name("notes", "fedora", 3), "notes-fedora-3");
        let long = "я".repeat(200) + ".txt";
        let copy = copy_name(&long, "fedora", 1);
        assert!(copy.len() <= 255 && copy.ends_with("-fedora.txt"), "{}", copy.len());
        assert_eq!(machine_name("work-laptop.example.org\n"), "work-laptop");
        assert_eq!(machine_name("a:b?c"), "a-b-c");
        assert_eq!(machine_name(""), "linux");
        assert_eq!(machine_name(&"x".repeat(40)).len(), 32);
    }
}
