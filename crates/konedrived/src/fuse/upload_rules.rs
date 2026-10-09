//! What of the folder is uploaded to OneDrive at all, and under which names.
//!
//! [`UploadRules`] owns the account's ignore list, as its settings share it, and the machine's
//! name. It touches nothing: not the disk, not the store, not the mount. The one fact of the
//! disk its answers need — whether a directory carries an item — is the caller's to tell.
//! The names OneDrive itself refuses are not here: they are a reason on a row
//! ([`crate::local::names::refused`]), said where the row is decided
//! ([`super::detections`]).
//!
//! What it answers:
//!
//! - whether what has no item stays on this computer: by its own name
//!   ([`UploadRules::stays_local`]), or by a directory above it ([`UploadRules::in_local_dir`]);
//! - which names a program may not make: the daemon's own ([`UploadRules::daemon_owned`]) and a
//!   file manager's trash ([`UploadRules::is_trash`]);
//! - the name the user's own is kept under beside an item that comes to its name
//!   ([`UploadRules::copy_name`]).

use std::path::Path;

use crate::folder::disk::daemon_owned;
use crate::local::names::copy_name;
use crate::local::SharedIgnore;

pub(super) struct UploadRules {
    /// The account's ignore list: what stays on this computer only.
    ignore: SharedIgnore,
    /// For conflict copies: `name-<machine>.ext`.
    machine: String,
}

impl UploadRules {
    pub(super) fn new(ignore: SharedIgnore, machine: String) -> Self {
        UploadRules { ignore, machine }
    }

    /// Whether what stands in the directory `dir` and has no item stays on this computer
    /// whatever its name: a directory above it that has no item is on the ignore list.
    /// `carries_item` says whether the directory at a place carries an item: one that does
    /// is OneDrive's, whatever its name.
    pub(super) fn in_local_dir(&self, dir: &Path, carries_item: impl Fn(&Path) -> bool) -> bool {
        let ignore = crate::panic::read(&self.ignore);
        let mut at = dir;
        while let (Some(name), Some(parent)) = (at.file_name(), at.parent()) {
            if ignore.matches(name) && !carries_item(at) {
                return true;
            }
            at = parent;
        }
        false
    }

    /// Whether what stands as `name` in the directory `dir` and has no item stays on this
    /// computer: its name is on the ignore list, or it is in a directory that stays
    /// ([`Self::in_local_dir`]).
    pub(super) fn stays_local(&self, dir: &Path, name: &str, carries_item: impl Fn(&Path) -> bool) -> bool {
        let ignored = crate::panic::read(&self.ignore).matches(name.as_ref());
        ignored || self.in_local_dir(dir, carries_item)
    }

    /// Whether `name` is one the daemon uses for itself in the folder: no program makes it
    /// or renames anything to it.
    pub(super) fn daemon_owned(&self, name: &str) -> bool {
        daemon_owned(name.as_ref())
    }

    /// Whether `name`, in the folder itself, is a file manager's trash directory. None is
    /// made: what is removed here goes to OneDrive's recycle bin, and a trash inside the
    /// folder would be uploaded.
    pub(super) fn is_trash(&self, name: &str) -> bool {
        name.starts_with(".Trash")
    }

    /// The name the user's own `name` is kept under beside an item that comes to it, at
    /// the `n`th try.
    pub(super) fn copy_name(&self, name: &str, n: u32) -> String {
        copy_name(name, &self.machine, n)
    }
}

#[cfg(test)]
pub(super) mod tests;
