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
        quota_used: 1,
        quota_total: 2,
        ..AccountSnapshot::default()
    };
    s.clear_account();
    assert_eq!(s.state, SignInState::SignedIn);
    assert_eq!(s.client_id, "cid");
    assert_eq!((s.display_name.as_str(), s.email.as_str(), s.quota_used, s.quota_total), ("", "", 0, 0));
}
