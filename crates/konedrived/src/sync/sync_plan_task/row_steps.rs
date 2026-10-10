//! The runner of the sync plan's steps of the rows alone: one function per case of
//! `RowStep`, each its commit and nothing else — but for the hash of a local content,
//! which reads the file, and the cases that leave the marks on disk in step with the row.
//!
//! **State owned:** none.
//!
//! **Every step:** takes the folder's tree lock for its commit and lets it go; what it
//! reads or writes on disk — a file hashed, marks written or taken off — it does with NO
//! lock: a hash of a file of any size holds up no write operation, and the marks go
//! through `ItemContentFiles`, whose opens cannot be crossed by a rename. `RowsChanged`
//! is answered as it is: nothing was written and nothing is to undo.
//!
//! **Tested without the rest**, over the fixture (`fuse::item_fixture`):
// test: each_case_calls_its_commit_and_answers_written
// test: a_step_planned_from_other_versions_answers_that_the_rows_changed
// test: the_hash_written_is_that_of_the_bytes_on_disk
// test: a_file_changed_while_it_was_hashed_writes_no_hash
// test: the_tree_lock_is_not_held_while_a_file_is_hashed
// test: an_item_that_gave_its_binding_up_has_no_onedrive_marks_on_disk
// test: a_content_agreed_on_is_marked_as_the_whole_copy_with_its_ctag
#![allow(dead_code, unused_variables)]

use std::sync::Arc;

use konedrive_tree::{ItemVersions, RowStep, Store};

use crate::folder::step_outcome::StepOutcome;
use crate::fuse::ItemContentFiles;
use crate::local::SharedIgnore;

/// The runner of the steps of the rows, for one folder.
pub struct RowSteps {
    store: Store,
    tree_lock: Arc<tokio::sync::Mutex<()>>,
    content_files: Arc<ItemContentFiles>,
    /// The account's list of ignored names: `GiveUpBinding`'s commit asks whether the
    /// item's name is ignored, now that nothing of OneDrive's keeps it from being left
    /// here.
    ignore: SharedIgnore,
}

impl RowSteps {
    /// Made by the folder's start (`sync_tasks::start_order::start_sync_tasks`).
    pub fn new(store: Store, tree_lock: Arc<tokio::sync::Mutex<()>>, content_files: Arc<ItemContentFiles>, ignore: SharedIgnore) -> Self {
        todo!()
    }

    /// Runs `step` and answers what came of it. Called by the plan's task, at once and
    /// in the plan's order: a step of the rows waits for no slot.
    pub async fn run(&self, step: RowStep) -> StepOutcome {
        todo!()
    }

    /// `GiveUpBinding`: `commit_give_up_binding(item, is_name_ignored)` under the tree
    /// lock; then the OneDrive marks taken off what the item has on disk
    /// (`ItemContentFiles::remove_onedrive_marks`). Replaces `outbox_orphan` and
    /// `upload_as_new` (`upload/steps/shared.rs:591`). The upload session the item had
    /// is abandoned by the store, with the binding.
    async fn run_give_up_binding(&self, item: &ItemVersions) -> StepOutcome {
        todo!()
    }

    /// `MergeItems`: `commit_merge_items(local, onedrive)`; then what the item has on disk
    /// marked as the whole copy of what the two sides agree on
    /// (`ItemContentFiles::mark_agreed_content`). Design §5.
    async fn run_merge_items(&self, local: &ItemVersions, onedrive: &ItemVersions) -> StepOutcome {
        todo!()
    }

    /// `AgreeOnRemoval`: `commit_agree_on_removal(item)`. Both sides removed the item.
    async fn run_agree_on_removal(&self, item: &ItemVersions) -> StepOutcome {
        todo!()
    }

    /// `ForgetSyncedVersion`: `commit_forget_synced_version(item)`.
    async fn run_forget_synced_version(&self, item: &ItemVersions) -> StepOutcome {
        todo!()
    }

    /// `HashLocalContent`: with no lock, opens the content the step was planned from
    /// (`ItemContentFiles::open_to_send`) and hashes it off the runtime
    /// (`LocalContentFile::hash`); then, under the tree lock,
    /// `commit_hash_local_content(item, hash)`: the plan made after it can ask whether the
    /// two sides have the same content (design §6). The commit writes the content as
    /// hashed; the check at start tells a clean copy by the local content against the
    /// synced one, not by that.
    async fn run_hash_local_content(&self, item: &ItemVersions) -> StepOutcome {
        todo!()
    }

    /// `AgreeOnPlace`: `commit_agree_on_place(item)`.
    async fn run_agree_on_place(&self, item: &ItemVersions) -> StepOutcome {
        todo!()
    }

    /// `AgreeOnContent`: `commit_agree_on_content(item)`; then the copy on disk marked as
    /// the whole copy with the cTag it agrees with
    /// (`ItemContentFiles::mark_agreed_content`): an id-only mark at a row that became
    /// equal to the synced content would be read as "holds nothing" by the next check.
    async fn run_agree_on_content(&self, item: &ItemVersions) -> StepOutcome {
        todo!()
    }
}
