use konedrive_dbus::overall::State;

use super::*;
use crate::status::snapshot::{OutboxNote, RootState, SwitchNote, SyncTrouble, NO_INTERCEPTION_WARNING};

/// A folder that is up, intercepted and at rest, its helper connected: `up-to-date` for an
/// account that is signed in.
fn healthy() -> SyncSnapshot {
    let mut s = SyncSnapshot::default();
    s.folder.root_path = "/home/u/OneDrive".into();
    s.folder.root_state = RootState::Ready;
    s.folder.helper_state = HelperState::Connected;
    s
}

/// What a signed-in account with nothing downloading is, its folder publishing `s`.
fn of(s: &SyncSnapshot) -> Overall {
    overall(SignInState::SignedIn, s, 0)
}

/// [`healthy`] after `change`, decided.
fn with(change: impl FnOnce(&mut SyncSnapshot)) -> Overall {
    let mut s = healthy();
    change(&mut s);
    of(&s)
}

fn unreachable(text: &str) -> Option<SyncTrouble> {
    Some(SyncTrouble { text: text.into(), blocking: false, kind: TroubleKind::Unreachable })
}

fn other(text: &str, blocking: bool) -> Option<SyncTrouble> {
    Some(SyncTrouble { text: text.into(), blocking, kind: TroubleKind::Other })
}

/// A reason with no sentence.
fn plain(reason: Reason) -> Overall {
    Overall { reason, trouble: String::new() }
}

fn about(reason: Reason, trouble: &str) -> Overall {
    Overall { reason, trouble: trouble.into() }
}

// --- One test for each row of the table ---

#[test]
fn an_account_signing_in_is_offline() {
    // Whatever its folder says.
    let mut s = healthy();
    s.outbox.held_count = 3;
    assert_eq!(overall(SignInState::SigningIn, &s, 1), plain(Reason::SigningIn));
    assert_eq!(Reason::SigningIn.state(), State::Offline);
}

#[test]
fn an_account_not_signed_in_is_offline() {
    let mut s = healthy();
    s.cycle.sync_trouble = other("signed out", true);
    assert_eq!(overall(SignInState::SignedOut, &s, 0), plain(Reason::SignedOut));
    assert_eq!(Reason::SignedOut.state(), State::Offline);
}

#[test]
fn an_account_with_no_folder_is_offline() {
    assert_eq!(of(&SyncSnapshot::default()), plain(Reason::NoFolder));
    // A path with nothing registered at it yet (the daemon has only just started), and a
    // state with no path, are no folder either.
    assert_eq!(with(|s| s.folder.root_state = RootState::None), plain(Reason::NoFolder));
    assert_eq!(with(|s| s.folder.root_path.clear()), plain(Reason::NoFolder));
    assert_eq!(Reason::NoFolder.state(), State::Offline);
}

#[test]
fn an_account_that_cannot_reach_onedrive_is_offline_and_says_the_trouble() {
    let said = "cannot reach OneDrive (timed out); trying again";
    assert_eq!(with(|s| s.cycle.sync_trouble = unreachable(said)), about(Reason::Unreachable, said));
    assert_eq!(Reason::Unreachable.state(), State::Offline);
}

#[test]
fn a_folder_not_up_yet_is_starting() {
    let waiting = with(|s| {
        s.folder.root_state = RootState::Waiting;
        s.folder.waits_for_helper = true;
        s.folder.helper_state = HelperState::Unknown;
    });
    assert_eq!(waiting, plain(Reason::Starting), "calm: nothing is known to be wrong");
    assert_eq!(Reason::Starting.state(), State::Syncing);
}

#[test]
fn the_first_listing_is_syncing() {
    assert_eq!(with(|s| s.cycle.listing = true), plain(Reason::Listing));
    assert_eq!(Reason::Listing.state(), State::Syncing);
}

#[test]
fn anything_moving_or_waiting_to_upload_is_transferring() {
    assert_eq!(overall(SignInState::SignedIn, &healthy(), 1), plain(Reason::Transferring), "a download");
    assert_eq!(with(|s| s.outbox.uploads = vec![("/home/u/OneDrive/a".into(), 1, 2)]), plain(Reason::Transferring), "an upload");
    assert_eq!(with(|s| s.outbox.pending_count = 1), plain(Reason::Transferring), "a change waiting to upload");
    assert_eq!(Reason::Transferring.state(), State::Syncing);
}

