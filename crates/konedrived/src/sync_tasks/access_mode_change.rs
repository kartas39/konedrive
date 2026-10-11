//! The switch of a folder between read-only and read-write, and the change of the
//! account's list of ignored names.
//!
//! **State owned:** none: the access mode is the mount's switch, and that a read-only
//! folder sends nothing is a hold of `SendingHolds` (`SendingHold::FolderReadOnly`).
//!
//! **Answers for:** the owner's rule for a switch to read-only, one for the account and
//! the folder:
//!
//! - asked while something is unsent and not forced, the switch is not made and not
//!   refused: the ACCOUNT keeps the wish (`account::mode`, `keep_read_only_wish`), the
//!   folder stays read-write, goes on taking changes and sending them, and the plan's
//!   task says when nothing is unsent (`SyncPlanTaskParts::unsent_count_report`); the
//!   account then makes the switch by itself. Nothing of that is this module's: until
//!   the switch itself the folder is a read-write folder;
//! - the switch itself — forced, or with nothing unsent — goes through at once
//!   ([`switch_to_read_only`]): what is unsent then stays here, not sent and not lost,
//!   under the hold, and is sent when the folder is read-write again. Nothing is
//!   dropped, ever, and no stamp of the switch is kept: a read-only folder sends
//!   nothing at all;
//! - no request that changes OneDrive leaves under a token that cannot write: the hold
//!   is set, and what is in flight has ended, BEFORE [`switch_to_read_only`] returns, and
//!   the account gives its token up only after it returned
//!   (`PendingUploads::hold_unsent_changes`; the account's order is `config.toml`, its
//!   mode worked out again, this switch awaited, the token). So no 403 comes from a switch, and nothing
//!   is kept as refused for good because of one.
//!
//! The sync is not stopped and started for a switch (today `follow_mode` rebuilds it,
//! `mode.rs:39-73`): every task runs in either access mode.
//!
//! **The order, to read-only:** the hold (`SendingHolds::set_folder_read_only(true)`,
//! which wakes the plan: nothing towards OneDrive is started from here on, an upload
//! stops after its fragment and keeps its session); the mount's switch, awaited
//! (`ItemMount::switch_to_read_only`: made on the write-operation thread, so that no
//! write operation is half done across it; after it the mount writes only the last
//! closes of files that were open); then the steps towards OneDrive that still run are
//! waited for (`RunningSteps::list_steps` has none of the classes `Content` and
//! `Metadata` left; a step whose answer was lost goes on reading what became of its
//! request, which a token that cannot write may do).
//! **To read-write:** the mount's switch, then the hold off (which wakes the plan).
//!
//! **Tested without the rest:**
// test: a_folder_switched_to_read_only_refuses_changes_at_once_and_sends_nothing_more
// test: what_was_unsent_at_the_switch_is_still_unsent_after_it_and_is_sent_when_read_write_again
// test: the_switch_to_read_only_returns_only_when_no_step_towards_onedrive_runs
// test: a_close_after_the_switch_is_written_and_waits_under_the_hold
// test: the_switch_stops_neither_task
// test: a_folder_started_read_only_holds_what_is_unsent_from_its_first_plan
// TODO: `SendingState::ReadOnly { last_unsent_since }`, the store's stamp of the switch (`item_store::sending_switch`: `switch_sending_to_read_only`, `switch_sending_to_read_write`, `find_sending_state`) and the plan's rule for it have no caller by this rule: a read-only folder is held as a whole. Check that nothing else needs them, and remove them with the plan's rule if so.
#![allow(dead_code, unused_variables)]

use crate::config::Mode;
use crate::mount::ItemMount;
use crate::sync_parts::running_steps::RunningSteps;
use crate::sync_parts::sending_holds::SendingHolds;
use crate::sync_parts::sync_plan_wake::SyncPlanWake;

/// Switches the folder to read-only: the order of the module's doc. Answers when the
/// folder takes no change, sends nothing, and no step towards OneDrive runs. The tasks go
/// on. Called by the owner of the folder's state (`PendingUploads::hold_unsent_changes`,
/// and `follow_mode` for a folder that follows an account that turned read-only for a
/// cause of its own), with the parts of the running sync; how many changes stay unsent is
/// the caller's to count afterwards (`ItemReader::count_unsent`). `Err` when the mount's
/// thread is gone: the hold stands all the same.
pub async fn switch_to_read_only(mount: &ItemMount, sending_holds: &SendingHolds, running_steps: &RunningSteps) -> Result<(), String> {
    todo!()
}

/// Switches the folder to read-write: the order of the module's doc. What was unsent
/// under the hold is planned at once.
pub async fn switch_to_read_write(mount: &ItemMount, sending_holds: &SendingHolds) -> Result<(), String> {
    todo!()
}

/// At a start, before the tasks: makes the mount's access mode the wanted one, and the
/// hold with it (`SendingHolds::set_folder_read_only(wanted == Mode::ReadOnly)`), before
/// any plan is made: a folder started read-only sends nothing from its first plan on.
pub async fn apply_wanted_access_mode(mount: &ItemMount, sending_holds: &SendingHolds, wanted: Mode) -> Result<(), String> {
    todo!()
}

/// The account's list of ignored names changed: the rows are gone over once
/// (`ItemMount::rewrite_ignored_names`: a job of the write-operation thread, so under the
/// tree lock) and the plan woken (`WakeCause::SendingHoldChanged`). Called by
/// `account_folder/settings.rs` for a folder whose mount is served, whether or not its
/// sync runs: `sync_plan_wake` is the one the mount was built with
/// (`KeptFolderParts::sync_plan_wake`). How many rows changed. MISSING-4.f.
pub async fn apply_ignore_list_change(mount: &ItemMount, sync_plan_wake: &SyncPlanWake) -> Result<u64, String> {
    todo!()
}
