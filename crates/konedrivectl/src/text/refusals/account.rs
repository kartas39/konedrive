//! The sentences for a refused call on the accounts themselves, and on the token export.

use konedrive_dbus::{Refusal, LABEL_RULE};

use super::read_error;
use crate::text::formats::shell_word;

/// What to tell a person when `TokenExport.ReadOnly()` failed — decided by the
/// D-Bus error name, never the message, the same discipline
/// [`explain_sync_error`](super::explain_sync_error) uses for the folder. Being signed out is worth its
/// own sentence, since the fix (`konedrivectl login`) is not what the
/// daemon's own message says; a locked wallet or a network error already
/// says what is wrong on its own, so its text is kept as is.
pub fn explain_dev_error(error: &zbus::Error, prefix: &str) -> String {
    let (refusal, detail) = read_error(error);
    dev_text(refusal.as_ref(), &detail, prefix)
}

/// [`explain_dev_error`]'s decision, on the name and message alone — so it
/// can be tested with a name and a message that disagree, the same shape
/// [`refusal_text`](super::refusal_text) is tested with.
pub fn dev_refusal_text(name: Option<&str>, detail: &str, prefix: &str) -> String {
    dev_text(name.map(Refusal::parse).as_ref(), detail, prefix)
}

fn dev_text(refusal: Option<&Refusal>, detail: &str, prefix: &str) -> String {
    let other = || format!("cannot get an access token: {detail}");
    let Some(refusal) = refusal else { return other() };
    match refusal {
        Refusal::NotSignedIn => format!(
            "cannot get an access token: the account is not signed in. Are you signed in? \
             (`{prefix} status` says; `{prefix} login` signs in.)"
        ),
        Refusal::WritesNotAllowed => format!(
            "cannot get a read-write access token: while uploads are being developed, only the test \
             accounts listed in write_test_drive_ids in ~/.config/konedrive/config.toml can be \
             read-write, and this account is not one of them. {WITHOUT_READ_WRITE}"
        ),
        Refusal::ModeNotGranted => format!(
            "cannot get a read-write access token: the account is read-only. Switch it first: \
             `{prefix} account mode read-write`"
        ),
        Refusal::NotEmpty
        | Refusal::Unsupported
        | Refusal::InUse
        | Refusal::NoRoot
        | Refusal::NoHelper
        | Refusal::NotManaged
        | Refusal::NotHydrated
        | Refusal::ModifiedLocally
        | Refusal::OutsideRoot
        | Refusal::AlreadyRegistered
        | Refusal::NoSource
        | Refusal::NoConflict
        | Refusal::NotAllowed
        | Refusal::Overlaps
        | Refusal::NoAccount
        | Refusal::NotUploaded
        | Refusal::PendingUploads
        | Refusal::Unreachable
        | Refusal::Failed
        | Refusal::InvalidArgs
        | Refusal::BusFailed
        | Refusal::UnknownObject
        | Refusal::UnknownMethod
        | Refusal::UnknownInterface
        | Refusal::NotUp
        | Refusal::Internal
        | Refusal::Other(_) => other(),
    }
}

/// What a refusal of read-write adds: the export without `--read-write` still works.
const WITHOUT_READ_WRITE: &str = "Without --read-write, the export gives a read-only token.";

