use crate::helper::status::HelperState;
use super::*;

/// A snapshot whose folder is `folder`, the rest as at the start.
fn folder(folder: FolderStatus) -> SyncSnapshot {
    SyncSnapshot { folder, ..SyncSnapshot::default() }
}

#[test]
fn the_published_state_is_computed_from_the_registration_and_the_sync() {
    let mut s = folder(FolderStatus { root_state: RootState::Ready, ..FolderStatus::default() });
    assert_eq!(published_state(&s), "ready");
    s.cycle.listing = true;
    assert_eq!(published_state(&s), "listing");
    s.cycle.sync_trouble = Some(SyncTrouble { text: "cannot reach OneDrive".into(), blocking: false });
    assert_eq!(published_state(&s), "listing", "no network is said, not an error");
    s.cycle.sync_trouble = Some(SyncTrouble { text: "signed out".into(), blocking: true });
    assert_eq!(published_state(&s), "error");
    s.folder.root_state = RootState::Error;
    s.folder.last_error = "the helper is not connected".into();
    s.cycle.replacement_note = Some(ReplacementNote { files: 1, why: "no space".into() });
    s.local.conflict_count = 1;
    assert_eq!(
        published_error(&s),
        "the helper is not connected. signed out. 1 file(s) changed in OneDrive could not be updated here yet: no space",
        "a conflict is not a problem; it is not in LastError"
    );
    assert_eq!(published_state(&SyncSnapshot::default()), "none");

    // `listing` never hides `no-interception`.
    let mut s = folder(FolderStatus { root_state: RootState::NoInterception, ..FolderStatus::default() });
    s.cycle.listing = true;
    assert_eq!(published_state(&s), "no-interception");
}

/// HS3: while a folder waits for the helper, `RootState` reads `error`
/// and `LastError` begins with what `HelperState` says — how to install
/// it, start it, or see why it failed — ahead of whatever else is said.
#[test]
fn a_folder_waiting_for_the_helper_says_how_to_start_it() {
    let said = |helper_state| {
        let s = folder(FolderStatus {
            root_state: RootState::Ready,
            waits_for_helper: true,
            helper_state,
            last_error: "recovery left 1 file".into(),
            ..FolderStatus::default()
        });
        (published_state(&s), published_error(&s))
    };
    let (state, error) = said(HelperState::NotInstalled);
    assert_eq!(state, "error");
    assert_eq!(
        error,
        "the konedrive helper is not installed: files are not kept in step and do not download when \
         opened. Install it: sudo scripts/install-helper.sh (see README). recovery left 1 file"
    );
    assert!(said(HelperState::Stopped).1.starts_with(
        "the konedrive helper is not running: start it with `sudo systemctl start konedrive-helper`. "
    ));
    assert!(said(HelperState::Failed).1.starts_with("the konedrive helper failed: see `systemctl status konedrive-helper`. "));
    assert!(said(HelperState::Unknown).1.starts_with("the konedrive helper is not connected. "));
    assert_eq!(said(HelperState::Connected).1, "recovery left 1 file", "nothing to say of a connected helper");

    let s = folder(FolderStatus { root_state: RootState::Ready, helper_state: HelperState::Stopped, ..FolderStatus::default() });
    assert_eq!((published_state(&s), published_error(&s).as_str()), ("ready", ""), "a folder not waiting says nothing of it");
}

/// Every note's text, written out: clients read `LastError`.
#[test]
fn every_note_says_what_last_error_said() {
    assert_eq!(
        SwitchNote { why: "errno 5".into() }.text(),
        "the konedrive helper is connected, but switching this folder to interception failed: errno 5; it is tried \
         again the next time the helper connects"
    );
    assert_eq!(OutboxNote::GateClosed("the account is read-only".into()).text(), "nothing is uploaded: the account is read-only");
    assert_eq!(
        OutboxNote::HeldBack(3).text(),
        "3 change(s) made here wait to be uploaded, so the folder is not kept in step with OneDrive: they go once \
         the account is read-write again, or are dropped by a forced switch to read-only"
    );
    assert_eq!(
        OutboxNote::Unreadable.text(),
        "the changes waiting to be uploaded cannot be read, so the folder is not kept in step with OneDrive"
    );
    assert_eq!(OutboxNote::FolderClosed("Permission denied (os error 13)".into()).text(), "the folder cannot be opened (Permission denied (os error 13)); uploads wait");
    // The time is the wall clock's here, whatever the zone: its shape is checked.
    let throttled = OutboxNote::Throttled(1_700_000_000).text();
    let time = throttled.strip_prefix("OneDrive asked to slow down; uploads continue at ").expect(&throttled);
    let (hour, minute) = time.split_once(':').expect(time);
    assert!(hour.len() == 2 && minute.len() == 2 && hour.parse::<u8>().unwrap() < 24 && minute.parse::<u8>().unwrap() < 60, "{time}");
}

/// The throttle's note names the minute the wait is over in: a time within a minute is
/// rounded up, so the note never names a minute that is past while the wait lasts.
#[test]
fn the_throttles_time_is_rounded_up_to_the_minute() {
    let minute = 1_700_000_040; // a multiple of 60
    assert_eq!(minute % 60, 0);
    let said = |at| OutboxNote::Throttled(at).text();
    assert_eq!(said(minute + 1), said(minute + 60), "a second into the minute reads as the next minute");
    assert_eq!(said(minute + 59), said(minute + 60));
    assert_ne!(said(minute), said(minute + 1), "the full minute reads as itself");
}

