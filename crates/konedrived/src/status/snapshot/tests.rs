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