/// A call on the accounts themselves, for [`explain_account_error`]. (`Remove`
/// forgets a folder, and is a [`SyncAction`](super::SyncAction).)
#[derive(Debug, Clone, Copy)]
pub enum AccountAction<'a> {
    /// `Accounts.SignIn`: a new account, added by signing in.
    Add,
    /// A development build's `DevTools.AddAccount`, with the label asked for.
    AddNamed(&'a str),
    /// `Account.SetLabel`: the account's label, and the one asked for.
    Rename(&'a str, &'a str),
    /// `Accounts.SetClientId`: the id given, and the labels of the accounts
    /// signed in or signing in when it was refused.
    SetClientId(&'a str, &'a [String]),
    /// `Account.BeginSignIn`, with the account's label.
    SignIn(&'a str),
    /// `Account.SignOut`, with the account's label.
    SignOut(&'a str),
    /// `Account.SetMode`: the account's label, the mode asked for, and how a command
    /// suggested about the account starts ([`command_prefix`](crate::choice::command_prefix)).
    SetMode(&'a str, &'a str, &'a str),
    /// `Accounts.SetPauseOnMetered` or `SetOnBattery`: a setting every account shares.
    Settings,
}

/// What to tell a person when a call on the accounts failed: `Accounts.SignIn`,
/// `Accounts.SetClientId`, `Account.SetLabel`, `BeginSignIn` or `SignOut`.
/// These refuse under the bus's own names — `InvalidArgs` for a label or a
/// client id the rules refuse, `Failed` for the rest — so the name decides,
/// and the daemon's message is kept as the reason.
pub fn explain_account_error(action: AccountAction<'_>, error: &zbus::Error) -> String {
    let (refusal, detail) = read_error(error);
    account_text(action, refusal.as_ref(), &detail)
}

/// [`explain_account_error`]'s decision, on the name and message alone.
pub fn account_refusal_text(action: AccountAction<'_>, name: Option<&str>, detail: &str) -> String {
    account_text(action, name.map(Refusal::parse).as_ref(), detail)
}

fn account_text(action: AccountAction<'_>, refusal: Option<&Refusal>, detail: &str) -> String {
    use AccountAction::*;
    match action {
        SetMode(label, mode, prefix) => mode_text(label, mode, prefix, refusal, detail),
        AddNamed(label) | Rename(_, label) if refusal == Some(&Refusal::InvalidArgs) => {
            format!("{label:?} cannot be an account's label: {detail}. {LABEL_RULE}")
        }
        SetClientId(id, _) if refusal == Some(&Refusal::InvalidArgs) => format!(
            "{id:?} is not an Application (client) ID. It is a GUID like \
             00000000-0000-0000-0000-000000000000: copy it from the Overview page of your app \
             registration in the Microsoft Entra admin center"
        ),
        // The daemon refuses a change while any account uses the old id.
        SetClientId(_, busy) if refusal == Some(&Refusal::BusFailed) && !busy.is_empty() => {
            let (who, verb) = match busy {
                [one] => (one.clone(), "is"),
                several => (several.join(", "), "are"),
            };
            let sign_out: Vec<String> =
                busy.iter().map(|label| format!("`konedrivectl --account {} logout`", shell_word(label))).collect();
            format!(
                "the client ID was not changed: every account signs in with it, so it cannot change \
                 while {who} {verb} signed in or signing in. Sign out first: {}",
                sign_out.join(", ")
            )
        }
        SetClientId(..) => format!("the client ID was not saved: {detail}"),
        Add => format!("cannot start signing in, and no account was added: {detail}"),
        AddNamed(label) => format!("the account {label:?} was not added: {detail}"),
        Rename(old, new) => format!("the account {old} was not renamed to {new:?}: {detail}"),
        SignIn(label) => format!("cannot start signing in to {label}: {detail}"),
        SignOut(label) => format!("cannot sign {label} out: {detail}"),
        Settings => format!("the setting was not changed: {detail}"),
    }
}

/// `account mode`: `Account.SetMode` refuses under the daemon's own names.
fn mode_text(label: &str, mode: &str, prefix: &str, refusal: Option<&Refusal>, detail: &str) -> String {
    let other = || format!("{label} was not switched to {mode}: {detail}");
    let Some(refusal) = refusal else { return other() };
    match refusal {
        Refusal::WritesNotAllowed => format!(
            "{label} was not switched to read-write: while uploads are being developed, only the test \
             accounts listed in write_test_drive_ids in ~/.config/konedrive/config.toml can be, and \
             this account is not one of them. Nothing was changed"
        ),
        Refusal::NotSignedIn => format!(
            "{label} was not switched to read-write: it is not signed in. Sign in first: `{prefix} login`"
        ),
        Refusal::PendingUploads => format!(
            "{label} was not switched to read-only: {detail}. `{prefix} account mode read-only --force` \
             switches anyway"
        ),
        Refusal::NotEmpty
        | Refusal::Unsupported
        | Refusal::InUse
        | Refusal::NoRoot
        | Refusal::NoHelper
        | Refusal::NotManaged
        | Refusal::NotHydrated
        | Refusal::ModifiedLocally
        | Refusal::OutsideRoot
        | Refusal::AlreadyRegistered
        | Refusal::NoSource
        | Refusal::NoConflict
        | Refusal::NotAllowed
        | Refusal::Overlaps
        | Refusal::NoAccount
        | Refusal::NotUploaded
        | Refusal::Unreachable
        | Refusal::Failed
        | Refusal::ModeNotGranted
        | Refusal::InvalidArgs
        | Refusal::BusFailed
        | Refusal::UnknownObject
        | Refusal::UnknownMethod
        | Refusal::UnknownInterface
        | Refusal::NotUp
        | Refusal::Internal
        | Refusal::Other(_) => other(),
    }
}