/// The worker's notes are said only where nothing else is, the folder's over the
/// throttle's, and the worker takes back only its own; a throttle shorter than five
/// seconds is not said, and one that is said stays until its end.
#[test]
fn the_worker_takes_back_only_its_own_notes() {
    const NOW: i64 = 1_000;
    let throttled = |until| Some(OutboxNote::Throttled(until));
    let closed = Some(OutboxNote::FolderClosed("EACCES".into()));
    let gate = Some(OutboxNote::GateClosed("why".into()));
    let after = |shown: &Option<OutboxNote>, folder: Option<&str>, until: Option<i64>, now: i64| OutboxNote::after_worker(shown, folder, until, now);
    assert_eq!(after(&None, None, Some(NOW + 100), NOW), Some(throttled(NOW + 100)));
    assert_eq!(after(&throttled(NOW + 100), None, Some(NOW + 100), NOW + 99), None, "said, it stays to its end");
    assert_eq!(after(&throttled(NOW + 100), None, Some(NOW + 160), NOW), Some(throttled(NOW + 160)));
    assert_eq!(after(&throttled(NOW + 100), None, None, NOW + 100), Some(None), "its own note goes as the wait ends");
    assert_eq!(after(&None, None, Some(NOW + 4), NOW), None, "a wait under five seconds is not said");
    assert_eq!(after(&None, None, Some(NOW + 5), NOW), Some(throttled(NOW + 5)));
    assert_eq!(after(&throttled(NOW + 100), None, Some(NOW + 3), NOW), Some(None), "a short wait that took a long one's place is not said either");
    assert_eq!(after(&gate, None, Some(NOW + 100), NOW), None, "the gate's stays");
    assert_eq!(after(&gate, Some("EACCES"), None, NOW), None);
    assert_eq!(after(&gate, None, None, NOW), None);
    assert_eq!(after(&Some(OutboxNote::HeldBack(3)), None, None, NOW), None, "the poller's stays");
    assert_eq!(after(&None, None, None, NOW), None);
    // The folder that cannot be opened is said over the throttle, and goes when it opens.
    assert_eq!(after(&throttled(NOW + 100), Some("EACCES"), Some(NOW + 100), NOW), Some(closed.clone()));
    assert_eq!(after(&closed, Some("EACCES"), None, NOW), None, "nothing changes");
    assert_eq!(after(&closed, None, Some(NOW + 100), NOW), Some(throttled(NOW + 100)), "opened, the throttle that still lasts is said");
    assert_eq!(after(&closed, None, None, NOW), Some(None));
    // The gate closing says so over the worker's notes, and takes neither back.
    assert_eq!(OutboxNote::after_gate(&throttled(100), Some("why")), Some(gate.clone()));
    assert_eq!(OutboxNote::after_gate(&throttled(100), None), None, "the gate does not take the throttle's note");
    assert_eq!(OutboxNote::after_gate(&closed, None), None);
}

/// The note of a failed switch stands behind the registration's text.
#[test]
fn the_switch_note_stands_behind_the_registrations_text() {
    let note = SwitchNote { why: "errno 5".into() };
    let mut s = folder(FolderStatus { switch_note: Some(note.clone()), ..FolderStatus::default() });
    assert_eq!(published_error(&s), note.text());
    s.folder.last_error = "recovery left 1 file".into();
    assert_eq!(published_error(&s), format!("recovery left 1 file. {}", note.text()));
}

/// A folder recorded and not up yet is calm while nothing is known to be wrong: `waiting`,
/// and nothing of the helper in `LastError`. Once the helper it waits for is known to be
/// missing, stopped or failed, it reads `error` and says what to do.
#[test]
fn a_folder_not_up_yet_waits_calmly_until_the_helper_is_known_to_be_down() {
    let said = |helper_state, waits_for_helper| {
        let s = folder(FolderStatus { root_state: RootState::Waiting, waits_for_helper, helper_state, ..FolderStatus::default() });
        (published_state(&s), published_error(&s))
    };
    assert_eq!(said(HelperState::Unknown, true), ("waiting", String::new()));
    assert_eq!(said(HelperState::Connected, true), ("waiting", String::new()), "being brought up");
    for down in [HelperState::NotInstalled, HelperState::Stopped, HelperState::Failed] {
        let (state, error) = said(down, true);
        assert_eq!(state, "error");
        assert_eq!(error, down.advice().unwrap());
    }
    assert_eq!(said(HelperState::Stopped, false), ("waiting", String::new()), "a folder that needs no helper is brought up without one");
}

/// The write gate says why it is closed over whatever is shown, and takes back only its
/// own note: what the poller says of a read-only folder stays.
#[test]
fn the_gate_takes_back_only_its_own_note() {
    let closed = |why: &str| Some(OutboxNote::GateClosed(why.to_owned()));
    assert_eq!(OutboxNote::after_gate(&None, Some("why")), Some(closed("why")));
    assert_eq!(OutboxNote::after_gate(&closed("why"), Some("why")), None, "nothing changes");
    assert_eq!(OutboxNote::after_gate(&closed("why"), None), Some(None), "its own note goes as it opens");
    assert_eq!(OutboxNote::after_gate(&Some(OutboxNote::HeldBack(3)), None), None, "the poller's stays");
    assert_eq!(OutboxNote::after_gate(&Some(OutboxNote::HeldBack(3)), Some("why")), Some(closed("why")));
    assert_eq!(OutboxNote::after_gate(&None, None), None);
}

/// Only a read-write folder has a watcher, and so a local scan.
#[test]
fn a_read_only_folder_has_no_local_scan() {
    let mut scan = LocalScan::default();
    assert_eq!(scan.state, ScanState::None);
    scan.follow(Mode::ReadWrite);
    assert_eq!(scan.state, ScanState::Idle);
    scan.follow(Mode::ReadOnly);
    assert_eq!(scan.state.as_str(), "none");
}
