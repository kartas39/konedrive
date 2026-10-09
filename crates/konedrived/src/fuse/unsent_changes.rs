//! What was moved, removed or changed here and is not sent yet, as the outbox's rows say it:
//! where a file or directory the user moved is until its move is sent, which the user
//! removed, and which files hold content of the user's own.
//!
//! The store has every file and directory where OneDrive has it; a change made in the folder
//! is a row of the outbox until it is sent. The mount shows the folder as the user left it:
//! the store, with these changes applied.
//!
//! [`UnsentChanges`] is a function of the store's rows and of nothing else: no disk, no node
//! id, no mount. Who reads it, and when it is read again, is for
//! [`DirectoryContents`](super::directory_contents::DirectoryContents) to say.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use konedrive_tree::outbox::OutboxKind;
use konedrive_tree::TreeStore;

use super::directory_reference::DirectoryReference;

/// The unsent changes, as of one reading of the store. Every key is a OneDrive id.
#[derive(Default)]
pub(super) struct UnsentChanges {
    /// Moved here and not moved in OneDrive yet: the directory each is in now, and its name
    /// there. The directory is referred to by its OneDrive id, or by its path alone when
    /// OneDrive does not have it yet.
    pub(super) moved_ids: HashMap<String, (DirectoryReference, String)>,
    /// Removed here and not removed from OneDrive yet.
    pub(super) removed_ids: HashSet<String>,
    /// Files whose content was changed here and is not sent yet.
    pub(super) modified_ids: HashSet<String>,
}

impl UnsentChanges {
    /// What the outbox's rows of `store` say now.
    pub(super) fn of(store: &TreeStore) -> Self {
        let mut now = UnsentChanges::default();
        for row in store.outbox_rows().unwrap_or_default() {
            let Some(id) = row.item_id.clone() else { continue };
            if row.kind.removes() {
                now.removed_ids.insert(id);
                continue;
            }
            if row.kind == OutboxKind::Update {
                now.modified_ids.insert(id.clone());
            }
            // Moved: the row takes it elsewhere than the base has it. It is where the row
            // last saw it, under the name it has there.
            let base = row.base.as_ref().map(|b| (b.parent.as_deref(), b.name.as_deref()));
            let name = row.rel.file_name().and_then(|n| n.to_str());
            if let (Some(base), Some(name)) = (base, name) {
                if base != (row.target_parent.as_deref(), row.target_name.as_deref()) {
                    let directory = DirectoryReference { onedrive_id: row.target_parent.clone(), path: row.rel.parent().map(PathBuf::from).unwrap_or_default() };
                    now.moved_ids.insert(id, (directory, name.to_owned()));
                }
            }
        }
        now
    }
}

#[cfg(test)]
mod tests;
