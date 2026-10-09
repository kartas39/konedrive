//! A finished download given its path in the backing directory.
//!
//! A download is built in a file that has no name: it is in the backing directory's
//! filesystem and in no directory, so nothing ever lists a file that is not whole, and one the
//! daemon leaves behind is removed by the kernel. Once it is whole and checked it lands: it is
//! marked as the downloaded copy of its OneDrive file and linked in at that file's path —
//! unless something is there that must not be replaced.

use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::SystemTime;

use konedrive_fs::placeholder::{self, State};

use super::downloaded_copies::DiskMarks;
use super::BackingDirectory;

/// What is at the path a download is to land at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::fuse) enum TargetContents {
    Nothing,
    /// The whole copy of the OneDrive file that was downloaded: a program made it or saved
    /// over it meanwhile.
    CopyOfSameFile,
    /// The whole copy of another OneDrive file.
    CopyOfOtherFile,
    /// A file that is marked with a OneDrive id and is not whole: it holds nothing.
    PartialFile,
    /// Anything that is marked with no OneDrive id: a file or directory of the user's own.
    UsersOwnFile,
}

/// What is done with a download, for what is at its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::fuse) enum LandingAction {
    /// The download gets the name.
    Link,
    /// What is there is removed first.
    Replace,
    /// What is there is the file's content already: the download is dropped.
    Keep,
    Refuse,
}

/// What is at the path by now is never replaced by what was downloaded: a file of the user's
/// own; the copy of the same OneDrive file, which a program made or saved over it meanwhile;
/// the whole copy of another OneDrive file. Only what holds nothing — a file that is not
/// whole — is replaced.
pub(in crate::fuse) fn choose_landing_action(target: TargetContents) -> LandingAction {
    match target {
        TargetContents::Nothing => LandingAction::Link,
        TargetContents::CopyOfSameFile => LandingAction::Keep,
        TargetContents::PartialFile => LandingAction::Replace,
        TargetContents::CopyOfOtherFile | TargetContents::UsersOwnFile => LandingAction::Refuse,
    }
}

/// How a download landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::fuse) enum LandingOutcome {
    /// It is the file at the path.
    Placed,
    /// The whole copy of the same OneDrive file was there already, and stays.
    Kept,
    /// Something that is not to be replaced is there, and stays.
    Refused,
}

impl BackingDirectory {
    /// A file of the backing directory's filesystem that is in no directory, to build a
    /// download in.
    pub(in crate::fuse) fn make_unnamed_file(&self) -> io::Result<File> {
        File::options().read(true).write(true).custom_flags(libc::O_TMPFILE).mode(0o644).open(self.resolve_path(Path::new("")))
    }

    /// What is at `rel`, for a download of the OneDrive file `id`.
    pub(in crate::fuse) fn inspect_target(&self, rel: &Path, id: &str) -> TargetContents {
        let path = self.resolve_path(rel);
        if fs::symlink_metadata(&path).is_err() {
            return TargetContents::Nothing;
        }
        let marks = DiskMarks::read(&path);
        if marks.is_downloaded_copy_of(id) {
            TargetContents::CopyOfSameFile
        } else if marks.is_partial_file() {
            TargetContents::PartialFile
        } else if marks.has_onedrive_id() {
            TargetContents::CopyOfOtherFile
        } else {
            TargetContents::UsersOwnFile
        }
    }

    /// Lands `file`, the whole and checked content of the OneDrive file `id` built in a file
    /// without a name, at `rel`: marked as that file's downloaded copy, last modified at
    /// `modified`, and given the name unless what is there says otherwise
    /// ([`choose_landing_action`]).
    pub(in crate::fuse) fn land(&self, file: &File, rel: &Path, id: &str, modified: SystemTime) -> io::Result<LandingOutcome> {
        // What is there is looked at first: a download that is dropped or refused is not
        // marked and not written out to the disk.
        let action = choose_landing_action(self.inspect_target(rel, id));
        match action {
            LandingAction::Keep => return Ok(LandingOutcome::Kept),
            LandingAction::Refuse => return Ok(LandingOutcome::Refused),
            LandingAction::Replace | LandingAction::Link => {}
        }
        placeholder::write_item_id(file, id)?;
        placeholder::write_state(file, State::Hydrated)?;
        file.set_modified(modified)?;
        // No write bit on a downloaded copy, as the mount shows it.
        file.set_permissions(fs::Permissions::from_mode(0o444))?;
        file.sync_all()?;
        let target = self.resolve_path(rel);
        if action == LandingAction::Replace {
            fs::remove_file(&target)?;
        }
        // Whole and checked: now it gets its name.
        let (unnamed, named) = (CString::new(konedrive_fs::proc_path(file).into_os_string().into_encoded_bytes())?, CString::new(target.into_os_string().into_encoded_bytes())?);
        // SAFETY: two paths this function owns.
        match unsafe { libc::linkat(libc::AT_FDCWD, unnamed.as_ptr(), libc::AT_FDCWD, named.as_ptr(), libc::AT_SYMLINK_FOLLOW) } {
            0 => Ok(LandingOutcome::Placed),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

#[cfg(test)]
mod tests;
