use super::*;

#[test]
fn state_strings_match_the_dbus_api() {
    assert_eq!(SignInState::SignedOut.as_str(), "signed-out");
    assert_eq!(SignInState::SigningIn.as_str(), "signing-in");
    assert_eq!(SignInState::SignedIn.as_str(), "signed-in");
}

#[tokio::test]
async fn update_notifies_subscribers() {
    let handle = StateHandle::new(AccountSnapshot::default());
    let mut changes = handle.subscribe();
    handle.update(|s| s.email = "a@example.com".into());
    changes.changed().await.unwrap();
    assert_eq!(changes.borrow().email, "a@example.com");
}

#[test]
fn try_transition_requires_the_expected_state() {
    let handle = StateHandle::new(AccountSnapshot::default());
    assert!(handle.try_transition(SignInState::SignedOut, SignInState::SigningIn));
    assert!(!handle.try_transition(SignInState::SignedOut, SignInState::SigningIn));
    assert_eq!(handle.get().state, SignInState::SigningIn);
}

#[test]
fn clear_account_keeps_state_and_client_id() {
    let mut s = AccountSnapshot {
        state: SignInState::SignedIn,
        client_id: "cid".into(),
        display_name: "n".into(),
        email: "e".into(),
        quota: QuotaFigures { used: 1, total: 2, ..QuotaFigures::default() },
        ..AccountSnapshot::default()
    };
    s.clear_account();
    assert_eq!(s.state, SignInState::SignedIn);
    assert_eq!(s.client_id, "cid");
    assert_eq!((s.display_name.as_str(), s.email.as_str(), s.quota), ("", "", QuotaFigures::default()));
}

/// Every note's text, written out: clients read it (limitations log F64).
#[test]
fn every_mode_note_says_what_last_error_said() {
    let said = |note: ModeNote| note.text();
    assert_eq!(
        said(ModeNote::WiderGrant("Files.ReadWrite offline_access".into())),
        "Microsoft answered a request for read-only access with a token that can also change files \
         (Files.ReadWrite offline_access); konedrive uses it to read only. The consent stays with Microsoft until it \
         is revoked at https://account.live.com/consent/Manage"
    );
    assert_eq!(
        said(ModeNote::ConfigUnreadable),
        "config.toml cannot be read now, so this account runs read-only until it can"
    );
    assert_eq!(
        said(ModeNote::GateKeepsReadOnly),
        "config.toml sets this account to read-write, but while uploads are being developed only the test accounts \
         in write_test_drive_ids can be; it runs read-only"
    );
    assert_eq!(
        said(ModeNote::DriveMismatch { live: "D2".into(), recorded: "D1".into() }),
        "this account's sign-in reaches the OneDrive drive D2, but config.toml records drive D1 for it; it runs \
         read-only until the two agree"
    );
    assert_eq!(
        said(ModeNote::SignInToWrite),
        "Changes made here are not uploaded: this account's sign-in does not allow konedrive to change files in \
         OneDrive. Switch it to read-write again to sign in with that permission."
    );
    assert_eq!(
        said(ModeNote::DriveNotSeen),
        "config.toml sets this account to read-write, but which OneDrive its sign-in reaches has not been checked \
         yet; it runs read-only until it is"
    );
}

/// An error and the mode's note stand in one place: each takes the other's, and a note
/// taken back leaves an error said since.
#[test]
fn an_error_and_the_mode_note_take_each_others_place() {
    let mut s = AccountSnapshot::default();
    s.set_error("Could not load account info: no network");
    s.set_mode_note(Some(ModeNote::SignInToWrite));
    assert_eq!(s.published_error(), ModeNote::SignInToWrite.text(), "a note replaces an error");
    s.set_mode_note(None);
    assert_eq!(s.published_error(), "", "and the error it replaced does not come back");

    s.set_mode_note(Some(ModeNote::DriveNotSeen));
    s.set_error("Could not load account info: no network");
    assert_eq!(s.published_error(), "Could not load account info: no network", "an error replaces a note");
    s.set_mode_note(None);
    assert_eq!(s.published_error(), "Could not load account info: no network", "a note taken back leaves the error");

    s.set_mode_note(Some(ModeNote::ConfigUnreadable));
    s.clear_error();
    assert_eq!((s.published_error().as_str(), &s.mode_note), ("", &None), "a clear takes both");
}
