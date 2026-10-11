//! What a step made in OneDrive that its commit did not write.
//!
//! **State owned:** none.
//!
//! **Answers for:** stage C's notes, "when a commit answers `RowsChanged`", and design
//! §6's cell "made and removed here before it was sent: if its sending is running, the
//! step finds the row gone at its commit and removes in OneDrive what it made". After a
//! restart nothing of this is done (the owner's decision, the map's F2): a file uploaded
//! and then removed here, the daemon killed before the confirmation, comes back from
//! OneDrive with the listing as any item of OneDrive's and is shown again; no layer asks
//! whose it was.
//!
//! **Tested without the rest**, against the fixture's fake OneDrive:
// test: a_new_item_whose_row_is_gone_is_removed_in_onedrive
// test: a_new_item_whose_row_is_still_there_asks_for_a_cycle
// test: a_made_item_that_cannot_be_removed_is_left_and_said
#![allow(dead_code, unused_variables)]

use konedrive_tree::{ConfirmedItem, Item, ItemVersions};

use super::GraphAccess;
use crate::sync_parts::step_outcome::RowsChangedFollowUp;

/// What is done about an item a step made in OneDrive whose commit wrote nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MadeItemFate {
    /// The item's row is gone: what was made is removed in OneDrive.
    Remove,
    /// The row is there, changed: a listing brings the made item as one of OneDrive's,
    /// and the plan compares the two (two new items of one name).
    LeaveForListing,
}

/// The fate of what `MakeDirectory` or `SendNewFile` made, from the item's row as it is
/// now. Pure.
pub fn decide_made_item_fate(row: Option<&Item>) -> MadeItemFate {
    todo!()
}

/// What follows up on made items.
#[derive(Clone)]
pub struct MadeItems {
    graph: GraphAccess,
}

impl MadeItems {
    pub fn new(graph: GraphAccess) -> Self {
        Self { graph }
    }

    /// After `RowsChanged` on `MakeDirectory` or `SendNewFile`: reads the row,
    /// [`decide_made_item_fate`], and removes what was made (`delete_item`,
    /// `G/write.rs:140`, guarded by the eTag of the answer; `delete_folder`, `:153`, for
    /// a directory: unguarded, and it takes only what this step made — a directory is
    /// adopted as the step's own only when it is empty, `lost_answers`) or asks for a
    /// cycle. Called with NO lock held: the tree lock of the
    /// commit was let go. Answers `MadeItemRemoved` for an item removed or found gone,
    /// `MadeItemLeft` for a removal OneDrive did not carry out (a 412: another changed it
    /// since; any failure), `CycleWanted` for a row that is still there. Today:
    /// `landed_away` (`upload/steps/shared.rs:663`) and `never_uploaded` (`:632`).
    pub async fn follow_up_new_item(&self, item: &ItemVersions, made: &ConfirmedItem) -> RowsChangedFollowUp {
        todo!()
    }
}
