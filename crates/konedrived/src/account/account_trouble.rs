//! What went wrong for an account, as cases with their data: a sign-in in the browser
//! ([`SignInFailure`]), a sign-in refused for the account ([`Refused`]), a switch to
//! read-write ([`ReadWriteFailure`]), and what the account's `LastError` says
//! ([`AccountTrouble`]). Their sentences are `crate::sentences`'s.

use crate::config::{AccountId, ConfigError};

/// Why a sign-in in the browser gave no tokens, or why the tokens it gave are no sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInFailure {
    /// The user cancelled the wait: nothing is said.
    Cancelled,
    /// The loopback listener the browser answers on could not be bound.
    CannotListen { detail: String },
    /// The browser did not answer in time.
    BrowserTimedOut,
    /// The browser's answer could not be taken.
    Browser { detail: String },
    /// Access was denied in the browser.
    AccessDenied,
    /// Microsoft answered the browser with an error.
    MicrosoftRefused { error: String, description: String },
    /// The code could not be exchanged for tokens.
    CodeNotExchanged { detail: String },
    /// The token response carries no refresh token.
    NoRefreshToken,
    /// Graph did not say which drive the token is for: why.
    Unidentified { detail: String },
}

/// Why a sign-in whose tokens came is refused for the account it was for, or for a new
/// account (`docs/design/accounts.md` §6.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The browser gave no sign-in.
    Failed(SignInFailure),
    /// Another account has the drive: that account.
    AlreadyConnected { id: AccountId, label: String },
    /// Another account may be this drive, and cannot be told apart from it yet.
    NotToldApart,
    /// The account was removed meanwhile.
    Removed,
    /// The account is `who`, and the browser signed in as another Microsoft account.
    OtherMicrosoftAccount { who: String },
    /// The wallet did not take the refresh token.
    NotStored { detail: String },
    /// `config.toml` could not be read or written.
    NotSaved { detail: String },
    /// A new account could not be made.
    NotMade { detail: String },
    /// A new account could not be put on the bus.
    NotOnBus { detail: String },
}

impl From<ConfigError> for Refused {
    fn from(error: ConfigError) -> Self {
        Refused::NotSaved { detail: error.to_string() }
    }
}

/// Why a switch to read-write did not go through: the account stays read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadWriteFailure {
    /// The browser gave no sign-in.
    Failed(SignInFailure),
    /// The sign-in does not allow changes: what it granted.
    NotGranted { granted: String },
    NoRefreshToken,
    /// Which account the token is for could not be checked: why.
    Unidentified { detail: String },
    /// The account was removed meanwhile.
    Removed,
    /// `config.toml` records no drive for the account, and Graph does not say which it is.
    DriveNotKnown,
    /// The account is `who`, and the browser signed in as another Microsoft account.
    OtherMicrosoftAccount { who: String },
    /// `config.toml` cannot be read now.
    ConfigUnreadable,
    /// The wallet did not take the refresh token.
    NotStored { detail: String },
    /// `config.toml` could not be written.
    NotSaved { detail: String },
}

impl From<ConfigError> for ReadWriteFailure {
    fn from(error: ConfigError) -> Self {
        ReadWriteFailure::NotSaved { detail: error.to_string() }
    }
}

/// What last went wrong for the account: what its `LastError` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountTrouble {
    /// A sign-in that did not go through.
    SignIn(Refused),
    /// A switch to read-write that did not go through.
    ReadWrite(ReadWriteFailure),
    /// The account's name and quota could not be read: why.
    ProfileNotRead { detail: String },
    /// Nobody can sign in: no client ID is set.
    NoClientId,
    /// The stored sign-in is gone.
    StoredSignInLost,
    /// What the token's refresh or the wallet said, as it said it.
    Token { detail: String },
}