#[test]
fn a_folder_stopped_on_an_error_says_everything_last_error_says() {
    let blocked = with(|s| s.cycle.sync_trouble = other("signed out: sign in again", true));
    assert_eq!(blocked, about(Reason::Stopped, "signed out: sign in again"));

    // The helper went away under a folder it intercepts: the helper's advice, then the rest.
    let mut s = healthy();
    s.folder.waits_for_helper = true;
    s.folder.helper_state = HelperState::Stopped;
    s.folder.last_error = "recovery left 1 file".into();
    let stopped = of(&s);
    assert_eq!(stopped.reason, Reason::Stopped);
    assert_eq!(stopped.trouble, published_error(&s));
    assert!(stopped.trouble.ends_with(". recovery left 1 file"), "{}", stopped.trouble);

    // A registration that failed, with nothing more to say than its state.
    assert_eq!(with(|s| s.folder.root_state = RootState::Error), plain(Reason::Stopped));
    assert_eq!(Reason::Stopped.state(), State::Warning);
}

#[test]
fn deletions_held_for_the_user_are_a_warning() {
    assert_eq!(with(|s| s.outbox.held_count = 40), plain(Reason::DeletesHeld));
    assert_eq!(Reason::DeletesHeld.state(), State::Warning);
}

#[test]
fn conflicts_are_a_warning() {
    assert_eq!(with(|s| s.local.conflict_count = 1), plain(Reason::Conflicts));
    assert_eq!(Reason::Conflicts.state(), State::Warning);
}

#[test]
fn a_full_onedrive_is_a_warning() {
    assert_eq!(with(|s| s.outbox.quota_full = true), plain(Reason::QuotaFull));
    assert_eq!(Reason::QuotaFull.state(), State::Warning);
}

#[test]
fn files_too_big_for_the_space_left_are_a_warning() {
    assert_eq!(with(|s| s.outbox.too_big_count = 2), plain(Reason::TooBig));
    assert_eq!(Reason::TooBig.state(), State::Warning);
}

#[test]
fn changes_that_cannot_be_uploaded_are_a_warning() {
    assert_eq!(with(|s| s.outbox.blocked_count = 1), plain(Reason::Blocked));
    assert_eq!(Reason::Blocked.state(), State::Warning);
}

#[test]
fn files_not_updated_here_yet_are_a_warning_that_says_the_note() {
    let note = ReplacementNote { files: 2, why: "no space left on device".into() };
    let decided = with(|s| s.cycle.replacement_note = Some(note.clone()));
    assert_eq!(decided, about(Reason::NotUpdated, &note.text()));
    assert_eq!(Reason::NotUpdated.state(), State::Warning);
}

/// The helper's trouble counts against a folder it intercepts, and not against one
/// registered without interception, where nothing downloads on open whatever the helper does.
#[test]
fn the_helper_in_trouble_is_a_warning_for_a_folder_it_intercepts() {
    for helper in [HelperState::NotInstalled, HelperState::Stopped, HelperState::Failed, HelperState::Unknown] {
        assert_eq!(with(|s| s.folder.helper_state = helper), plain(Reason::HelperUnavailable), "{helper:?}");
        let without = with(|s| {
            s.folder.helper_state = helper;
            s.folder.root_state = RootState::NoInterception;
            s.folder.no_interception_warning = true;
        });
        assert_eq!(without, plain(Reason::UpToDate), "{helper:?}");
    }
    assert_eq!(Reason::HelperUnavailable.state(), State::Warning);
}

#[test]
fn trouble_that_does_not_stop_the_folder_is_a_warning_that_says_it() {
    let said = "the folder could not be brought up to date: errno 5";
    assert_eq!(with(|s| s.cycle.sync_trouble = other(said, false)), about(Reason::Trouble, said));
    // Every part of `LastError` that is a problem of a running folder counts, joined as
    // `LastError` joins them.
    let all = with(|s| {
        s.folder.last_error = "left 1 interrupted file(s)".into();
        s.folder.switch_note = Some(SwitchNote { why: "errno 5".into() });
        s.folder.locked_note = "the folder cannot be watched".into();
        s.local.watch_note = "part of the folder is scanned".into();
        s.local.handles_note = "the handles were taken again".into();
        s.outbox.note = Some(OutboxNote::Unreadable);
    });
    assert_eq!(all.reason, Reason::Trouble);
    assert_eq!(
        all.trouble,
        format!(
            "left 1 interrupted file(s). {}. the folder cannot be watched. part of the folder is scanned. the handles were taken again. {}",
            SwitchNote { why: "errno 5".into() }.text(),
            OutboxNote::Unreadable.text()
        )
    );
    assert_eq!(Reason::Trouble.state(), State::Warning);
}

