//! The state an account is in as a whole, and the reason for it (`Folder.Overall`, with
//! `Folder.Trouble`): decided here, in one function over what the account and its folder
//! publish, so that no client works it out from the properties or reads the text of
//! `LastError` to decide anything. The spellings are `konedrive_dbus::overall`'s.
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
    /// `Folder.Trouble`: the sentence the reason is about, for [`Reason::Stopped`],
    /// [`Reason::Trouble`], [`Reason::Unreachable`] and [`Reason::NotUpdated`]; empty for
    /// every other.
    pub trouble: String,
}

impl Overall {
    fn of(reason: Reason) -> Self {
        Self { reason, trouble: String::new() }
    }

    fn about(reason: Reason, trouble: String) -> Self {
        Self { reason, trouble }
    }
}

/// The overall state of an account whose sign-in stands at `sign_in`, whose folder
/// publishes `s`, and which has `downloads` downloads under way.
///
/// The first rule that holds decides, in the order written. Nothing is read from a
/// sentence: every rule is a fact of the snapshot.
pub fn overall(sign_in: SignInState, s: &SyncSnapshot, downloads: usize) -> Overall {
    match sign_in {
        SignInState::SigningIn => return Overall::of(Reason::SigningIn),
        SignInState::SignedOut => return Overall::of(Reason::SignedOut),
        SignInState::SignedIn => {}
    }
    let state = published_state(s);
    if s.folder.root_path.is_empty() || state == "none" {
        return Overall::of(Reason::NoFolder);
    }
    // Recorded and not up yet, with nothing known to be wrong: calm. It reads `error` once
    // something is known to be wrong.
    if state == "waiting" {
        return Overall::of(Reason::Starting);
    }
    // Everything `LastError` says is what the folder stopped on.
    if state == "error" {
        return Overall::about(Reason::Stopped, published_error(s));
    }
    // What needs the user comes first, the mass-delete guard before all: only the user can
    // decide.
    if s.outbox.held_count > 0 {
        return Overall::of(Reason::DeletesHeld);
    }
    if s.local.conflict_count > 0 {
        return Overall::of(Reason::Conflicts);
    }
    if s.outbox.quota_full {
        return Overall::of(Reason::QuotaFull);
    }
    if s.outbox.too_big_count > 0 {
        return Overall::of(Reason::TooBig);
    }
    if s.outbox.blocked_count > 0 {
        return Overall::of(Reason::Blocked);
    }
    if let Some(note) = &s.cycle.replacement_note {
        return Overall::about(Reason::NotUpdated, ReplacementNote::text(note));
    }
    // The helper serves every account: its trouble counts against each account whose
    // folder it intercepts, not one registered without interception, where nothing
    // downloads on open whatever the helper does. A warning and not offline: the folder
    // itself may be fine.
    if s.folder.helper_state != HelperState::Connected && state != "no-interception" {
        return Overall::of(Reason::HelperUnavailable);
    }
    if s.pause.paused_until.is_some() {
        return Overall::of(Reason::Paused);
    }
    // The account's own hold ranks as the user's pause.
    if !s.pause.held_back.is_empty() {
        return Overall::of(Reason::HeldBack);
    }
    // While the first listing runs, the trouble of a folder that keeps running is not
    // counted: the listing is what the account is doing.
    let trouble = if state == "listing" { None } else { running_trouble(s) };
    if let Some((trouble, kind)) = trouble {
        // Offline only when out of reach is all that is wrong: by the kind the trouble
        // carries, never by its sentence.
        let reason = match kind {
            TroubleKind::Unreachable => Reason::Unreachable,
            TroubleKind::Other => Reason::Trouble,
        };
        return Overall::about(reason, trouble);
    }
    if state == "listing" {
        return Overall::of(Reason::Listing);
    }
    if downloads > 0 || !s.outbox.uploads.is_empty() || s.outbox.pending_count > 0 {
        return Overall::of(Reason::Transferring);
    }
    Overall::of(Reason::UpToDate)
}

#[cfg(test)]
mod tests;
