//! The switch of a folder between read-only and read-write over a mount that is built and
//! not served, the bounded wait for the steps towards OneDrive on a paused clock, and the
//! change of the ignore list.

use std::collections::BTreeSet;
use std::sync::Arc;

use konedrive_tree::LocalItemId;

use super::*;
use crate::local::IgnoreList;
use crate::mount::item_fixture::ItemFixture;
use crate::mount::TakeDownCause;
use crate::sync_parts::sending_holds::SendingHold;
use crate::sync_tasks::tests::block_on_paused_clock;

/// The holds of a folder over a wake of their own, with the first listing ended: nothing
/// holds but what the test sets.
fn make_holds() -> (Arc<SyncPlanWake>, SendingHolds) {
    let sync_plan_wake = Arc::new(SyncPlanWake::new());
    let sending_holds = SendingHolds::new(Arc::clone(&sync_plan_wake));
    sending_holds.set_listing_ended();
    (sync_plan_wake, sending_holds)
}

fn running_step(item: i64, class: StepClass) -> RunningStep {
    RunningStep { class, item: LocalItemId::new(item), other_items: Vec::new(), destination: None, sent: 0, total: 0 }
}

#[test]
fn only_a_step_that_sends_content_or_metadata_is_a_step_towards_onedrive() {
    let cases = [
        (StepClass::Content, true),
        (StepClass::Metadata, true),
        (StepClass::Download, false),
        (StepClass::Folder, false),
        (StepClass::Row, false),
    ];
    for (class, is_towards_onedrive) in cases {
        assert_eq!(has_step_towards_onedrive(&[running_step(1, class)]), is_towards_onedrive, "{class:?}");
    }
    assert!(!has_step_towards_onedrive(&[]));
    assert!(has_step_towards_onedrive(&[running_step(1, StepClass::Folder), running_step(2, StepClass::Row), running_step(3, StepClass::Metadata)]));
}

#[test]
fn a_folder_switched_to_read_only_refuses_changes_at_once_and_sends_nothing_more() {
    let fixture = ItemFixture::new();
    let mount = fixture.build_mount();
    let (sync_plan_wake, sending_holds) = make_holds();
    let running_steps = RunningSteps::new();
    // A download and a step towards the folder run: neither is waited for.
    assert!(running_steps.begin(running_step(7, StepClass::Download)) && running_steps.begin(running_step(8, StepClass::Folder)));
    fixture.runtime().block_on(async {
        switch_to_read_write(&mount, &sending_holds).await.unwrap();
        assert!(mount.access_mode_switch().is_read_write());
        sync_plan_wake.wait().await;

        let steps = switch_to_read_only(&mount, &sending_holds, &running_steps).await.unwrap();

        assert_eq!(steps, StepsTowardsOneDrive::Ended);
        assert!(!mount.access_mode_switch().is_read_write());
        assert_eq!(sending_holds.find(0), Some(SendingHold::FolderReadOnly));
        assert!(sending_holds.is_held(StepClass::Content, 0) && sending_holds.is_held(StepClass::Metadata, 0) && !sending_holds.is_held(StepClass::Download, 0));
        assert_eq!(sync_plan_wake.wait().await, BTreeSet::from([WakeCause::SendingHoldChanged]), "the hold wakes the plan");

        // And back: the mount takes changes, and the hold is off.
        switch_to_read_write(&mount, &sending_holds).await.unwrap();
        assert!(mount.access_mode_switch().is_read_write());
        assert_eq!(sending_holds.find(0), None);
        assert_eq!(sync_plan_wake.wait().await, BTreeSet::from([WakeCause::SendingHoldChanged]));
    });
}

#[test]
fn the_switch_to_read_only_returns_only_when_no_step_towards_onedrive_runs() {
    let fixture = ItemFixture::new();
    let mount = fixture.build_mount();
    let (_, sending_holds) = make_holds();
    let running_steps = Arc::new(RunningSteps::new());
    let step = running_step(7, StepClass::Metadata);
    assert!(running_steps.begin(step.clone()));

    let (steps, waited) = block_on_paused_clock(async {
        // The step ends a second after the switch began.
        let ending = Arc::clone(&running_steps);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            ending.end(step.item);
        });
        let began = tokio::time::Instant::now();
        (switch_to_read_only(&mount, &sending_holds, &running_steps).await.unwrap(), began.elapsed())
    });

    assert_eq!(steps, StepsTowardsOneDrive::Ended);
    assert!(waited >= std::time::Duration::from_secs(1) && waited < RUNNING_STEPS_WAITED_FOR, "it waited for the step, and not for the bound: {waited:?}");
}

#[test]
fn a_step_towards_onedrive_that_does_not_end_holds_the_switch_for_its_bound_and_is_said() {
    let fixture = ItemFixture::new();
    let mount = fixture.build_mount();
    let (_, sending_holds) = make_holds();
    let running_steps = RunningSteps::new();
    assert!(running_steps.begin(running_step(7, StepClass::Content)));

    let (steps, waited) = block_on_paused_clock(async {
        let began = tokio::time::Instant::now();
        (switch_to_read_only(&mount, &sending_holds, &running_steps).await.unwrap(), began.elapsed())
    });

    assert_eq!(steps, StepsTowardsOneDrive::StillRunning, "the caller is told that a request may still be in flight");
    assert!(waited >= RUNNING_STEPS_WAITED_FOR && waited < RUNNING_STEPS_WAITED_FOR + RUNNING_STEPS_LOOKED_AT_EVERY * 2, "{waited:?}");
    assert!(!mount.access_mode_switch().is_read_write(), "the switch went on");
    assert_eq!(sending_holds.find(0), Some(SendingHold::FolderReadOnly));
}

