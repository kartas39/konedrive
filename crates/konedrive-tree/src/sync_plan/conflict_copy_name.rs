//! The name of a conflict copy: the name the user's own item is renamed to here when another
//! item has, or takes, the name it had. The scheme is that of the daemon's `copy_name`
//! (`local/names.rs`), which goes at the switch; here the name is also chosen free.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use crate::folded_name::FoldedName;

/// How many names are tried for one conflict copy before the plan says none is free.
pub(crate) const CONFLICT_COPY_NAME_TRIES: u32 = 100;

/// The `attempt`th name of a conflict copy of `name`, counted from 1: this computer's name
/// added before the extension (`Report.docx` → `Report-pc.docx`, `archive.tar.gz` →
/// `archive.tar-pc.gz`, `.bashrc` → `.bashrc-pc`), and from the second attempt its number
/// after it (`Report-pc-2.docx`). What stands before the extension is cut, between two
/// letters, so that the name is that of a directory entry.
///
/// The caller of the plan asks it too: the rows of what has such a name in the directory of
/// an item, on either side, are given with the item ([`SyncPlanInput`](super::SyncPlanInput)).
pub fn make_conflict_copy_name(name: &OsStr, computer_name: &str, attempt: u32) -> OsString {
    let name = name.as_bytes();
    let added = if attempt <= 1 { format!("-{computer_name}") } else { format!("-{computer_name}-{attempt}") };
    let (stem, extension) = match name.iter().rposition(|byte| *byte == b'.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, &[][..]),
    };
    let mut kept = stem.len().min(konedrive_fs::NAME_MAX.saturating_sub(added.len() + extension.len()));
    // Not inside a letter of UTF-8: its later bytes begin with the bits 10.
    while kept > 0 && kept < stem.len() && stem[kept] & 0b1100_0000 == 0b1000_0000 {
        kept -= 1;
    }
    OsString::from_vec([&stem[..kept], added.as_bytes(), extension].concat())
}

/// The first conflict copy's name of `name` that `is_taken` does not say is taken, as
/// OneDrive compares names; `None` when [`CONFLICT_COPY_NAME_TRIES`] names are all taken.
pub(crate) fn find_free_conflict_copy_name(name: &OsStr, computer_name: &str, is_taken: impl Fn(&FoldedName) -> bool) -> Option<OsString> {
    (1..=CONFLICT_COPY_NAME_TRIES).map(|attempt| make_conflict_copy_name(name, computer_name, attempt)).find(|candidate| !is_taken(&FoldedName::of(candidate)))
}

#[cfg(test)]
mod tests;
