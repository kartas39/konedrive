//! `Folder.Overall`: the state an account is in as a whole, and the reason for it — the
//! spellings, as the daemon publishes them and as a client reads them. The daemon decides
//! both (`konedrived`'s `status::overall`); a client turns the reason into words and an
//! icon, and decides nothing.

use serde::{Deserialize, Serialize};
use zbus::zvariant::{OwnedValue, Type, Value};

/// The state of an account as a whole: what a tray icon shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Ok,
    Syncing,
    Warning,
    Paused,
    Offline,
}

impl State {
    /// Every state.
    pub const ALL: [State; 5] = [Self::Ok, Self::Syncing, Self::Warning, Self::Paused, Self::Offline];

    /// The state as the property spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Syncing => "syncing",
            Self::Warning => "warning",
            Self::Paused => "paused",
            Self::Offline => "offline",
        }
    }

    /// The state the property's value `text` spells; `None` for one this build does not know.
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == text)
    }
}

/// Why an account is in its [`State`]. Each reason belongs to one state ([`Reason::state`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The account is signing in.
    SigningIn,
    /// The account is not signed in.
    SignedOut,
    /// No folder is registered.
    NoFolder,
    /// OneDrive cannot be reached, and nothing else is wrong; `Folder.Trouble` says it.
    Unreachable,
    /// The folder is recorded and not up yet, with nothing known to be wrong.
    Starting,
    /// The first listing of the drive is running.
    Listing,
    /// Files are downloading or uploading, or changes wait to upload.
    Transferring,
    /// The folder's syncing has stopped on an error; `Folder.Trouble` says it.
    Stopped,
    /// Deletions wait for the user's decision.
    DeletesHeld,
    /// Changed files were moved out of the way.
    Conflicts,
    /// OneDrive is full and files wait for space.
    QuotaFull,
    /// Files are too big for the space left.
    TooBig,
    /// Changes cannot be uploaded.
    Blocked,
    /// Files changed in OneDrive could not be updated here yet; `Folder.Trouble` says it.
    NotUpdated,
    /// The helper is in trouble, for a folder it intercepts.
    HelperUnavailable,
    /// Any other trouble that does not stop the folder; `Folder.Trouble` says it.
    Trouble,
    /// The user paused the account.
    Paused,
    /// The account holds back by itself (metered, battery, power-saver).
    HeldBack,
    /// None of the others.
    UpToDate,
}

impl Reason {
    /// Every reason.
    pub const ALL: [Reason; 19] = [
        Self::SigningIn,
        Self::SignedOut,
        Self::NoFolder,
        Self::Unreachable,
        Self::Starting,
        Self::Listing,
        Self::Transferring,
        Self::Stopped,
        Self::DeletesHeld,
        Self::Conflicts,
        Self::QuotaFull,
        Self::TooBig,
        Self::Blocked,
        Self::NotUpdated,
        Self::HelperUnavailable,
        Self::Trouble,
        Self::Paused,
        Self::HeldBack,
        Self::UpToDate,
    ];

    /// The reason as the property spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SigningIn => "signing-in",
            Self::SignedOut => "signed-out",
            Self::NoFolder => "no-folder",
            Self::Unreachable => "unreachable",
            Self::Starting => "starting",
            Self::Listing => "listing",
            Self::Transferring => "transferring",
            Self::Stopped => "stopped",
            Self::DeletesHeld => "deletes-held",
            Self::Conflicts => "conflicts",
            Self::QuotaFull => "quota-full",
            Self::TooBig => "too-big",
            Self::Blocked => "blocked",
            Self::NotUpdated => "not-updated",
            Self::HelperUnavailable => "helper-unavailable",
            Self::Trouble => "trouble",
            Self::Paused => "paused",
            Self::HeldBack => "held-back",
            Self::UpToDate => "up-to-date",
        }
    }

    /// The reason the property's value `text` spells; `None` for one this build does not know.
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|reason| reason.as_str() == text)
    }

    /// The state an account is in for this reason.
    pub fn state(self) -> State {
        match self {
            Self::SigningIn | Self::SignedOut | Self::NoFolder | Self::Unreachable => State::Offline,
            Self::Starting | Self::Listing | Self::Transferring => State::Syncing,
            Self::Stopped
            | Self::DeletesHeld
            | Self::Conflicts
            | Self::QuotaFull
            | Self::TooBig
            | Self::Blocked
            | Self::NotUpdated
            | Self::HelperUnavailable
            | Self::Trouble => State::Warning,
            Self::Paused | Self::HeldBack => State::Paused,
            Self::UpToDate => State::Ok,
        }
    }

    /// Whether `Folder.Trouble` carries the sentence this reason is about.
    pub fn has_sentence(self) -> bool {
        matches!(self, Self::Stopped | Self::Trouble | Self::Unreachable | Self::NotUpdated)
    }
}

/// `Folder.Overall` as it is on the bus, `(ss)`: the state and the reason, spelled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type, Value, OwnedValue)]
pub struct Overall {
    /// One of [`State`]'s spellings.
    pub state: String,
    /// One of [`Reason`]'s spellings.
    pub reason: String,
}

impl From<Reason> for Overall {
    fn from(reason: Reason) -> Self {
        Self { state: reason.state().as_str().to_owned(), reason: reason.as_str().to_owned() }
    }
}

#[cfg(test)]
mod tests;
