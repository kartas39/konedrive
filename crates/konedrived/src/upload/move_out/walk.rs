use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use konedrive_fs::placeholder::XATTR_ITEM_ID;
use nix::fcntl::{openat2, AtFlags, OFlag, OpenHow, ResolveFlag};
use xattr::FileExt;

use super::place::{proc_path, reopen_parent};

// ---------------------------------------------------------------------------
// walking a moved-out folder
// ---------------------------------------------------------------------------

/// The directory at `path`, opened by path — through the user's own lookups, never beneath a
/// descriptor `OpenByHandle` gave (F90) — and only if it is still `object`.
pub(super) fn reopen_dir(path: &Path, object: &File) -> io::Result<Option<File>> {
    let dir = reopen_parent(path)?;
    let (a, b) = (dir.metadata()?, object.metadata()?);
    Ok(((a.dev(), a.ino()) == (b.dev(), b.ino())).then_some(dir))
}

/// `rel` below `top` — a directory opened by path, never one `OpenByHandle` gave — opened beneath
/// it, never through a symlink: a regular file read-only (`O_NONBLOCK`; the daemon's own open is
/// never intercepted), or a directory.
fn open_below(top: &File, rel: &Path, is_dir: bool) -> io::Result<File> {
    let flags = if is_dir { OFlag::O_RDONLY | OFlag::O_DIRECTORY } else { OFlag::O_RDONLY | OFlag::O_NONBLOCK };
    let how = OpenHow::new()
        .flags(flags | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
        .resolve(ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_SYMLINKS | ResolveFlag::RESOLVE_NO_MAGICLINKS);
    Ok(File::from(openat2(top.as_fd(), rel, how)?))
}

pub(super) fn item_id_of(file: &File) -> Option<String> {
    file.get_xattr(XATTR_ITEM_ID).ok().flatten().and_then(|v| String::from_utf8(v).ok())
}

/// One regular file or directory met below a moved-out folder, by its place below it.
#[derive(Debug, Clone)]
pub(super) struct Met {
    /// Relative to the folder.
    pub(super) rel: PathBuf,
    pub(super) id: Option<String>,
    pub(super) is_dir: bool,
    /// `(st_dev, st_ino)` when it was listed: what is opened again at `rel` must still be it.
    key: (u64, u64),
}

impl Met {
    /// Its directory, relative to the folder.
    pub(super) fn dir(&self) -> &Path {
        self.rel.parent().unwrap_or(Path::new(""))
    }
}

/// Every regular file and directory below `top` (a directory opened by path), on its device,
/// parents before children: listed one directory at a time, each opened beneath `top`, never
/// through a symlink, and closed again; attributes read by name, never through a symlink.
/// Nothing below is filled.
pub(super) fn walk(top: &File) -> io::Result<Vec<Met>> {
    let dev = top.metadata()?.dev();
    let mut out = Vec::new();
    let mut dirs: Vec<(PathBuf, Option<(u64, u64)>)> = vec![(PathBuf::new(), None)];
    while let Some((at_rel, key)) = dirs.pop() {
        let at = if at_rel.as_os_str().is_empty() {
            top.try_clone()?
        } else {
            match open_below(top, &at_rel, true) {
                Ok(dir) => dir,
                Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => continue,
                Err(e) => return Err(e),
            }
        };
        if let Some(key) = key {
            let meta = at.metadata()?;
            if (meta.dev(), meta.ino()) != key {
                continue;
            }
        }
        let mut names: Vec<OsString> = std::fs::read_dir(proc_path(&at))?.filter_map(|e| e.ok().map(|e| e.file_name())).collect();
        names.sort();
        for name in names {
            let stat = match nix::sys::stat::fstatat(at.as_fd(), name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                Err(nix::errno::Errno::ENOENT) => continue,
                Err(e) => return Err(e.into()),
            };
            let kind = stat.st_mode & libc::S_IFMT;
            if (kind != libc::S_IFREG && kind != libc::S_IFDIR) || stat.st_dev != dev {
                continue;
            }
            let is_dir = kind == libc::S_IFDIR;
            let rel = at_rel.join(&name);
            let key = (stat.st_dev, stat.st_ino);
            // `lgetxattr` on the name: a symlink swapped in meanwhile is not followed.
            let id = xattr::get(proc_path(&at).join(&name), XATTR_ITEM_ID).ok().flatten().and_then(|v| String::from_utf8(v).ok());
            if is_dir {
                dirs.push((rel.clone(), Some(key)));
            }
            out.push(Met { rel, id, is_dir, key });
        }
    }
    Ok(out)
}

/// What the walk met at `m`, opened again beneath `top`, and only if it is still that.
pub(super) fn open_met(top: &File, m: &Met) -> io::Result<File> {
    let file = open_below(top, &m.rel, m.is_dir)?;
    let meta = file.metadata()?;
    if (meta.dev(), meta.ino()) != m.key {
        return Err(io::Error::other(format!("{} changed while it was looked at", m.rel.display())));
    }
    Ok(file)
}

/// The directory `rel` below `top` (`top` itself for `""`).
pub(super) fn dir_below(top: &File, rel: &Path) -> io::Result<File> {
    if rel.as_os_str().is_empty() {
        top.try_clone()
    } else {
        open_below(top, rel, true)
    }
}
