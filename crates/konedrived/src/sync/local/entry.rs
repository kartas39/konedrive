//! One directory entry as the examination sees it: `lstat` and the
//! attributes read by name (`lgetxattr`), never an open — a placeholder is
//! never filled by being looked at (§3.4).

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::path::{Path, PathBuf};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{Stamp, State, XATTR_CTAG, XATTR_ITEM_ID, XATTR_STAMP, XATTR_STATE};
use nix::errno::Errno;
use nix::fcntl::AtFlags;

use crate::tree::outbox::Inode;
use crate::tree::usable_id;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Type {
    File,
    Dir,
    Symlink,
    Fifo,
    Socket,
    Device,
}

impl Type {
    /// Why it is never uploaded, for anything but a file or a directory.
    pub(super) fn skip_reason(self) -> Option<&'static str> {
        match self {
            Type::File | Type::Dir => None,
            Type::Symlink => Some("symlink"),
            Type::Fifo => Some("fifo"),
            Type::Socket => Some("socket"),
            Type::Device => Some("device"),
        }
    }
}

/// `user.konedrive.state`, as read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StateAttr {
    Absent,
    Known(State),
    /// Set to something no konedrive writes.
    Corrupt,
}

#[derive(Debug, Clone)]
pub(super) struct Entry {
    /// Relative to the root.
    pub rel: PathBuf,
    pub name: OsString,
    pub ty: Type,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    pub size: u64,
    pub mtime: (i64, i64),
    /// A usable item id: one that could be ours.
    pub id: Option<String>,
    pub state: StateAttr,
    pub stamp: Option<Stamp>,
    pub ctag: Option<String>,
    pub handle: Option<FileHandle>,
}

impl Entry {
    pub fn dir_rel(&self) -> &Path {
        self.rel.parent().unwrap_or(Path::new(""))
    }

    pub fn inode(&self) -> Inode {
        Inode { dev: self.dev, ino: self.ino, handle: self.handle.clone() }
    }

    pub fn same_object(&self, other: &Entry) -> bool {
        self.inode().same_object(&other.inode())
    }

    /// Downloaded, and so readable for an upload (WR1).
    pub fn hydrated(&self) -> bool {
        self.state == StateAttr::Known(State::Hydrated)
    }
}

pub(super) fn proc_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn attr(path: &Path, name: &str) -> io::Result<Option<String>> {
    Ok(xattr::get(path, name)?.map(|raw| String::from_utf8_lossy(&raw).into_owned()))
}

/// `name` in `dir` (at `dir_rel`); `None` when there is nothing there.
pub(super) fn read(dir: &File, dir_rel: &Path, name: &OsStr) -> io::Result<Option<Entry>> {
    let stat = match nix::sys::stat::fstatat(dir.as_fd(), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::ENOENT | Errno::ENOTDIR) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let ty = match stat.st_mode & libc::S_IFMT {
        libc::S_IFREG => Type::File,
        libc::S_IFDIR => Type::Dir,
        libc::S_IFLNK => Type::Symlink,
        libc::S_IFIFO => Type::Fifo,
        libc::S_IFSOCK => Type::Socket,
        _ => Type::Device,
    };
    let mut entry = Entry {
        rel: dir_rel.join(name),
        name: name.to_owned(),
        ty,
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
        nlink: stat.st_nlink as u64,
        size: stat.st_size as u64,
        mtime: (stat.st_mtime as i64, stat.st_mtime_nsec as i64),
        id: None,
        state: StateAttr::Absent,
        stamp: None,
        ctag: None,
        handle: None,
    };
    if !matches!(ty, Type::File | Type::Dir) {
        return Ok(Some(entry));
    }
    let path = proc_path(dir).join(name);
    let mut read_attrs = || -> io::Result<()> {
        entry.id = attr(&path, XATTR_ITEM_ID)?.filter(|id| usable_id(id));
        entry.state = match attr(&path, XATTR_STATE)? {
            None => StateAttr::Absent,
            Some(value) => value.parse().map(StateAttr::Known).unwrap_or(StateAttr::Corrupt),
        };
        entry.stamp = attr(&path, XATTR_STAMP)?.as_deref().and_then(Stamp::decode);
        entry.ctag = attr(&path, XATTR_CTAG)?;
        Ok(())
    };
    match read_attrs() {
        Ok(()) => {}
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => return Ok(None),
        Err(e) => return Err(e),
    }
    entry.handle = FileHandle::at(dir, name).ok();
    Ok(Some(entry))
}
