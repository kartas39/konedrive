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
use crate::helper::status::HelperState;
use crate::status::snapshot::{published_error, published_state, running_trouble, ReplacementNote, SyncSnapshot, TroubleKind};

/// What [`overall`] decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overall {
    /// Why the account is in its state; the state is the reason's ([`Reason::state`]).
    pub reason: Reason,
    /// `Folder.Trouble` ([`trouble`]): the trouble there is now, whatever the reason is.
    pub trouble: String,
    /// `Folder.NotUpdated`: the failed-update note alone, whenever there is one.
    pub not_updated: String,
}

/// The overall state of an account whose sign-in stands at `sign_in`, whose folder
/// publishes `s`, and which has `downloads` downloads under way, with the two sentences of
/// what is wrong. The sentences are the folder's alone: they are there under every reason.
pub fn overall(sign_in: SignInState, s: &SyncSnapshot, downloads: usize) -> Overall {
    Overall {
        reason: reason(sign_in, s, downloads),
        trouble: trouble(s),
        not_updated: not_updated(s),
    }
}

/// `Folder.NotUpdated`: the failed-update note alone, whenever there is one; or empty.
pub fn not_updated(s: &SyncSnapshot) -> String {
    s.cycle.replacement_note.as_ref().map(ReplacementNote::text).unwrap_or_default()
}

/// `Folder.Trouble`: the sentence of the trouble there is now, or empty.
///
/// - A folder that has stopped on an error: everything `LastError` says.
/// - A folder that runs: its problems that do not stop it, out of reach included
///   ([`running_trouble`]), without the no-interception warning and the failed-update note.
/// - While the first listing runs, and for a folder that is not up or not there: nothing,
///   as [`reason`] counts no trouble then.
pub fn trouble(s: &SyncSnapshot) -> String {
    match published_state(s) {
        "error" => published_error(s),
        "ready" | "no-interception" => running_trouble(s).map(|(text, _)| text).unwrap_or_default(),
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
    // What needs the user comes first, the mass-delete guard before all: only the user can
    // decide.
    if s.outbox.held_count > 0 {
        return Reason::DeletesHeld;
    }
    if s.local.conflict_count > 0 {
        return Reason::Conflicts;
    }
    if s.outbox.quota_full {
        return Reason::QuotaFull;
    }
    if s.outbox.too_big_count > 0 {
        return Reason::TooBig;
    }
    if s.outbox.blocked_count > 0 {
        return Reason::Blocked;
    }
    if s.cycle.replacement_note.is_some() {
        return Reason::NotUpdated;
    }
    // The helper serves every account: its trouble counts against each account whose
    // folder it intercepts, not one registered without interception, where nothing
    // downloads on open whatever the helper does. A warning and not offline: the folder
    // itself may be fine.
    if s.folder.helper_state != HelperState::Connected && state != "no-interception" {
        return Reason::HelperUnavailable;
    }
    if s.pause.paused_until.is_some() {
        return Reason::Paused;
    }
    // The account's own hold ranks as the user's pause.
    if !s.pause.held_back.is_empty() {
        return Reason::HeldBack;
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
    if downloads > 0 || !s.outbox.uploads.is_empty() || s.outbox.pending_count > 0 {
        return Reason::Transferring;
    }
    Reason::UpToDate
}

#[cfg(test)]
mod tests;
