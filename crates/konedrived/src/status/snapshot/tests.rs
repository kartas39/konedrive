use crate::helper::status::HelperState;
use super::*;

#[test]
fn the_published_state_is_computed_from_the_registration_and_the_sync() {
    let mut s = SyncSnapshot { root_state: RootState::Ready, ..SyncSnapshot::default() };
    assert_eq!(published_state(&s), "ready");
    s.listing = true;
    assert_eq!(published_state(&s), "listing");
    s.sync_trouble = Some(SyncTrouble { text: "cannot reach OneDrive".into(), blocking: false });
    assert_eq!(published_state(&s), "listing", "no network is said, not an error");
    s.sync_trouble = Some(SyncTrouble { text: "signed out".into(), blocking: true });
    assert_eq!(published_state(&s), "error");
    s.root_state = RootState::Error;
    s.last_error = "the helper is not connected".into();
    s.replacement_note = "1 file(s) changed in OneDrive could not be updated here yet: no space".into();
    s.conflict_count = 1;
    assert_eq!(
        published_error(&s),
        "the helper is not connected. signed out. 1 file(s) changed in OneDrive could not be updated here yet: no space",
        "a conflict is not a problem; it is not in LastError"
    );
    assert_eq!(published_state(&SyncSnapshot::default()), "none");

    // `listing` never hides `no-interception`.
    let s = SyncSnapshot { root_state: RootState::NoInterception, listing: true, ..SyncSnapshot::default() };
    assert_eq!(published_state(&s), "no-interception");
}

/// HS3: while a folder waits for the helper, `RootState` reads `error`
/// and `LastError` begins with what `HelperState` says — how to install
/// it, start it, or see why it failed — ahead of whatever else is said.
#[test]
fn a_folder_waiting_for_the_helper_says_how_to_start_it() {
    let said = |helper_state| {
        let s = SyncSnapshot {
            root_state: RootState::Ready,
            waits_for_helper: true,
            helper_state,
            last_error: "recovery left 1 file".into(),
            ..SyncSnapshot::default()
        };
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

    let s = SyncSnapshot { root_state: RootState::Ready, helper_state: HelperState::Stopped, ..SyncSnapshot::default() };
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
}

/// The note of a failed switch stands behind the registration's text, and goes when that
/// text is said anew or taken back.
#[test]
fn the_switch_note_stands_behind_the_registrations_text_and_goes_with_it() {
    let note = SwitchNote { why: "errno 5".into() };
    let mut s = SyncSnapshot { switch_note: Some(note.clone()), ..SyncSnapshot::default() };
    assert_eq!(published_error(&s), note.text());
    s.set_error("recovery left 1 file");
    assert_eq!(published_error(&s), "recovery left 1 file", "a new text takes the note");
    s.switch_note = Some(note.clone());
    assert_eq!(published_error(&s), format!("recovery left 1 file. {}", note.text()));
    s.clear_error();
    assert_eq!(published_error(&s), "");
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
