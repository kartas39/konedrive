//! The observable account state; the D-Bus layer turns changes into PropertiesChanged.

use std::sync::Arc;

use tokio::sync::watch;

use crate::config::Mode;

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
    /// `Account1.Label`: what the account is called here, as `config.toml` keeps it.
    pub label: String,
    pub display_name: String,
    pub email: String,
    pub quota_used: u64,
    pub quota_total: u64,
    /// `Account1.Mode`: the mode the account runs in (`docs/design/writes.md` §2) — read-write only
    /// while `config.toml` says so, the gate lets its drive through, and `granted_scopes`
    /// carries `Files.ReadWrite`. The account's folder follows it
    /// (`crate::sync::write_mode::follow`).
    pub mode: Mode,
    /// What the account's last token may be used for: the token response's `scope` (or what
    /// was asked for, when it said nothing), never more than was asked for — see
    /// `wider_grant`. Empty until one came, and after a sign-out.
    pub granted_scopes: String,
    /// What the last token was valid for when that was more than a read-only request asked
    /// for: consent Microsoft still holds (limitations log F66). Such a token is used to read
    /// only, `Dev1` hands it out to nobody, and `LastError` says so. Empty otherwise.
    pub wider_grant: String,
    /// The drive the account's token was last seen to reach (`GET /me/drive` at a sign-in,
    /// at `RefreshAccountInfo`, or for `Dev1.ReadWriteAccessToken`). The account is
    /// read-write only while it is the drive `config.toml` records.
    pub live_drive: String,
}

impl Default for AccountSnapshot {
    fn default() -> Self {
        Self {
            state: SignInState::SignedOut,
            last_error: String::new(),
            client_id: String::new(),
            label: String::new(),
            display_name: String::new(),
            email: String::new(),
            quota_used: 0,
            quota_total: 0,
            mode: Mode::ReadOnly,
            granted_scopes: String::new(),
            wider_grant: String::new(),
            live_drive: String::new(),
        }
    }
}

impl AccountSnapshot {
    /// What goes when the account is no longer signed in: its name and quota, and with its
    /// token what the token allowed — so it runs read-only until it signs in again.
    pub fn clear_account(&mut self) {
        self.display_name.clear();
        self.email.clear();
        self.quota_used = 0;
        self.quota_total = 0;
        self.granted_scopes.clear();
        self.wider_grant.clear();
        self.live_drive.clear();
        self.mode = Mode::ReadOnly;
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
