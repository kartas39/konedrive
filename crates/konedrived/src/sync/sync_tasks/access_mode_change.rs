//! The switch of a folder between read-only and read-write, with the stamp of the switch
//! and "what is unsent is sent first".
//!
//! **State owned:** none: the access mode is the mount's switch, the stamp the store's
//! (`item_store::sending_switch`).
//!
//! **Answers for:** design §13 (3): the folder refuses every change at once, and what was
//! unsent is sent first; nothing is dropped and the switch is never refused — `force` of
//! `Account.SetMode` stays in the XML and changes nothing. The sync is not stopped and
//! started for it (today `follow_mode` rebuilds it, `mode.rs:39-73`).
//!
//! **The order, to read-only:** the mount's switch, awaited
//! (`ItemMount::switch_to_read_only`: made on the write-operation thread, so that no
//! write operation is half done across it); then the stamp
//! (`ItemStore::switch_sending_to_read_only`): after the switch the mount writes only the
//! last closes of files that were open, and a close gives no new stamp, so the stamp read
//! then is the last one given before the switch; then the plan woken.
//! **To read-write:** the stamp forgotten, then the mount's switch, then the plan woken.
//!
//! **Tested without the rest:**
// test: a_folder_switched_to_read_only_refuses_changes_at_once_and_keeps_sending_what_was_unsent
// test: what_comes_to_be_unsent_after_the_switch_waits_for_read_write
// test: a_close_after_the_switch_gives_no_stamp_later_than_the_switchs
// test: the_switch_stops_neither_task
// test: a_folder_started_read_only_with_a_stamp_in_its_store_goes_on_sending
// test: a_forced_switch_drops_nothing
#![allow(dead_code, unused_variables)]

use konedrive_tree::{SendingState, Store};

use crate::config::Mode;
use crate::folder::sync_plan_wake::SyncPlanWake;
use crate::fuse::ItemMount;

/// Switches the folder to read-only: the order of the module's doc. Answers the state of
/// sending the plan is given from now on. The tasks go on. Called by the owner of the
/// folder's state for `Account.SetMode`, whatever `force` says; the text's count of what
/// is still to be sent is `ItemReader::count_unsent`.
pub async fn switch_to_read_only(mount: &ItemMount, store: &Store, sync_plan_wake: &SyncPlanWake) -> Result<SendingState, String> {
    todo!()
}

/// Switches the folder to read-write: the order of the module's doc.
pub async fn switch_to_read_write(mount: &ItemMount, store: &Store, sync_plan_wake: &SyncPlanWake) -> Result<SendingState, String> {
    todo!()
}

/// At a start, before the tasks: makes the mount's access mode the wanted one, and the
/// store's state of sending with it — a folder wanted read-only whose store has no stamp
/// gets one now, a folder wanted read-write whose store has one loses it.
pub async fn apply_wanted_access_mode(mount: &ItemMount, store: &Store, wanted: Mode) -> Result<SendingState, String> {
    todo!()
}

/// The account's list of ignored names changed: the rows are gone over once
/// (`ItemMount::rewrite_ignored_names`: a job of the write-operation thread, so under the
/// tree lock) and the plan woken. Called by `settings.rs`, which today applies the list
/// only to "every change made from now on". MISSING-4.f.
pub async fn apply_ignore_list_change(mount: &ItemMount, sync_plan_wake: &SyncPlanWake) -> Result<u64, String> {
    todo!()
}
