use super::*;
use crate::account::account_trouble::AccountTrouble;

#[test]
fn state_strings_match_the_dbus_api() {
    assert_eq!(SignInState::SignedOut.as_str(), "signed-out");
    assert_eq!(SignInState::SigningIn.as_str(), "signing-in");
    assert_eq!(SignInState::SignedIn.as_str(), "signed-in");
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

/// A wish for read-only that waits is no word of `LastError`: an error and a note come and
/// go beside it, and it goes when the account is no longer signed in.
#[test]
fn a_wish_for_read_only_is_a_state_beside_last_error() {
    let mut s = AccountSnapshot { read_only_wish: Some(ReadOnlyWish { unsent: 3 }), ..AccountSnapshot::default() };
    assert_eq!(s.published_error(), "");
    s.set_error(AccountTrouble::ProfileNotRead { detail: "no network".into() });
    s.set_mode_note(Some(ModeNote::DriveNotSeen));
    s.clear_error();
    assert_eq!((s.read_only_wish, s.published_error().as_str()), (Some(ReadOnlyWish { unsent: 3 }), ""));
    s.clear_account();
    assert_eq!(s.read_only_wish, None);
}

/// An error and the mode's note stand in one place: each takes the other's, and a note
/// taken back leaves an error said since.
#[test]
fn an_error_and_the_mode_note_take_each_others_place() {
    let mut s = AccountSnapshot::default();
    s.set_error(AccountTrouble::ProfileNotRead { detail: "no network".into() });
    s.set_mode_note(Some(ModeNote::SignInToWrite));
    assert_eq!(s.published_error(), ModeNote::SignInToWrite.to_string(), "a note replaces an error");
    s.set_mode_note(None);
    assert_eq!(s.published_error(), "", "and the error it replaced does not come back");

    s.set_mode_note(Some(ModeNote::DriveNotSeen));
    s.set_error(AccountTrouble::ProfileNotRead { detail: "no network".into() });
    assert_eq!(s.published_error(), "Could not load account info: no network", "an error replaces a note");
    s.set_mode_note(None);
    assert_eq!(s.published_error(), "Could not load account info: no network", "a note taken back leaves the error");

    s.set_mode_note(Some(ModeNote::ConfigUnreadable));
    s.clear_error();
    assert_eq!((s.published_error().as_str(), &s.mode_note), ("", &None), "a clear takes both");
}