#[test]
fn a_pause_by_the_user_is_paused() {
    assert_eq!(with(|s| s.pause.paused_until = Some(0)), plain(Reason::Paused));
    assert_eq!(Reason::Paused.state(), State::Paused);
}

#[test]
fn an_account_holding_back_by_itself_is_paused() {
    assert_eq!(with(|s| s.pause.held_back = "metered".into()), plain(Reason::HeldBack));
    // The user's pause is said first when both hold.
    let both = with(|s| {
        s.pause.held_back = "on-battery".into();
        s.pause.paused_until = Some(1_700_000_000);
    });
    assert_eq!(both, plain(Reason::Paused));
    assert_eq!(Reason::HeldBack.state(), State::Paused);
}

#[test]
fn a_folder_at_rest_is_up_to_date() {
    assert_eq!(of(&healthy()), plain(Reason::UpToDate));
    assert_eq!(Reason::UpToDate.state(), State::Ok);
}

// --- The order between rows that can hold at once ---

/// The window's order, one rule after the other: each is decided over every rule after it.
#[test]
fn the_first_rule_that_holds_decides() {
    type Change = fn(&mut SyncSnapshot);
    let rules: [(Reason, Change); 12] = [
        (Reason::DeletesHeld, |s| s.outbox.held_count = 1),
        (Reason::Conflicts, |s| s.local.conflict_count = 1),
        (Reason::QuotaFull, |s| s.outbox.quota_full = true),
        (Reason::TooBig, |s| s.outbox.too_big_count = 1),
        (Reason::Blocked, |s| s.outbox.blocked_count = 1),
        (Reason::NotUpdated, |s| s.cycle.replacement_note = Some(ReplacementNote { files: 1, why: "no space".into() })),
        (Reason::HelperUnavailable, |s| s.folder.helper_state = HelperState::Unknown),
        (Reason::Paused, |s| s.pause.paused_until = Some(0)),
        (Reason::HeldBack, |s| s.pause.held_back = "metered".into()),
        (Reason::Trouble, |s| s.local.watch_note = "part of the folder is scanned".into()),
        (Reason::Transferring, |s| s.outbox.pending_count = 1),
        (Reason::UpToDate, |_| {}),
    ];
    for first in 0..rules.len() {
        let mut s = healthy();
        for (_, change) in &rules[first..] {
            change(&mut s);
        }
        assert_eq!(of(&s).reason, rules[first].0, "over every rule after it");
    }
    // The four before them, each over all twelve.
    let mut s = healthy();
    for (_, change) in &rules {
        change(&mut s);
    }
    s.cycle.sync_trouble = other("the tree store: disk I/O error", true);
    assert_eq!(of(&s).reason, Reason::Stopped);
    s.folder.root_state = RootState::Waiting;
    s.cycle.sync_trouble = None;
    assert_eq!(of(&s).reason, Reason::Starting);
    s.folder.root_path.clear();
    assert_eq!(of(&s).reason, Reason::NoFolder);
    assert_eq!(overall(SignInState::SignedOut, &s, 0).reason, Reason::SignedOut);
    assert_eq!(overall(SignInState::SigningIn, &s, 0).reason, Reason::SigningIn);
}

#[test]
fn deletes_held_come_before_conflicts() {
    let both = with(|s| {
        s.outbox.held_count = 2;
        s.local.conflict_count = 5;
    });
    assert_eq!(both, plain(Reason::DeletesHeld));
}

/// A pause is said over trouble that does not stop the folder, and the trouble's sentence
/// is not published with it; trouble that stops the folder is said over a pause.
#[test]
fn a_pause_and_trouble() {
    let paused = with(|s| {
        s.pause.paused_until = Some(0);
        s.local.watch_note = "part of the folder is scanned".into();
    });
    assert_eq!(paused, plain(Reason::Paused));
    let stopped = with(|s| {
        s.pause.paused_until = Some(0);
        s.cycle.sync_trouble = other("signed out", true);
    });
    assert_eq!(stopped, about(Reason::Stopped, "signed out"));
}

#[test]
fn unreachable_and_paused_is_paused() {
    let both = with(|s| {
        s.cycle.sync_trouble = unreachable("cannot reach OneDrive (timed out); trying again");
        s.pause.paused_until = Some(0);
    });
    assert_eq!(both, plain(Reason::Paused));
    let held = with(|s| {
        s.cycle.sync_trouble = unreachable("cannot reach OneDrive (timed out); trying again");
        s.pause.held_back = "power-saver".into();
    });
    assert_eq!(held, plain(Reason::HeldBack));
}

