//! The observable account state; the D-Bus layer turns changes into PropertiesChanged.

use std::sync::Arc;

use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInState {
    SignedOut,
    SigningIn,
    SignedIn,
}

impl SignInState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SignedOut => "signed-out",
            Self::SigningIn => "signing-in",
            Self::SignedIn => "signed-in",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSnapshot {
    pub state: SignInState,
    pub last_error: String,
    pub client_id: String,
    pub display_name: String,
    pub email: String,
    pub quota_used: u64,
    pub quota_total: u64,
}

impl Default for AccountSnapshot {
    fn default() -> Self {
        Self {
            state: SignInState::SignedOut,
            last_error: String::new(),
            client_id: String::new(),
            display_name: String::new(),
            email: String::new(),
            quota_used: 0,
            quota_total: 0,
        }
    }
}

impl AccountSnapshot {
    pub fn clear_account(&mut self) {
        self.display_name.clear();
        self.email.clear();
        self.quota_used = 0;
        self.quota_total = 0;
    }
}

/// Shared, observable account state.
#[derive(Clone)]
pub struct StateHandle {
    tx: Arc<watch::Sender<AccountSnapshot>>,
}

impl StateHandle {
    pub fn new(initial: AccountSnapshot) -> Self {
        let (tx, _rx) = watch::channel(initial);
        Self { tx: Arc::new(tx) }
    }

    pub fn get(&self) -> AccountSnapshot {
        self.tx.borrow().clone()
    }

    pub fn update(&self, change: impl FnOnce(&mut AccountSnapshot)) {
        self.tx.send_modify(change);
    }

    /// Atomically moves from `from` to `to`; returns false (and changes nothing) otherwise.
    pub fn try_transition(&self, from: SignInState, to: SignInState) -> bool {
        self.tx.send_if_modified(|s| {
            if s.state == from {
                s.state = to;
                true
            } else {
                false
            }
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<AccountSnapshot> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
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
}
