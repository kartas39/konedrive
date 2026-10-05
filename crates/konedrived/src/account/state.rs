//! The observable account state; the D-Bus layer turns changes into PropertiesChanged.

use std::sync::Arc;

use tokio::sync::watch;

use crate::account::quota::QuotaFigures;
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
    /// What last went wrong: a sign-in, a refresh, a switch. Written through
    /// [`set_error`](Self::set_error) and [`clear_error`](Self::clear_error); `LastError` is
    /// [`published_error`](Self::published_error).
    pub last_error: String,
    /// Why the account runs read-only against `config.toml`, or holds more than it asked
    /// for: `recompute_mode`'s alone to set and to take back. It stands in `LastError`
    /// where an error would, so one takes the other's place.
    pub mode_note: Option<ModeNote>,
    pub client_id: String,
    /// `Account.Label`: what the account is called here, as `config.toml` keeps it.
    pub label: String,
    pub display_name: String,
    pub email: String,
    /// The account's one quota (`crate::account::quota`); 0 and empty until read.
    pub quota: QuotaFigures,
    /// `Account.Mode`: the mode the account runs in (`docs/design/writes.md` §2) — read-write only
    /// while `config.toml` says so, the gate lets its drive through, and `granted_scopes`
    /// carries `Files.ReadWrite`. The account's folder follows it
    /// (`crate::sync::mode::follow`).
    pub mode: Mode,
    /// What the account's last token may be used for: the token response's `scope` (or what
    /// was asked for, when it said nothing), never more than was asked for — see
    /// `wider_grant`. Empty until one came, and after a sign-out.
    pub granted_scopes: String,
    /// What the last token was valid for when that was more than a read-only request asked
    /// for: consent Microsoft still holds (limitations log F66). Such a token is used to read
    /// only, `TokenExport` hands it out to nobody, and `LastError` says so. Empty otherwise.
    pub wider_grant: String,
    /// The drive the account's token was last seen to reach (`GET /me/drive` at a sign-in,
    /// at `RefreshInfo`, or for `TokenExport.ReadWrite`). The account is
    /// read-write only while it is the drive `config.toml` records.
    pub live_drive: String,
}

impl Default for AccountSnapshot {
    fn default() -> Self {
        Self {
            state: SignInState::SignedOut,
            last_error: String::new(),
            mode_note: None,
            client_id: String::new(),
            label: String::new(),
            display_name: String::new(),
            email: String::new(),
            quota: QuotaFigures::default(),
            mode: Mode::ReadOnly,
            granted_scopes: String::new(),
            wider_grant: String::new(),
            live_drive: String::new(),
        }
    }
}

impl AccountSnapshot {
    /// `LastError` as published: what went wrong, or with nothing wrong the mode's note.
    pub fn published_error(&self) -> String {
        match &self.mode_note {
            Some(note) if self.last_error.is_empty() => note.text(),
            _ => self.last_error.clone(),
        }
    }

    /// Says what went wrong, in place of whatever `LastError` said, the mode's note included.
    pub fn set_error(&mut self, message: impl Into<String>) {
        self.last_error = message.into();
        self.mode_note = None;
    }

    /// Empties `LastError`: the error and the mode's note.
    pub fn clear_error(&mut self) {
        self.last_error.clear();
        self.mode_note = None;
    }

    /// Sets the mode's note in place of whatever `LastError` said, or takes it back; an
    /// error said since it was set stays.
    pub fn set_mode_note(&mut self, note: Option<ModeNote>) {
        if note.is_some() {
            self.last_error.clear();
        }
        self.mode_note = note;
    }

    /// What goes when the account is no longer signed in: its name and quota, and with its
    /// token what the token allowed — so it runs read-only until it signs in again.
    pub fn clear_account(&mut self) {
        self.display_name.clear();
        self.email.clear();
        self.quota = QuotaFigures::default();
        self.granted_scopes.clear();
        self.wider_grant.clear();
        self.live_drive.clear();
        self.mode = Mode::ReadOnly;
    }
}

/// Why an account runs read-only against `config.toml`, or holds more than it asked for
/// (`AccountService::recompute_mode`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeNote {
    /// A read-only request was answered with a token that can change files; what it was
    /// granted.
    WiderGrant(String),
    /// `config.toml` cannot be read now.
    ConfigUnreadable,
    /// `config.toml` says read-write, and the gate does not let the drive through.
    GateKeepsReadOnly,
    /// The sign-in reaches another drive than `config.toml` records: the one it reaches,
    /// and the one recorded.
    DriveMismatch { live: String, recorded: String },
    /// The token does not carry `Files.ReadWrite`.
    SignInToWrite,
    /// Which drive the sign-in reaches has not been seen yet.
    DriveNotSeen,
}

impl ModeNote {
    /// The note as `LastError` says it.
    pub fn text(&self) -> String {
        use super::{CONFIG_UNREADABLE, DRIVE_MISMATCH, DRIVE_NOT_SEEN, GATE_KEEPS_READ_ONLY, SIGN_IN_TO_WRITE, WIDER_GRANT};
        match self {
            Self::WiderGrant(granted) => format!(
                "{WIDER_GRANT} also change files ({granted}); konedrive uses it to read only. The consent \
                 stays with Microsoft until it is revoked at https://account.live.com/consent/Manage"
            ),
            Self::ConfigUnreadable => CONFIG_UNREADABLE.to_owned(),
            Self::GateKeepsReadOnly => GATE_KEEPS_READ_ONLY.to_owned(),
            Self::DriveMismatch { live, recorded } => format!(
                "{DRIVE_MISMATCH} {live}, but config.toml records drive {recorded} for it; it runs read-only \
                 until the two agree"
            ),
            Self::SignInToWrite => SIGN_IN_TO_WRITE.to_owned(),
            Self::DriveNotSeen => DRIVE_NOT_SEEN.to_owned(),
        }
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

/// What a failed token refresh does to the account's state.
impl konedrive_graph::token::RefreshReport for StateHandle {
    fn failed(&self, message: &str) {
        self.update(|s| s.set_error(message));
    }

    fn signed_out(&self, message: &str) {
        self.update(|s| {
            s.state = SignInState::SignedOut;
            s.set_error(message);
            s.clear_account();
        });
    }
}

#[cfg(test)]
mod tests;
