//! A finished download given its place in the folder.
//!
//! A download is built in a file that has no name: it is in the folder's filesystem and in no
//! directory, so nothing ever lists a file that is not whole, and one the daemon leaves behind
//! goes by itself. Once it is whole and checked it lands: it is stamped as its item's copy and
//! linked in at the item's place — unless something stands there that must not be replaced.

use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::SystemTime;

use konedrive_fs::placeholder::{self, State, XATTR_ITEM_ID, XATTR_STATE};

use super::Under;

/// What stands at the place a download is to land at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::fuse) enum Stands {
    Nothing,
    /// The whole copy of the item that was downloaded: a program made it or saved over it
    /// meanwhile.
    ItsCopy,
    /// The whole copy of another item.
    AnothersCopy,
    /// A file that carries an item's id and is not whole: it holds nothing.
    NotWhole,
    /// Anything that carries no item's id: a file or directory of the user's own.
    TheUsers,
}

/// What is done with a download, for what stands at its place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::fuse) enum Step {
    /// The download gets the name.
    Link,
    /// What stands there goes first.
    Replace,
    /// What stands there is the item's content already: the download is dropped.
    Keep,
    Refuse,
}

/// What stands at the item's place by now is never replaced by what was downloaded: a file of
/// the user's own; the item's copy, which a program made or saved over it meanwhile; the
/// whole copy of another item. Only what holds nothing — a file that is not whole — goes.
pub(in crate::fuse) fn step(stands: Stands) -> Step {
    match stands {
        Stands::Nothing => Step::Link,
        Stands::ItsCopy => Step::Keep,
        Stands::NotWhole => Step::Replace,
        Stands::AnothersCopy | Stands::TheUsers => Step::Refuse,
    }
}

/// How a download landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::fuse) enum Landed {
    /// It is the file at the item's place.
    Placed,
    /// The item's whole copy stood there already, and stays.
    Kept,
    /// Something that is not to be replaced stands there, and stays.
    Refused,
}

impl Under {
    /// A file of the folder's filesystem that is in no directory, to build a download in.
    pub(in crate::fuse) fn unnamed(&self) -> io::Result<File> {
        File::options().read(true).write(true).custom_flags(libc::O_TMPFILE).mode(0o644).open(self.path(Path::new("")))
    }

    /// What stands at `rel`, for a download of item `id`.
    pub(in crate::fuse) fn stands(&self, rel: &Path, id: &str) -> Stands {
        let path = self.path(rel);
        if fs::symlink_metadata(&path).is_err() {
            return Stands::Nothing;
        }
        let whole = xattr::get(&path, XATTR_STATE).ok().flatten().is_some_and(|state| state == State::Hydrated.as_str().as_bytes());
        match xattr::get(&path, XATTR_ITEM_ID).ok().flatten() {
            Some(carried) if carried == id.as_bytes() && whole => Stands::ItsCopy,
            Some(_) if !whole => Stands::NotWhole,
            Some(_) => Stands::AnothersCopy,
            None => Stands::TheUsers,
        }
    }

    /// Lands `file`, the whole and checked content of item `id` built in a file without a
    /// name, at `rel`: stamped as the item's copy, last changed at `modified`, and given the
    /// name unless what stands there says otherwise ([`step`]).
    pub(in crate::fuse) fn land(&self, file: &File, rel: &Path, id: &str, modified: SystemTime) -> io::Result<Landed> {
        placeholder::write_item_id(file, id)?;
        placeholder::write_state(file, State::Hydrated)?;
        file.set_modified(modified)?;
        // No write bit on a downloaded copy, as the mount shows it.
        file.set_permissions(fs::Permissions::from_mode(0o444))?;
        file.sync_all()?;
        let target = self.path(rel);
        match step(self.stands(rel, id)) {
            Step::Keep => return Ok(Landed::Kept),
            Step::Refuse => return Ok(Landed::Refused),
            Step::Replace => fs::remove_file(&target)?,
            Step::Link => {}
        }
        // Whole and checked: now it gets its name.
        let (unnamed, named) = (CString::new(konedrive_fs::proc_path(file).into_os_string().into_encoded_bytes())?, CString::new(target.into_os_string().into_encoded_bytes())?);
        // SAFETY: two paths this function owns.
        match unsafe { libc::linkat(libc::AT_FDCWD, unnamed.as_ptr(), libc::AT_FDCWD, named.as_ptr(), libc::AT_SYMLINK_FOLLOW) } {
            0 => Ok(Landed::Placed),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

#[cfg(test)]
mod tests;
