//! The value of an extended attribute the mount shows of a file or directory of OneDrive: a
//! decision of the entry, its pin and whether it is being downloaded, with no disk in it.

use konedrive_fs::placeholder::{XATTR_ITEM_ID, XATTR_PIN, XATTR_STATE};

use crate::fuse::directory_contents::{DirectoryEntry, EntryState};
use crate::fuse::state_attribute::to_state_attribute;

/// The value of the attribute `name` of `entry`, which carries a pin of its own when
/// `is_pinned` and is being downloaded whole when `is_downloading`. `None` for an attribute
/// the entry does not have:
///
/// - its OneDrive id: what is in OneDrive has it, a local-only entry has none;
/// - its state: a file of OneDrive has it — online-only, being downloaded or downloaded — a
///   directory has none;
/// - its pin: `1` on what carries a pin of its own, and absent on everything else.
///
/// Any other name is none of the store's: the caller reads it from the backing directory,
/// or has it not.
pub(super) fn to_xattr_value(name: &str, entry: &DirectoryEntry, is_pinned: bool, is_downloading: bool) -> Option<String> {
    let row = entry.row()?;
    match (name, &entry.state) {
        (XATTR_ITEM_ID, _) => Some(row.id.clone()),
        (XATTR_STATE, EntryState::File(file)) => Some(to_state_attribute(file, is_downloading)?.as_str().to_owned()),
        (XATTR_PIN, _) if is_pinned => Some("1".to_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
