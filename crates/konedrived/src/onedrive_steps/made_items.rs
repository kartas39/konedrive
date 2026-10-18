//! What a step made in OneDrive that its commit did not write.
//!
//! **State owned:** none.
//!
//! **Answers for:** a commit that answers `RowsChanged` after a step made an item: made
//! and removed here before it was sent, while its sending ran, the step finds the row
//! gone at its commit and removes in OneDrive what it made (`docs/design/sync.md` §7.2,
//! `docs/design/writes.md` §4.5). After a restart nothing of this is done: a file uploaded
//! and then removed here, the daemon killed before the confirmation, comes back from
//! OneDrive with the listing as any item of OneDrive's and is shown again; no layer asks
//! whose it was.
//!
//! **Tested without the rest**, against the fixture's fake OneDrive:
// test: a_new_item_whose_row_is_gone_is_removed_in_onedrive
// test: a_new_item_whose_row_is_still_there_asks_for_a_cycle
// test: a_made_item_that_cannot_be_removed_is_left_and_said
// test: the_fate_of_a_made_item_is_by_its_row

use konedrive_graph::drive::WriteError;
use konedrive_graph::pool::Class;
use konedrive_tree::{ConfirmedItem, Item, ItemVersions};

use super::GraphAccess;
use crate::sync_parts::step_outcome::RowsChangedFollowUp;

/// What is done about an item a step made in OneDrive whose commit wrote nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MadeItemFate {
    /// The item's row is gone: what was made is removed in OneDrive.
    Remove,
    /// The row is there, changed: a listing brings the made item as one of OneDrive's,
    /// and the plan compares the two (two new items of one name).
    LeaveForListing,
}

/// The fate of what `MakeDirectory` or `SendNewFile` made, from the item's row as it is
/// now. Pure.
pub(crate) fn decide_made_item_fate(row: Option<&Item>) -> MadeItemFate {
    match row {
        None => MadeItemFate::Remove,
        Some(_) => MadeItemFate::LeaveForListing,
    }
}

/// What follows up on made items.
#[derive(Clone)]
pub(crate) struct MadeItems {
    graph: GraphAccess,
}

impl MadeItems {
    pub fn new(graph: GraphAccess) -> Self {
        Self { graph }
    }

    /// After `RowsChanged` on `MakeDirectory` or `SendNewFile`: reads the row,
    /// [`decide_made_item_fate`], and removes what was made (`delete_item`, guarded by
    /// the eTag of the answer; `delete_folder`, for
    /// a directory: unguarded, and it takes only what this step made — a directory is
    /// adopted as the step's own only when it is empty, `lost_answers`) or asks for a
    /// cycle. Called with NO lock held: the tree lock of the
    /// commit was let go. Answers `MadeItemRemoved` for an item removed or found gone,
    /// `MadeItemLeft` for a removal OneDrive did not carry out (a 412: another changed it
    /// since; any failure), `CycleWanted` for a row that is still there — and for a row
    /// that cannot be read: nothing is removed on a guess.
    pub(crate) async fn follow_up_new_item(&self, item: &ItemVersions, made: &ConfirmedItem) -> RowsChangedFollowUp {
        let id = item.local_item_id;
        let row = match self.graph.store.read_items(move |items| items.find_item(id)).await {
            Ok(row) => row,
            Err(error) => {
                tracing::warn!("cannot read the row of item {id} after its commit found the rows changed: {error}");
                return RowsChangedFollowUp::CycleWanted;
            }
        };
        if decide_made_item_fate(row.as_ref()) == MadeItemFate::LeaveForListing {
            return RowsChangedFollowUp::CycleWanted;
        }
        let made_id = made.item.item_id.as_str();
        let removed = {
            let _slot = self.graph.drive.pool().acquire(Class::Metadata).await;
            match made.content {
                Some(_) => self.graph.drive.delete_item(made_id, &made.etag).await,
                None => self.graph.drive.delete_folder(made_id).await,
            }
        };
        match removed {
            Ok(()) | Err(WriteError::NotFound) => RowsChangedFollowUp::MadeItemRemoved(made.item.clone()),
            Err(error) => {
                tracing::warn!("what item {id} made in OneDrive before its row went is not removed there: {error}");
                RowsChangedFollowUp::MadeItemLeft(made.item.clone())
            }
        }
    }
}

#[cfg(test)]
mod tests;
