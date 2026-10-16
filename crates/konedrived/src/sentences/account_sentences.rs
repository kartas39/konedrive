//! What is said of an account's trouble (`crate::account::account_trouble`) and of why it
//! runs read-only against `config.toml` (`ModeNote`), in `LastError`, in the browser and
//! in the upload queue's note.

use std::fmt;

use crate::account::account_trouble::{AccountTrouble, ReadWriteFailure, Refused, SignInFailure};
use crate::account::state::ModeNote;
use crate::account::DRIVE_NOT_KNOWN;

/// What the browser's sign-in said that is not the user's own: the tail of a sentence that
/// says the account stays read-only.
const STAYS_READ_ONLY: &str = "the account stays read-only.";

impl fmt::Display for SignInFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SignInFailure::Cancelled => Ok(()),
            SignInFailure::CannotListen { detail } => write!(f, "cannot listen on localhost: {detail}"),
            SignInFailure::BrowserTimedOut => write!(
                f,
                "Timed out waiting for the browser. If Microsoft showed an error page, check the app registration: \
                 client ID and redirect URI http://localhost."
            ),
            SignInFailure::Browser { detail } => write!(f, "{detail}"),
            SignInFailure::AccessDenied => write!(f, "Access was denied in the browser."),
            SignInFailure::MicrosoftRefused { error, description } => write!(f, "Microsoft reported an error: {error}: {description}"),
            SignInFailure::CodeNotExchanged { detail } => write!(f, "Sign-in failed: {detail}"),
            SignInFailure::NoRefreshToken => write!(f, "Microsoft did not return a refresh token."),
            SignInFailure::Unidentified { detail } => write!(f, "Could not check which account this is ({detail}); try again."),
        }
    }
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Failed(failure) => failure.fmt(f),
            Refused::AlreadyConnected { label, .. } => write!(f, "This Microsoft account is already connected as '{label}'."),
            Refused::NotToldApart => write!(f, "Could not check which account this is; try again."),
            Refused::Removed => write!(f, "This account was removed."),
            Refused::OtherMicrosoftAccount { who } => write!(
                f,
                "This account is {who}. You signed in as a different Microsoft account; to connect that one, add a new account."
            ),
            Refused::NotStored { detail } | Refused::NotSaved { detail } | Refused::NotMade { detail } => write!(f, "{detail}"),
            Refused::NotOnBus { detail } => write!(f, "cannot put the account on the bus: {detail}"),
        }
    }
}

impl fmt::Display for ReadWriteFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadWriteFailure::Failed(failure) => failure.fmt(f),
            ReadWriteFailure::NotGranted { granted } => write!(
                f,
                "Microsoft did not allow konedrive to change files in OneDrive (the sign-in granted {granted:?}); {STAYS_READ_ONLY}"
            ),
            ReadWriteFailure::NoRefreshToken => write!(f, "Microsoft did not return a refresh token; {STAYS_READ_ONLY}"),
            ReadWriteFailure::Unidentified { detail } => write!(f, "Could not check which account this is ({detail}); {STAYS_READ_ONLY}"),
            ReadWriteFailure::Removed => write!(f, "This account was removed."),
            ReadWriteFailure::DriveNotKnown => write!(f, "{DRIVE_NOT_KNOWN}; {STAYS_READ_ONLY}"),
            ReadWriteFailure::OtherMicrosoftAccount { who } => {
                write!(f, "This account is {who}, and the browser signed in as a different Microsoft account; {STAYS_READ_ONLY}")
            }
            ReadWriteFailure::ConfigUnreadable => {
                write!(f, "config.toml cannot be read now, so the switch to read-write was not saved; {STAYS_READ_ONLY}")
            }
            ReadWriteFailure::NotStored { detail } => write!(f, "{detail}; {STAYS_READ_ONLY}"),
            ReadWriteFailure::NotSaved { detail } => write!(f, "{detail}"),
        }
    }
}

impl fmt::Display for AccountTrouble {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccountTrouble::SignIn(refused) => refused.fmt(f),
            AccountTrouble::ReadWrite(failure) => failure.fmt(f),
            AccountTrouble::ProfileNotRead { detail } => write!(f, "Could not load account info: {detail}"),
            AccountTrouble::NoClientId => write!(f, "Set a client ID first, then sign in again."),
            AccountTrouble::StoredSignInLost => write!(f, "The stored sign-in was lost. Sign in again."),
            AccountTrouble::Token { detail } => write!(f, "{detail}"),
        }
    }
}

/// `LastError` of an account that runs read-only against what `config.toml` says.
impl fmt::Display for ModeNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModeNote::WiderGrant(granted) => write!(
                f,
                "Microsoft answered a request for read-only access with a token that can also change files ({granted}); \
                 konedrive uses it to read only. The consent stays with Microsoft until it is revoked at \
                 https://account.live.com/consent/Manage"
            ),
            ModeNote::ConfigUnreadable => write!(f, "config.toml cannot be read now, so this account runs read-only until it can"),
            ModeNote::NoDriveRecorded => write!(
                f,
                "config.toml sets this account to read-write, but records no OneDrive drive for it; it runs read-only until \
                 a sign-in records one"
            ),
            ModeNote::DriveMismatch { live, recorded } => write!(
                f,
                "this account's sign-in reaches the OneDrive drive {live}, but config.toml records drive {recorded} for it; \
                 it runs read-only until the two agree"
            ),
            ModeNote::SignInToWrite => write!(
                f,
                "Changes made here are not uploaded: this account's sign-in does not allow konedrive to change files in \
                 OneDrive. Switch it to read-write again to sign in with that permission."
            ),
            ModeNote::DriveNotSeen => write!(
                f,
                "config.toml sets this account to read-write, but which OneDrive its sign-in reaches has not been checked \
                 yet; it runs read-only until it is"
            ),
        }
    }
}

/// Why the account may not write, as the upload queue's note says it after "nothing is
/// uploaded: ".
pub(super) fn say_writes_refusal(note: &ModeNote) -> String {
    match note {
        ModeNote::ConfigUnreadable => "config.toml cannot be read".to_owned(),
        ModeNote::NoDriveRecorded => "config.toml records no drive for the account".to_owned(),
        ModeNote::DriveNotSeen => "which OneDrive drive the account's sign-in reaches has not been seen yet".to_owned(),
        ModeNote::DriveMismatch { live, recorded } => {
            format!("the account's token was last seen to reach drive {live:?}, not drive {recorded:?}, which config.toml records")
        }
        ModeNote::SignInToWrite => "the account's sign-in does not allow changes".to_owned(),
        // Not a refusal: a read-only account's folder is held as read-only.
        ModeNote::WiderGrant(granted) => format!("the account's token can also change files ({granted}), and is used to read only"),
    }
}

#[cfg(test)]
mod tests;