#[test]
fn a_switch_to_read_only_of_a_mount_whose_thread_is_gone_is_an_error_and_the_hold_stands() {
    let fixture = ItemFixture::new();
    let mount = fixture.build_mount();
    let (_, sending_holds) = make_holds();
    mount.stop();

    let switched = fixture.runtime().block_on(switch_to_read_only(&mount, &sending_holds, &RunningSteps::new()));

    assert!(switched.is_err());
    assert_eq!(sending_holds.find(0), Some(SendingHold::FolderReadOnly));
    assert!(!mount.access_mode_switch().is_read_write());
}

#[test]
fn a_mount_whose_take_down_was_asked_is_not_switched_to_read_write() {
    let fixture = ItemFixture::new();
    let mount = fixture.build_mount();
    let (_, sending_holds) = make_holds();
    fixture.runtime().block_on(async {
        switch_to_read_write(&mount, &sending_holds).await.unwrap();
        switch_to_read_only(&mount, &sending_holds, &RunningSteps::new()).await.unwrap();
        mount.folder_step_parts().mount_take_down.ask(TakeDownCause::UndoFailed { path: "a fault".into() });

        let switched = switch_to_read_write(&mount, &sending_holds).await;

        assert!(switched.is_err());
        assert!(!mount.access_mode_switch().is_read_write());
        assert_eq!(sending_holds.find(0), Some(SendingHold::FolderReadOnly), "the hold is left on");
    });
}

#[test]
fn the_wanted_access_mode_is_the_mounts_and_the_holds_before_any_plan() {
    // (wanted, a take-down asked before, the mount takes changes after, the folder's hold stands after)
    let cases = [
        (Mode::ReadOnly, false, false, true),
        (Mode::ReadWrite, false, true, false),
        // A faulted mount stays read-only, under the hold, whatever is wanted; the start goes on.
        (Mode::ReadWrite, true, false, true),
        (Mode::ReadOnly, true, false, true),
    ];
    for (wanted, is_take_down_asked, takes_changes, is_held) in cases {
        let fixture = ItemFixture::new();
        let mount = fixture.build_mount();
        let (_, sending_holds) = make_holds();
        if is_take_down_asked {
            mount.folder_step_parts().mount_take_down.ask(TakeDownCause::UndoFailed { path: "a fault".into() });
        }

        let applied = fixture.runtime().block_on(apply_wanted_access_mode(&mount, &sending_holds, wanted));

        assert!(applied.is_ok(), "{wanted:?}, take-down asked: {is_take_down_asked}");
        assert_eq!(mount.access_mode_switch().is_read_write(), takes_changes, "{wanted:?}, take-down asked: {is_take_down_asked}");
        assert_eq!(sending_holds.find(0) == Some(SendingHold::FolderReadOnly), is_held, "{wanted:?}, take-down asked: {is_take_down_asked}");
    }
}

#[test]
fn a_folder_wanted_read_only_over_a_mount_whose_thread_is_gone_is_refused() {
    let fixture = ItemFixture::new();
    let mount = fixture.build_mount();
    let (_, sending_holds) = make_holds();
    mount.stop();

    assert!(fixture.runtime().block_on(apply_wanted_access_mode(&mount, &sending_holds, Mode::ReadOnly)).is_err(), "stage 5 of a start refuses");
    // Read-write needs no thread: the switch is made at once.
    assert!(fixture.runtime().block_on(apply_wanted_access_mode(&mount, &sending_holds, Mode::ReadWrite)).is_ok());
}

#[test]
fn a_change_of_the_ignore_list_goes_over_the_rows_once_and_wakes_the_plan() {
    let fixture = ItemFixture::new();
    let notes = fixture.make_own_file("notes.txt", b"notes");
    fixture.make_own_file("kept.txt", b"kept");
    let mount = fixture.build_mount();
    let sync_plan_wake = SyncPlanWake::new();
    *fixture.ignore.write().unwrap() = IgnoreList::new(["notes.txt"]);
    fixture.runtime().block_on(async {
        assert_eq!(apply_ignore_list_change(&mount, &sync_plan_wake).await, Ok(1), "one row's flag changed");
        assert_eq!(sync_plan_wake.wait().await, BTreeSet::from([WakeCause::SendingHoldChanged]));
        assert!(fixture.find_item(notes.local_item_id).unwrap().is_name_ignored);

        // A mount that is stopped goes over nothing, and the plan is not woken for it.
        mount.stop();
        assert!(apply_ignore_list_change(&mount, &sync_plan_wake).await.is_err());
        sync_plan_wake.wake(WakeCause::QuotaRead);
        assert_eq!(sync_plan_wake.wait().await, BTreeSet::from([WakeCause::QuotaRead]));
    });
}