#[test]
fn a_listing_and_a_pause_is_paused() {
    let both = with(|s| {
        s.cycle.listing = true;
        s.pause.paused_until = Some(0);
    });
    assert_eq!(both, plain(Reason::Paused));
}

/// While the first listing runs, trouble that does not stop the folder is not counted, as
/// the window never read it then: the account is listing.
#[test]
fn a_listing_with_trouble_that_does_not_stop_it_is_listing() {
    let listing = with(|s| {
        s.cycle.listing = true;
        s.cycle.sync_trouble = unreachable("cannot reach OneDrive (timed out); trying again");
    });
    assert_eq!(listing, plain(Reason::Listing));
    // What needs the user is counted while listing too.
    let held = with(|s| {
        s.cycle.listing = true;
        s.outbox.held_count = 1;
    });
    assert_eq!(held, plain(Reason::DeletesHeld));
}

/// The helper in trouble over a folder that is paused: the warning for a folder it
/// intercepts, the pause for one it does not.
#[test]
fn helper_trouble_and_a_pause_on_an_intercepted_and_on_a_not_intercepted_folder() {
    let change = |s: &mut SyncSnapshot| {
        s.folder.helper_state = HelperState::Failed;
        s.pause.paused_until = Some(0);
    };
    assert_eq!(with(change), plain(Reason::HelperUnavailable));
    let without = with(|s| {
        change(s);
        s.folder.root_state = RootState::NoInterception;
        s.folder.no_interception_warning = true;
    });
    assert_eq!(without, plain(Reason::Paused));
}

// --- Nothing is decided by a sentence ---

/// Out of reach is the kind the trouble carries: a reworded sentence is offline all the
/// same, and the old sentence under another kind is not.
#[test]
fn unreachable_is_decided_by_the_kind_and_not_by_the_sentence() {
    let reworded = "OneDrive does not answer; the next try is in a minute";
    assert_eq!(with(|s| s.cycle.sync_trouble = unreachable(reworded)), about(Reason::Unreachable, reworded));
    let old_words = "cannot reach OneDrive (timed out); trying again";
    assert_eq!(with(|s| s.cycle.sync_trouble = other(old_words, false)), about(Reason::Trouble, old_words));
}

/// Offline only when out of reach is all that is wrong: with anything else said, it is a
/// warning, and both sentences are published.
#[test]
fn unreachable_with_other_trouble_is_a_warning() {
    let both = with(|s| {
        s.cycle.sync_trouble = unreachable("cannot reach OneDrive (timed out); trying again");
        s.local.watch_note = "part of the folder is scanned".into();
    });
    assert_eq!(both, about(Reason::Trouble, "cannot reach OneDrive (timed out); trying again. part of the folder is scanned"));
}

/// The no-interception warning is a fact of the folder, never trouble: a folder that says
/// only it is up to date, and what it ran into is published without the warning.
#[test]
fn the_no_interception_warning_is_not_trouble() {
    let mut s = healthy();
    s.folder.root_state = RootState::NoInterception;
    s.folder.no_interception_warning = true;
    assert_eq!(published_error(&s), NO_INTERCEPTION_WARNING);
    assert_eq!(of(&s), plain(Reason::UpToDate));

    s.folder.last_error = "left 1 interrupted file(s)".into();
    assert_eq!(published_error(&s), format!("{NO_INTERCEPTION_WARNING}. left 1 interrupted file(s)"));
    assert_eq!(of(&s), about(Reason::Trouble, "left 1 interrupted file(s)"));
}

/// The failed-update note is the note alone: what `LastError` says after it is trouble of
/// its own, and waits its turn.
#[test]
fn the_failed_update_note_is_published_alone() {
    let note = ReplacementNote { files: 1, why: "no space".into() };
    let decided = with(|s| {
        s.cycle.replacement_note = Some(note.clone());
        s.cycle.sync_trouble = unreachable("cannot reach OneDrive (timed out); trying again");
        s.local.watch_note = "part of the folder is scanned".into();
    });
    assert_eq!(decided, about(Reason::NotUpdated, &note.text()));
}

/// Only the four reasons that are about a sentence publish one.
#[test]
fn a_sentence_is_published_only_for_the_reasons_that_have_one() {
    let mut s = healthy();
    s.local.watch_note = "part of the folder is scanned".into();
    s.outbox.held_count = 1;
    let decided = of(&s);
    assert!(!decided.reason.has_sentence() && decided.trouble.is_empty(), "{decided:?}");
    for reason in [Reason::Stopped, Reason::Trouble, Reason::Unreachable, Reason::NotUpdated] {
        assert!(reason.has_sentence());
    }
}
