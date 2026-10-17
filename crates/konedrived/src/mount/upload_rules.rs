//! What of the folder is uploaded to OneDrive at all.
//!
//! [`UploadRules`] owns the account's ignore list, as its settings share it. It touches
//! nothing: not the disk, not the store, not the mount. The names OneDrive itself refuses are
//! not here: they are a reason on a row ([`crate::local::names::refused`]), said where the
//! row is decided.
//!
//! What it answers:
//!
//! - whether a name is on the ignore list ([`UploadRules::is_name_ignored`]): what a row keeps
//!   of its item's name; whether a directory above holds what is below back is the rows' to
//!   say;
//! - whether a name is a file manager's trash, which a program may not make
//!   ([`UploadRules::is_trash`]).

use std::ffi::OsStr;

use crate::local::SharedIgnore;

pub(crate) struct UploadRules {
    /// The account's ignore list: what is never uploaded.
    ignore: SharedIgnore,
}

impl UploadRules {
    pub(super) fn new(ignore: SharedIgnore) -> Self {
        UploadRules { ignore }
    }

    /// Whether the name itself is on the ignore list, whatever directory it is in: what a
    /// row of `items` keeps of its item's name (`is_name_ignored`). The directories above
    /// are the rows' to ask. A name that is not UTF-8 is matched as the list matches it.
    pub(crate) fn is_name_ignored(&self, name: &OsStr) -> bool {
        crate::panic::read(&self.ignore).matches(name)
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
