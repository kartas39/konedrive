//! What of the folder is uploaded to OneDrive at all.
//!
//! [`UploadRules`] owns the account's ignore list, as its settings share it. It touches
//! nothing: not the disk, not the store, not the mount. The one fact of the disk its answers
//! need — whether a directory is marked with a OneDrive id — is the caller's to tell. The
//! names OneDrive itself refuses are not here: they are a reason on a row
//! ([`crate::local::names::refused`]), said where the row is decided
//! ([`super::detections`]).
//!
//! What it answers:
//!
//! - whether what is not in OneDrive is ignored, and so never uploaded: by its own name
//!   ([`UploadRules::is_ignored`], or as an [`UploadDecision`]
//!   [`UploadRules::decide_upload`]), or by a directory it is in
//!   ([`UploadRules::is_in_ignored_directory`]);
//! - which names a program may not make: the daemon's own ([`UploadRules::is_daemon_owned`])
//!   and a file manager's trash ([`UploadRules::is_trash`]).

use std::path::Path;

use crate::folder::disk::daemon_owned;
use crate::local::SharedIgnore;

/// What the upload rules decide of what is not in OneDrive at a place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UploadDecision {
    /// It is uploaded from there.
    Upload,
    /// It is ignored: its name, or a directory it is in, is on the ignore list.
    Ignore,
}

pub(super) struct UploadRules {
    /// The account's ignore list: what is never uploaded.
    ignore: SharedIgnore,
}

impl UploadRules {
    pub(super) fn new(ignore: SharedIgnore) -> Self {
        UploadRules { ignore }
    }

    /// Whether what is in the directory `directory` and is not in OneDrive is ignored
    /// whatever its name: a directory it is in, which is not in OneDrive either, is on the
    /// ignore list. `in_onedrive` says whether the directory at a path is marked with a
    /// OneDrive id: one that is, is OneDrive's, whatever its name.
    pub(super) fn is_in_ignored_directory(&self, directory: &Path, in_onedrive: impl Fn(&Path) -> bool) -> bool {
        let ignore = crate::panic::read(&self.ignore);
        let mut at = directory;
        while let (Some(name), Some(parent)) = (at.file_name(), at.parent()) {
            if ignore.matches(name) && !in_onedrive(at) {
                return true;
            }
            at = parent;
        }
        false
    }

    /// Whether the name `name` itself is on the ignore list, wherever it is: what a row of
    /// `items` keeps of an item that is not in OneDrive (`is_name_ignored`).
    pub(super) fn is_name_ignored(&self, name: &str) -> bool {
        crate::panic::read(&self.ignore).matches(name.as_ref())
    }

    /// Whether what is `name` in the directory `directory` and is not in OneDrive is ignored:
    /// its name is on the ignore list, or it is in a directory that is
    /// ([`Self::is_in_ignored_directory`]).
    pub(super) fn is_ignored(&self, directory: &Path, name: &str, in_onedrive: impl Fn(&Path) -> bool) -> bool {
        let ignored = crate::panic::read(&self.ignore).matches(name.as_ref());
        ignored || self.is_in_ignored_directory(directory, in_onedrive)
    }

    /// Whether what is `name` in the directory `directory` and is not in OneDrive is uploaded from
    /// there, or ignored ([`Self::is_ignored`]).
    pub(super) fn decide_upload(&self, directory: &Path, name: &str, in_onedrive: impl Fn(&Path) -> bool) -> UploadDecision {
        match self.is_ignored(directory, name, in_onedrive) {
            true => UploadDecision::Ignore,
            false => UploadDecision::Upload,
        }
    }

    /// Whether `name` is one the daemon uses for itself in the folder: no program makes it
    /// or renames anything to it.
    pub(super) fn is_daemon_owned(&self, name: &str) -> bool {
        daemon_owned(name.as_ref())
    }

    /// Whether `name`, in the folder itself, is a file manager's trash directory. None is
    /// made: what is removed here goes to OneDrive's recycle bin, and a trash inside the
    /// folder would be uploaded.
    pub(super) fn is_trash(&self, name: &str) -> bool {
        name.starts_with(".Trash")
    }
}

#[cfg(test)]
pub(super) mod tests;
