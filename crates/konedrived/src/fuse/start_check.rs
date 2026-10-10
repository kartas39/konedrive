//! The check of the backing directory at the folder's start, as the mount's build calls
//! it: before the mount goes over the folder.
//!
//! **State owned:** none. It calls `BackingDirectoryCheck::new(..).run(..)` and nothing
//! else of the check, and turns its answer into what the start and the plan's task need.
//!
//! **Answers for:** the map's 7.1 (where the check goes), 7.b (what is done with its
//! answer): design §10's second rule — sending is held back for the rows the check left
//! (`DiskFindings::list_held_items`) and what is below each, not for the folder. The
//! paths the check left, with their reasons, are said in the journal here: no member of
//! the bus shows them.
//!
//! **Tested without the rest:**
// test: a_check_that_left_nothing_holds_nothing_back
// test: the_rows_a_check_left_are_answered_as_items
// test: a_store_that_was_made_empty_has_its_marked_entries_bound_before_the_first_listing
#![allow(dead_code, unused_variables)]

use std::collections::BTreeSet;
use std::sync::Arc;

use konedrive_tree::{DriveId, LocalItemId, Store, TreeError};

use super::backing_directory::BackingDirectory;
use super::upload_rules::UploadRules;

/// What the check at start found, as the start and the plan need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartCheckFindings {
    /// The rows the check left as it found them: nothing of these, and of what the rows
    /// say is below them, is sent before a check decides them. Given to the plan as
    /// `items_left_by_start_up_check` for as long as the mount of this start is up — also
    /// when a program then writes that very item: a limit, until the next start's check
    /// decides it (to be recorded as an issue, not solved here).
    pub held_items: BTreeSet<LocalItemId>,
    /// Whether a part of the backing directory was not read: for the journal.
    pub has_unread_part: bool,
    /// How many paths the check left (each said in the journal with its reason).
    pub unresolved_path_count: usize,
    /// How many entries were bound by a mark no row had: OneDrive's version of them is not
    /// known until a full listing says it.
    pub bound_by_mark_count: usize,
}

/// Runs the check of `backing_directory` against the rows of `store`, as one job of the
/// store's thread (`Store::call_blocking`, whose closure is `'static`: so the three are
/// taken owned, and it runs on a blocking thread): the rows then say
/// what the disk has, but for the rows the answer names. `drive_id` is the folder's
/// drive, used only where no bound directory is above an entry.
///
/// Called by the mount's build (`item_mount::build_order`, after `BackingDirectory::open`
/// and the upload rules, before the mount goes over the folder): nothing else reads or
/// writes the rows or the backing directory meanwhile. The mount waits for it (design
/// §13: 0.07 to 1.4 s per 100 000 files), and hands the findings out
/// (`ItemMount::start_check_findings`).
pub(super) fn check_backing_directory_at_start(backing_directory: Arc<BackingDirectory>, upload_rules: Arc<UploadRules>, drive_id: DriveId, store: &Store) -> Result<StartCheckFindings, TreeError> {
    todo!()
}
