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
//!   (`switch_to_read_only`): what is unsent then stays here, not sent and not lost,
//!   under the hold, and is sent when the folder is read-write again. Nothing is
//!   dropped, ever, and no stamp of the switch is kept: a read-only folder sends
//!   nothing at all;
//! - no request that changes OneDrive leaves under a token that cannot write: the hold
//!   is set, and what is in flight has ended — or the bounded wait for it ran out, which
//!   the answer says (`StepsTowardsOneDrive::StillRunning`) —, BEFORE
//!   `switch_to_read_only` returns, and
//!   the account gives its token up only after it returned
//!   (`AccountFolder::hold_unsent_changes`; the account's order is `config.toml`, its
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
#![allow(dead_code, unused_variables)]

use crate::config::Mode;
use crate::mount::ItemMount;
use crate::sync_parts::running_steps::{RunningStep, RunningSteps, StepClass};
use crate::sync_parts::sending_holds::SendingHolds;
use crate::sync_parts::sync_plan_wake::{SyncPlanWake, WakeCause};

/// How often a switch to read-only looks whether a step towards OneDrive still runs:
/// `RunningSteps` tells nobody when a step ends.
const RUNNING_STEPS_LOOKED_AT_EVERY: std::time::Duration = std::time::Duration::from_millis(50);

/// How long a switch to read-only waits for the steps towards OneDrive that still run. A
/// guess: longer than a fragment of an upload takes on a slow line.
const RUNNING_STEPS_WAITED_FOR: std::time::Duration = std::time::Duration::from_secs(30);

/// Whether the steps towards OneDrive had ended when a switch to read-only returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepsTowardsOneDrive {
    /// None runs: no request that changes OneDrive is in flight.
    Ended,
    /// The wait ran out with one still running.
    StillRunning,
}

/// Whether a step towards OneDrive is among `steps`: one that sends content or
/// metadata. Pure.
fn has_step_towards_onedrive(steps: &[RunningStep]) -> bool {
    steps.iter().any(|step| matches!(step.class, StepClass::Content | StepClass::Metadata))
}

/// Switches the folder to read-only: the order of the module's doc. Answers when the
/// folder takes no change, sends nothing, and no step towards OneDrive runs. The tasks go
/// on. Called by the owner of the folder's state (`AccountFolder::hold_unsent_changes`,
/// and `follow_mode` for a folder that follows an account that turned read-only for a
/// cause of its own), with the parts of the running sync; how many changes stay unsent is
/// the caller's to count afterwards (`ItemReader::count_unsent`). `Err` when the mount's
/// thread is gone: the hold stands all the same.
///
/// The wait for the running steps is bounded ([`RUNNING_STEPS_WAITED_FOR`]): after it the
/// switch goes on with a warning and answers [`StepsTowardsOneDrive::StillRunning`].
/// Sending is held by then, so a step that still runs starts nothing new — an upload
/// stops after its fragment — but a request of it that hangs may still be sent again by
/// the Graph client. Whether the account gives its token up all the same is the
/// caller's: a request sent under a token that cannot write is refused, and that
/// refusal is kept.
pub(crate) async fn switch_to_read_only(mount: &ItemMount, sending_holds: &SendingHolds, running_steps: &RunningSteps) -> Result<StepsTowardsOneDrive, String> {
    // Nothing towards OneDrive is started from here on.
    sending_holds.set_folder_read_only(true);
    let switched = mount.switch_to_read_only().await;
    let waited_since = tokio::time::Instant::now();
    let mut steps = StepsTowardsOneDrive::Ended;
    while has_step_towards_onedrive(&running_steps.list_steps()) {
        if waited_since.elapsed() >= RUNNING_STEPS_WAITED_FOR {
            tracing::warn!("a step towards OneDrive still runs {} s after the folder was switched to read-only; the switch goes on", RUNNING_STEPS_WAITED_FOR.as_secs());
            steps = StepsTowardsOneDrive::StillRunning;
            break;
        }
        tokio::time::sleep(RUNNING_STEPS_LOOKED_AT_EVERY).await;
    }
    switched.map(|_| steps).map_err(|error| error.to_string())
}

/// Switches the folder to read-write: the order of the module's doc. What was unsent
/// under the hold is planned at once. `Err`, with the hold left on, for a mount whose
/// take-down was asked: the mount's own switch refuses it (`ItemMount::switch_to_read_write`,
/// in one step with the switch), and this passes the answer on.
pub(crate) async fn switch_to_read_write(mount: &ItemMount, sending_holds: &SendingHolds) -> Result<(), String> {
    mount.switch_to_read_write().map_err(|error| error.to_string())?;
    sending_holds.set_folder_read_only(false);
    Ok(())
}

/// At a start, before the tasks: makes the mount's access mode the wanted one, and the
/// hold with it (`SendingHolds::set_folder_read_only(wanted == Mode::ReadOnly)`), before
/// any plan is made: a folder started read-only sends nothing from its first plan on. A
/// mount whose take-down was asked stays read-only, under the hold, whatever is wanted.
pub async fn apply_wanted_access_mode(mount: &ItemMount, sending_holds: &SendingHolds, wanted: Mode) -> Result<(), String> {
    match wanted {
        Mode::ReadOnly => {
            sending_holds.set_folder_read_only(true);
            mount.switch_to_read_only().await.map_err(|error| error.to_string())?;
        }
        // A mount that asked to be taken down refuses, and stays read-only under the
        // hold: the start goes on, and the owner's take-down follows.
        Mode::ReadWrite => sending_holds.set_folder_read_only(mount.switch_to_read_write().is_err()),
    }
    Ok(())
}

/// The account's list of ignored names changed: the rows are gone over once
/// (`ItemMount::rewrite_ignored_names`: a job of the write-operation thread, so under the
/// tree lock) and the plan woken (`WakeCause::SendingHoldChanged`). Called by
/// `account_folder/settings.rs` for a folder whose mount is served, whether or not its
/// sync runs: `sync_plan_wake` is the one the mount was built with
/// (`KeptFolderParts::sync_plan_wake`). How many rows changed. MISSING-4.f.
pub(crate) async fn apply_ignore_list_change(mount: &ItemMount, sync_plan_wake: &SyncPlanWake) -> Result<u64, String> {
    let rewritten = mount.rewrite_ignored_names().await.map_err(|error| format!("the ignored names of the folder's rows could not be gone over: {error}"))?;
    sync_plan_wake.wake(WakeCause::SendingHoldChanged);
    Ok(rewritten)
}

#[cfg(test)]
mod tests;
