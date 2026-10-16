//! The state an account is in as a whole, and the reason for it (`Folder.Overall`), with
//! the two sentences published next to it (`Folder.Trouble`, `Folder.NotUpdated`): decided
//! here, in one function over what the account and its folder publish, so that no client
//! works it out from the properties or reads the text of `LastError` to decide anything.
//! The spellings are `konedrive_dbus::overall`'s.
//!
//! The one state no daemon can say is "the service is not running": a client shows that
//! by itself.

use konedrive_dbus::overall::Reason;

use crate::account::state::SignInState;
use crate::status::snapshot::{published_error, published_state, running_trouble, SyncSnapshot};
use crate::status::sync_trouble::TroubleKind;

/// What [`overall`] decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Overall {
    /// Why the account is in its state; the state is the reason's ([`Reason::state`]).
    pub(crate) reason: Reason,
    /// `Folder.Trouble` ([`trouble`]): the trouble there is now, whatever the reason is.
    pub(crate) trouble: String,
    /// `Folder.NotUpdated`: always empty. A file that changed in OneDrive has its downloaded
    /// copy removed, and is read from OneDrive again: no update can fail and wait.
    pub(crate) not_updated: String,
}

/// The overall state of an account whose sign-in stands at `sign_in`, whose folder
/// publishes `s`, and which has `downloads` downloads under way, with the two sentences of
/// what is wrong. The sentences are the folder's alone: they are there under every reason.
pub fn overall(sign_in: SignInState, s: &SyncSnapshot, downloads: usize) -> Overall {
    Overall {
        reason: reason(sign_in, s, downloads),
        trouble: trouble(s),
        not_updated: String::new(),
    }
}


/// `Folder.Trouble`: the sentence of the trouble there is now, or empty.
///
/// - A folder that has stopped on an error: everything `LastError` says.
/// - A folder that runs: its problems that do not stop it, out of reach included
///   ([`running_trouble`]).
/// - While the first listing runs, and for a folder that is not up or not there: nothing,
///   as [`reason`] counts no trouble then.
pub fn trouble(s: &SyncSnapshot) -> String {
    match published_state(s) {
        "error" => published_error(s),
        "ready" => running_trouble(s).map(|(text, _)| text).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Why the account is in its state: the first rule that holds decides, in the order
/// written. Nothing is read from a sentence: every rule is a fact of the snapshot.
pub fn reason(sign_in: SignInState, s: &SyncSnapshot, downloads: usize) -> Reason {
    match sign_in {
        SignInState::SigningIn => return Reason::SigningIn,
        SignInState::SignedOut => return Reason::SignedOut,
        SignInState::SignedIn => {}
    }
    let state = published_state(s);
    if s.folder.root_path.is_empty() || state == "none" {
        return Reason::NoFolder;
    }
    // Recorded and not up yet, with nothing known to be wrong: calm. It reads `error` once
    // something is known to be wrong.
    if state == "waiting" {
        return Reason::Starting;
    }
    if state == "error" {
        return Reason::Stopped;
    }
    // What needs the user comes first.
    if s.local.conflict_count > 0 {
        return Reason::Conflicts;
    }
    if s.upload_queue.quota_full {
        return Reason::QuotaFull;
    }
    if s.upload_queue.too_big_count > 0 {
        return Reason::TooBig;
    }
    if s.upload_queue.blocked_count > 0 {
        return Reason::Blocked;
    }
    if s.pause.paused_until.is_some() {
        return Reason::Paused;
    }
    // The account's own hold ranks as the user's pause.
    if !s.pause.held_back.is_empty() {
        return Reason::HeldBack;
    }
    // OneDrive asked the account to wait: nothing is listed or moved until then, whatever
    // the account was doing, and what its uploads' hold says is this very wait.
    if s.transfers.throughput.retry_after > 0 {
        return Reason::Throttled;
    }
    // While the first listing runs, the trouble of a folder that keeps running is not
    // counted: the listing is what the account is doing.
    let trouble = if state == "listing" { None } else { running_trouble(s) };
    if let Some((_, kind)) = trouble {
        // Offline only when out of reach is all that is wrong: by the kind the trouble
        // carries, never by its sentence.
        return match kind {
            TroubleKind::Unreachable => Reason::Unreachable,
            TroubleKind::Other => Reason::Trouble,
        };
    }
    if state == "listing" {
        return Reason::Listing;
    }
    if downloads > 0 || !s.upload_queue.uploads.is_empty() || s.upload_queue.pending_count > 0 {
        return Reason::Transferring;
    }
    Reason::UpToDate
}

#[cfg(test)]
mod tests;
