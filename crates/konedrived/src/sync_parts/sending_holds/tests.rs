use std::collections::BTreeSet;

use futures_util::FutureExt;

use super::*;
use crate::account::state::ModeNote;
use crate::sync_parts::sync_plan_wake::WakeCause;
use crate::sync_parts::sending_holds::BACKOFF_MAX;

const NOW: i64 = 1_700_000_000;

fn secs(seconds: u64) -> Option<Duration> {
    Some(Duration::from_secs(seconds))
}

/// The holds of a folder whose first listing ended, with nothing waking the plan yet.
fn sending_holds() -> (Arc<SyncPlanWake>, SendingHolds) {
    let wake = Arc::new(SyncPlanWake::new());
    let holds = SendingHolds::new(Arc::clone(&wake));
    holds.set_listing_ended();
    assert!(is_plan_woken(&wake));
    (wake, holds)
}

/// Whether the plan was woken since this was last asked, for a change of what holds.
fn is_plan_woken(wake: &SyncPlanWake) -> bool {
    match wake.wait().now_or_never() {
        Some(causes) => {
            assert_eq!(causes, BTreeSet::from([WakeCause::SendingHoldChanged]));
            true
        }
        None => false,
    }
}

fn throttled_until(holds: &SendingHolds, now: i64) -> Option<i64> {
    match holds.find(now) {
        Some(SendingHold::Throttled { until }) => Some(until),
        None => None,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_time_onedrive_named_beats_one_that_was_chosen_and_the_later_of_two_stands() {
    // Of two times OneDrive named, the later end stands, whichever answer came last.
    let (_wake, holds) = sending_holds();
    holds.throttle(secs(120), NOW);
    holds.throttle(secs(1), NOW + 1);
    assert_eq!(throttled_until(&holds, NOW + 1), Some(NOW + 120));
    holds.throttle(secs(300), NOW + 2);
    assert_eq!(throttled_until(&holds, NOW + 2), Some(NOW + 302));
    // A named time takes the place of a chosen one, even a shorter one; a refusal with no
    // time adds nothing to a named wait.
    let (_wake, holds) = sending_holds();
    holds.throttle(None, NOW);
    holds.throttle(None, NOW + 10);
    assert_eq!(throttled_until(&holds, NOW + 10), Some(NOW + 30));
    holds.throttle(secs(5), NOW + 11);
    assert_eq!(throttled_until(&holds, NOW + 11), Some(NOW + 16), "OneDrive said when");
    holds.throttle(None, NOW + 12);
    assert_eq!(throttled_until(&holds, NOW + 12), Some(NOW + 16));
    holds.throttle(secs(2), NOW + 13);
    assert_eq!(throttled_until(&holds, NOW + 13), Some(NOW + 16), "a named wait is not cut short by a named one");
    // As long as OneDrive asked: at least a second, at most an hour.
    let (_wake, holds) = sending_holds();
    holds.throttle(secs(0), NOW);
    assert_eq!(throttled_until(&holds, NOW), Some(NOW + 1));
    assert_eq!(throttled_until(&holds, NOW + 1), None, "over when its second has come");
    holds.throttle(secs(100_000), NOW + 5);
    assert_eq!(throttled_until(&holds, NOW + 5), Some(NOW + 5 + 3600));
}

#[test]
fn with_no_time_named_the_wait_doubles_from_ten_seconds_to_an_hour() {
    let (_wake, holds) = sending_holds();
    let mut now = NOW;
    let mut waits = Vec::new();
    for _throttle in 0..11 {
        holds.throttle(None, now);
        // The steps refused together are one throttle.
        holds.throttle(None, now + 1);
        let until = throttled_until(&holds, now + 1).unwrap();
        waits.push(until - now);
        now = until;
    }
    assert_eq!(waits, [10, 20, 40, 80, 160, 320, 640, 1280, 2560, 3600, 3600]);
    assert_eq!(BACKOFF_MAX, Duration::from_secs(3600));
}

#[test]
fn a_step_that_went_through_ends_the_chosen_wait() {
    let (_wake, holds) = sending_holds();
    holds.throttle(None, NOW);
    holds.throttle(None, NOW + 10);
    assert_eq!(throttled_until(&holds, NOW + 10), Some(NOW + 30), "the next throttle waits 20 s");
    holds.pass();
    assert_eq!(throttled_until(&holds, NOW + 10), Some(NOW + 30), "the wait under way is not cut short");
    holds.throttle(None, NOW + 30);
    assert_eq!(throttled_until(&holds, NOW + 30), Some(NOW + 40), "10 s again once a step went through");
}

#[test]
fn each_cause_is_set_and_cleared_by_itself_and_wakes_the_plan() {
    let wake = Arc::new(SyncPlanWake::new());
    let holds = SendingHolds::new(Arc::clone(&wake));
    assert_eq!(holds.find(NOW), Some(SendingHold::FirstListingNotEnded), "nothing holds but the first listing");
    assert!(!is_plan_woken(&wake));
    holds.set_listing_ended();
    assert_eq!((holds.find(NOW), is_plan_woken(&wake)), (None, true));
    holds.set_listing_ended();
    assert!(!is_plan_woken(&wake), "what changes nothing wakes nobody");

    holds.set_paused(Some(None));
    assert_eq!((holds.find(NOW), is_plan_woken(&wake)), (Some(SendingHold::Paused { until: None }), true));
    holds.set_paused(Some(None));
    assert!(!is_plan_woken(&wake));
    holds.set_paused(Some(Some(NOW + 60)));
    assert_eq!((holds.find(NOW), is_plan_woken(&wake)), (Some(SendingHold::Paused { until: Some(NOW + 60) }), true));
    holds.set_paused(None);
    assert_eq!((holds.find(NOW), is_plan_woken(&wake)), (None, true));

    holds.set_signed_out(true);
    assert_eq!((holds.find(NOW), is_plan_woken(&wake)), (Some(SendingHold::SignedOut), true));
    holds.set_signed_out(true);
    assert!(!is_plan_woken(&wake));
    holds.set_signed_out(false);
    assert_eq!((holds.find(NOW), is_plan_woken(&wake)), (None, true));

    holds.set_writes_refusal(Some(ModeNote::SignInToWrite));
    assert_eq!((holds.find(NOW), is_plan_woken(&wake)), (Some(SendingHold::WritesNotAllowed { reason: ModeNote::SignInToWrite }), true));
    holds.set_writes_refusal(Some(ModeNote::DriveNotSeen));
    assert_eq!((holds.find(NOW), is_plan_woken(&wake)), (Some(SendingHold::WritesNotAllowed { reason: ModeNote::DriveNotSeen }), true), "another reason is a change");
    holds.set_writes_refusal(None);
    assert_eq!((holds.find(NOW), is_plan_woken(&wake)), (None, true));

    holds.throttle(secs(30), NOW);
    assert_eq!((holds.find(NOW), is_plan_woken(&wake)), (Some(SendingHold::Throttled { until: NOW + 30 }), true));
    holds.throttle(None, NOW + 1);
    assert!(!is_plan_woken(&wake), "a refusal that adds nothing");
    holds.pass();
    assert!(!is_plan_woken(&wake), "a step that went through holds and frees nothing");
    assert_eq!(holds.find(NOW + 30), None, "a throttle ends by its time");

    holds.set_stopping();
    assert_eq!((holds.find(NOW), is_plan_woken(&wake)), (Some(SendingHold::Stopping), true));
    holds.set_stopping();
    assert!(!is_plan_woken(&wake));
}

#[test]
fn the_first_cause_in_the_order_of_the_cases_is_the_one_shown() {
    let wake = Arc::new(SyncPlanWake::new());
    let holds = SendingHolds::new(wake);
    assert_eq!((holds.find(NOW), holds.find_end(NOW)), (Some(SendingHold::FirstListingNotEnded), None));
    holds.throttle(secs(30), NOW);
    assert_eq!((holds.find(NOW), holds.find_end(NOW)), (Some(SendingHold::Throttled { until: NOW + 30 }), Some(NOW + 30)));
    holds.set_paused(Some(Some(NOW + 60)));
    assert_eq!((holds.find(NOW), holds.find_end(NOW)), (Some(SendingHold::Paused { until: Some(NOW + 60) }), Some(NOW + 30)), "the throttle behind the pause ends first");
    assert_eq!(holds.find(NOW + 60), Some(SendingHold::FirstListingNotEnded), "a pause and a throttle whose end passed hold no more");
    holds.set_paused(Some(None));
    assert_eq!((holds.find(NOW), holds.find_end(NOW + 30)), (Some(SendingHold::Paused { until: None }), None), "a pause until resumed ends by nothing");
    holds.set_writes_refusal(Some(ModeNote::SignInToWrite));
    assert_eq!((holds.find(NOW), holds.find_end(NOW + 30)), (Some(SendingHold::WritesNotAllowed { reason: ModeNote::SignInToWrite }), None));
    holds.set_signed_out(true);
    assert_eq!(holds.find(NOW), Some(SendingHold::SignedOut));
    holds.set_stopping();
    assert_eq!((holds.find(NOW), holds.find_end(NOW + 30)), (Some(SendingHold::Stopping), None));
}

#[test]
fn a_throttle_holds_no_download_and_a_pause_does() {
    let every_hold = [
        (SendingHold::Stopping, true),
        (SendingHold::SignedOut, true),
        (SendingHold::WritesNotAllowed { reason: ModeNote::SignInToWrite }, false),
        (SendingHold::Paused { until: None }, true),
        (SendingHold::Paused { until: Some(NOW) }, true),
        (SendingHold::Throttled { until: NOW }, false),
        (SendingHold::FirstListingNotEnded, false),
    ];
    for (hold, holds_download) in &every_hold {
        assert!(decide_is_held(Some(hold), StepClass::Content) && decide_is_held(Some(hold), StepClass::Metadata), "{hold:?} holds what is sent");
        assert_eq!(decide_is_held(Some(hold), StepClass::Download), *holds_download, "{hold:?}");
        assert!(!decide_is_held(Some(hold), StepClass::Folder) && !decide_is_held(Some(hold), StepClass::Row), "{hold:?} holds no step towards the folder or of the rows");
    }
    for class in [StepClass::Content, StepClass::Metadata, StepClass::Download, StepClass::Folder, StepClass::Row] {
        assert!(!decide_is_held(None, class));
    }
}

/// The hold shown is the first; a step is held by ANY that holds its class.
#[test]
fn a_download_is_held_by_a_pause_that_is_not_the_hold_shown() {
    let (_wake, holds) = sending_holds();
    holds.set_writes_refusal(Some(ModeNote::SignInToWrite));
    assert!(!holds.is_held(StepClass::Download, NOW) && holds.is_held(StepClass::Content, NOW));
    holds.set_paused(Some(Some(NOW + 60)));
    assert_eq!(holds.find(NOW), Some(SendingHold::WritesNotAllowed { reason: ModeNote::SignInToWrite }), "the pause is not the one shown");
    assert!(holds.is_held(StepClass::Download, NOW), "and holds the download all the same");
    assert!(!holds.is_held(StepClass::Download, NOW + 60), "until its end");
    assert!(!holds.is_held(StepClass::Folder, NOW) && !holds.is_held(StepClass::Row, NOW));
    // The classes held, as the choice of what starts is given them; and the end the
    // plan's timer waits for is the pause's, though the hold shown ends by nothing.
    assert_eq!(holds.list_held_classes(NOW), BTreeSet::from([StepClass::Content, StepClass::Metadata, StepClass::Download]));
    assert_eq!(holds.list_held_classes(NOW + 60), BTreeSet::from([StepClass::Content, StepClass::Metadata]));
    assert_eq!(holds.find_end(NOW), Some(NOW + 60), "the soonest end of anything that holds");
    holds.throttle(secs(30), NOW);
    assert_eq!(holds.find_end(NOW), Some(NOW + 30));
    assert_eq!(holds.find_end(NOW + 60), None);
    holds.set_writes_refusal(None);
    assert!(!holds.is_held(StepClass::Content, NOW + 60));
}

#[test]
fn a_read_only_folder_that_still_sends_what_was_unsent_is_not_held_for_being_read_only() {
    // The gate's two refusals of a read-only folder and account are not given here: its
    // caller gives none for them.
    let (wake, holds) = sending_holds();
    holds.set_writes_refusal(None);
    assert_eq!(holds.find(NOW), None);
    assert!(!is_plan_woken(&wake));
    assert!(!holds.is_held(StepClass::Content, NOW) && !holds.is_held(StepClass::Metadata, NOW));
}

#[test]
fn a_read_only_folder_holds_what_is_sent_and_no_download_and_is_shown_after_the_accounts_refusal() {
    let (wake, holds) = sending_holds();
    holds.set_folder_read_only(true);
    assert_eq!((holds.find(NOW), is_plan_woken(&wake)), (Some(SendingHold::FolderReadOnly), true));
    holds.set_folder_read_only(true);
    assert!(!is_plan_woken(&wake), "what changes nothing wakes nobody");
    // What is sent is held, a download and what goes towards the folder are not; it ends by nothing.
    assert_eq!(holds.list_held_classes(NOW), BTreeSet::from([StepClass::Content, StepClass::Metadata]));
    assert!(holds.is_held(StepClass::Content, NOW) && holds.is_held(StepClass::Metadata, NOW) && !holds.is_held(StepClass::Download, NOW));
    assert!(decide_is_held(Some(&SendingHold::FolderReadOnly), StepClass::Content) && decide_is_held(Some(&SendingHold::FolderReadOnly), StepClass::Metadata));
    assert!(!decide_is_held(Some(&SendingHold::FolderReadOnly), StepClass::Download) && !decide_is_held(Some(&SendingHold::FolderReadOnly), StepClass::Folder) && !decide_is_held(Some(&SendingHold::FolderReadOnly), StepClass::Row));
    assert_eq!(holds.find_end(NOW), None);
    // Shown before a pause and a throttle, after the account's own refusal.
    holds.throttle(secs(30), NOW);
    holds.set_paused(Some(None));
    assert_eq!(holds.find(NOW), Some(SendingHold::FolderReadOnly));
    assert!(holds.is_held(StepClass::Download, NOW), "the pause behind it holds the download");
    holds.set_writes_refusal(Some(ModeNote::SignInToWrite));
    assert_eq!(holds.find(NOW), Some(SendingHold::WritesNotAllowed { reason: ModeNote::SignInToWrite }));
    // Neither setter clears the other's hold.
    holds.set_writes_refusal(None);
    assert_eq!(holds.find(NOW), Some(SendingHold::FolderReadOnly), "the account's refusal ended, the folder is read-only still");
    holds.set_writes_refusal(Some(ModeNote::SignInToWrite));
    holds.set_paused(None);
    while is_plan_woken(&wake) {}
    holds.set_folder_read_only(false);
    assert_eq!((holds.find(NOW), is_plan_woken(&wake)), (Some(SendingHold::WritesNotAllowed { reason: ModeNote::SignInToWrite }), true), "read-write again: the account's refusal stands, and the plan is woken");
    holds.set_writes_refusal(None);
    assert_eq!(holds.find(NOW), Some(SendingHold::Throttled { until: NOW + 30 }));
    assert_eq!(holds.find(NOW + 30), None);
}
