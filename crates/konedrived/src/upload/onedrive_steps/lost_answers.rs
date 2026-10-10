//! A step's own earlier request that went through though its answer was lost: design
//! §7's second exception. The step adopts what it finds.
//!
//! **State owned:** none. Each function reads one item from Graph and compares it with
//! what the step sent; nothing is remembered for it but what the step memory holds anyway
//! (the temporary name, the content being sent).
//!
//! **Answers for:** a daemon that died, or was stopped, between a request and its commit:
//! nothing in the rows says a request went out, so the replay rests on these
//! (the map's F9, E2). Today: `upload/steps/meta.rs:33-36`, `:100-104`,
//! `upload/content.rs:211-215`, `:374`, `:400-403`, `upload/content/session.rs:397`.
//!
//! **Tested without the rest**, against the fixture's fake OneDrive:
// test: a_directory_of_the_name_that_no_row_is_bound_to_is_the_one_this_step_made
// test: a_directory_of_the_name_that_a_row_is_bound_to_is_another_item
// test: a_file_of_the_name_with_the_hash_sent_that_no_row_is_bound_to_is_adopted
// test: an_item_already_at_the_place_sent_is_the_move_that_went_through
// test: an_item_that_holds_the_hash_sent_is_the_upload_that_went_through
// test: an_item_that_was_moved_or_changed_by_another_is_not_adopted
#![allow(dead_code, unused_variables)]

use std::ffi::OsStr;

use konedrive_tree::{ConfirmedItem, ItemVersions, OneDriveItemReference};

use super::GraphAccess;
use crate::folder::step_outcome::StepOutcome;

/// What reads OneDrive for a step's own earlier request.
#[derive(Clone)]
pub struct LostAnswers {
    graph: GraphAccess,
}

impl LostAnswers {
    pub fn new(graph: GraphAccess) -> Self {
        todo!()
    }

    /// After a 409 on `MakeDirectory`: the directory OneDrive has under `name` in
    /// `parent`, when it is this step's own — a directory no row is bound to
    /// (`ItemReader::find_bound_item`). Reads `child` (`G/mod.rs:228`). `None`: the
    /// holder is another item.
    pub async fn find_made_directory(&self, parent: &OneDriveItemReference, name: &OsStr) -> Result<Option<ConfirmedItem>, StepOutcome> {
        todo!()
    }

    /// After a 412 on `SendMove`: the item as OneDrive has it now, when it is already in
    /// `parent` under `name` with the content the step was planned from. Reads `item`
    /// (`G/mod.rs:217`). `None`: another change, which a listing brings.
    pub async fn find_sent_move(&self, item: &ItemVersions, parent: &OneDriveItemReference, name: &OsStr) -> Result<Option<ConfirmedItem>, StepOutcome> {
        todo!()
    }

    /// After a 412 on `SendContent`, and after an upload session was found gone: the item
    /// as OneDrive has it now, when it holds the hash of the content sent. Reads `item`,
    /// or for a new file `child` of `new_file_place`, and compares with `sent_hash`.
    /// `None`: other content.
    pub async fn find_sent_content(&self, item: &ItemVersions, new_file_place: Option<(&OneDriveItemReference, &OsStr)>, sent_hash: &str) -> Result<Option<ConfirmedItem>, StepOutcome> {
        todo!()
    }
}
