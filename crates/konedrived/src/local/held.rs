//! What the user's changes hold of the folder while they wait in the outbox
//! (`docs/design/writes.md` §9): the items a cycle of the listing leaves as they are, and
//! the ones being removed.
//!
//! The folder also holds the user's own changes, which the outbox sends. So a cycle in
//! read-write mode:
//!
//! - **keeps its hands off what a local change holds** ([`Rw::held`]): an item with a live
//!   outbox row, in any state, and what the base has below a folder such a row moves — not
//!   moved or replaced. Their changes wait: the base keeps the version the folder holds;
//! - **changes nothing below a folder being removed** ([`Rw::removing`]): below a live
//!   `delete` row the delta's changes go to the base, and nothing goes to the folder.

use std::collections::HashSet;

use konedrive_tree::{Kind, Table, TreeError, TreeStore};

/// What a read-write folder's cycle needs to know besides the tree.
#[derive(Debug, Default, Clone)]
pub struct Rw {
    /// Items a live outbox row concerns, and what the base or the new tree
    /// has below a folder such a row moves: hands off, their changes wait.
    pub held: HashSet<String>,
    /// Items a live `delete` row removes, and everything below them: nothing
    /// is changed there, and their changes go to the base.
    pub removing: HashSet<String>,
}

impl Rw {
    /// Read from the store once `staging` holds the new tree: the outbox's
    /// live rows.
    pub fn read(s: &TreeStore) -> Result<Self, TreeError> {
        let mut rw = Rw::default();
        for row in s.outbox_rows()? {
            let Some(id) = row.item_id.clone() else { continue };
            let folder = s.get(Table::Items, &id)?.is_some_and(|r| r.kind == Kind::Folder);
            if folder {
                // What the base has below the folder goes with the row; what
                // OneDrive moved in from elsewhere since is not the folder's:
                // the base still has it where it was, and its move waits.
                rw.extend_below(s, &id, row.kind.removes())?;
            }
            if row.kind.removes() {
                rw.removing.insert(id);
            } else {
                rw.held.insert(id);
            }
        }
        Ok(rw)
    }

    /// Everything below folder `id`, which a live row takes away (`removing`)
    /// or moves: the base's descendants go with the row; one the new tree
    /// moves in from elsewhere in the base is held where the base has it, its
    /// move waiting; one new to the base goes with the row too.
    fn extend_below(&mut self, s: &TreeStore, id: &str, removing: bool) -> Result<(), TreeError> {
        let base: HashSet<String> = s.descendants(Table::Items, id)?.into_iter().collect();
        for staged in s.descendants(Table::Staging, id)? {
            if base.contains(&staged) {
                continue;
            }
            if s.get(Table::Items, &staged)?.is_some() {
                self.held.insert(staged);
            } else if removing {
                self.removing.insert(staged);
            } else {
                self.held.insert(staged);
            }
        }
        if removing {
            self.removing.extend(base);
        } else {
            self.held.extend(base);
        }
        Ok(())
    }
}
