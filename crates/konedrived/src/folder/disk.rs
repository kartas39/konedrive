//! The folder on disk as the daemon marks it: the names it uses for itself, and the one
//! lock on the modes of a folder's directories.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

/// The holding directory of an older konedrive: a folder that holds one is not mounted,
/// and the mount hides the name (`docs/design/mount.md` §3.1).
pub(crate) const HOLDING: &str = ".konedrive-holding";
/// A new folder's name until it is labelled and marked.
pub const NEW_PREFIX: &str = ".konedrive-new-";

/// Whether `name` is one the daemon uses for itself in the folder: the holding
/// directory, or a new folder before it has its name. Nothing under such a name is the
/// user's: the mount shows none of it.
pub(crate) fn daemon_owned(name: &OsStr) -> bool {
    name == OsStr::new(HOLDING) || name.as_bytes().starts_with(NEW_PREFIX.as_bytes())
}

/// One folder's lock on the modes of its directories, held by everything in this daemon
/// that lifts a locked directory's write bit and puts it back: the drive written on the root. Two such windows on one
/// directory used to be able to interleave — one put the lock back while the other was
/// still writing, which failed `EACCES`.
///
/// A folder has one, whoever asks ([`Modes::of`]), and another account's folder has its
/// own. It is not re-entrant. Never held across an `.await`.
pub(crate) struct Modes(Mutex<()>);

/// A hold on a folder's [`Modes`], released when dropped.
pub(crate) struct ModesHeld<'a>(#[allow(dead_code)] MutexGuard<'a, ()>);

/// The folders whose [`Modes`] somebody holds a share of, by the root directory's device
/// and inode. An entry goes when nobody does.
static FOLDERS: Mutex<Vec<(RootKey, Weak<Modes>)>> = Mutex::new(Vec::new());

/// A root directory: its device and inode.
type RootKey = (u64, u64);

impl Modes {
    /// The lock of the folder whose root directory `root` is open on.
    pub(crate) fn of(root: &File) -> io::Result<Arc<Modes>> {
        let meta = root.metadata()?;
        let key = (meta.dev(), meta.ino());
        let mut folders = crate::panic::lock(&FOLDERS);
        folders.retain(|(_, modes)| modes.strong_count() > 0);
        if let Some(modes) = folders.iter().find(|(k, _)| *k == key).and_then(|(_, modes)| modes.upgrade()) {
            return Ok(modes);
        }
        let modes = Arc::new(Modes(Mutex::new(())));
        folders.push((key, Arc::downgrade(&modes)));
        Ok(modes)
    }

    pub(crate) fn hold(&self) -> ModesHeld<'_> {
        // The lock guards no data a panic could leave half-written.
        ModesHeld(crate::panic::lock(&self.0))
    }
}

#[cfg(test)]
mod tests;

// ---- The names of the mount over the rows. ----
//
// Two rules, with two questions, because the two kinds of name need OPPOSITE answers from
// the check of the backing directory at start. Tested without the rest:
// test: an_entry_set_aside_is_an_entry_no_row_says_and_a_hidden_name
// test: a_temporary_name_is_a_hidden_name_and_not_an_entry_no_row_says
// test: the_holding_directory_and_a_new_folder_are_both
// test: the_set_aside_name_of_an_item_is_the_same_at_every_start_and_another_items_is_another

/// How the name begins that an entry of the backing directory is set aside to, in the root,
/// before it is removed for good (`mount::place_changes`, `PlaceChange::set_aside`): an
/// entry NO row says. After it comes the local item id of the item the entry was of.
pub(crate) const ASIDE_ENTRY_PREFIX: &str = ".konedrive-aside-";

/// How the temporary name of an exchange of names begins
/// (`konedrive_tree::make_temporary_name`): the place of a ROW, with its file or directory
/// on disk under that name.
pub(crate) const TEMPORARY_NAME_PREFIX: &str = ".konedrive-swap-";

/// The name the entry of `item` is set aside to: made of the local item id, which is never
/// given twice, so that it is the same name at every start and no other item's. One item
/// has one entry set aside at most (one step per item, under the tree lock); a leftover
/// of a death is put back by the check at start from the record of the change, and what
/// is still set aside after it removed (`PlaceChanges::remove_aside_entries`).
pub(crate) fn make_aside_entry_name(item: konedrive_tree::LocalItemId) -> std::ffi::OsString {
    format!("{ASIDE_ENTRY_PREFIX}{item}").into()
}

/// Whether `name`, in the root of the backing directory, is an entry no row says: a name
/// of the daemon's own ([`daemon_owned`]), or an entry set aside. What the check of the
/// backing directory at start does not read as something of the folder, and what
/// `PlaceChanges::remove_aside_entries` cleans of the last kind once the check has put
/// back what a death left. A temporary name is NOT among them: it is a row's place, and
/// skipping it would make the check find the row's file gone.
pub(crate) fn is_rowless_entry_name(name: &OsStr) -> bool {
    daemon_owned(name) || name.as_bytes().starts_with(ASIDE_ENTRY_PREFIX.as_bytes())
}

/// Whether the mount hides `name` and refuses it to programs (lookup `ENOENT`, make and
/// rename `EPERM`): every entry no row says ([`is_rowless_entry_name`]), and a temporary
/// name of an exchange. Asked by the mount's lookup, directory readings, make and rename,
/// and by the queue's answers, which show no such row.
pub(crate) fn is_hidden_name(name: &OsStr) -> bool {
    is_rowless_entry_name(name) || name.as_bytes().starts_with(TEMPORARY_NAME_PREFIX.as_bytes())
}
