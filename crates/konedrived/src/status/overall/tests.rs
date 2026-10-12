use konedrive_dbus::overall::State;

use super::*;
use crate::status::snapshot::{UploadQueueNote, RootState, SyncTrouble};

/// A folder that is up and at rest: `up-to-date` for an account that is signed in.
fn healthy() -> SyncSnapshot {
    let mut s = SyncSnapshot::default();
    s.folder.root_path = "/home/u/OneDrive".into();
    s.folder.root_state = RootState::Ready;
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

/// A reason with nothing said of trouble.
fn plain(reason: Reason) -> Overall {
    Overall { reason, trouble: String::new(), not_updated: String::new() }
}

/// A reason with the trouble there is.
fn about(reason: Reason, trouble: &str) -> Overall {
    Overall { reason, trouble: trouble.into(), not_updated: String::new() }
}

// --- One test for each row of the table ---

#[test]
fn an_account_signing_in_is_offline() {
    // Whatever its folder says.
    let mut s = healthy();
    s.upload_queue.blocked_count = 3;
    assert_eq!(overall(SignInState::SigningIn, &s, 1), plain(Reason::SigningIn));
    assert_eq!(Reason::SigningIn.state(), State::Offline);
}

#[test]
fn an_account_not_signed_in_is_offline() {
    let mut s = healthy();
    s.cycle.sync_trouble = other("signed out", true);
    assert_eq!(overall(SignInState::SignedOut, &s, 0), about(Reason::SignedOut, "signed out"));
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
fn the_first_listing_is_syncing() {
    assert_eq!(with(|s| s.cycle.listing = true), plain(Reason::Listing));
    assert_eq!(Reason::Listing.state(), State::Syncing);
}

#[test]
fn anything_moving_or_waiting_to_upload_is_transferring() {
    assert_eq!(overall(SignInState::SignedIn, &healthy(), 1), plain(Reason::Transferring), "a download");
    assert_eq!(with(|s| s.upload_queue.uploads = vec![("/home/u/OneDrive/a".into(), 1, 2)]), plain(Reason::Transferring), "an upload");
    assert_eq!(with(|s| s.upload_queue.pending_count = 1), plain(Reason::Transferring), "a change waiting to upload");
    assert_eq!(Reason::Transferring.state(), State::Syncing);
}

#[test]
fn conflicts_are_a_warning() {
    assert_eq!(with(|s| s.local.conflict_count = 1), plain(Reason::Conflicts));
    assert_eq!(Reason::Conflicts.state(), State::Warning);
}

#[test]
fn a_full_onedrive_is_a_warning() {
    assert_eq!(with(|s| s.upload_queue.quota_full = true), plain(Reason::QuotaFull));
    assert_eq!(Reason::QuotaFull.state(), State::Warning);
}

#[test]
fn files_too_big_for_the_space_left_are_a_warning() {
    assert_eq!(with(|s| s.upload_queue.too_big_count = 2), plain(Reason::TooBig));
    assert_eq!(Reason::TooBig.state(), State::Warning);
}

#[test]
fn changes_that_cannot_be_uploaded_are_a_warning() {
    assert_eq!(with(|s| s.upload_queue.blocked_count = 1), plain(Reason::Blocked));
    assert_eq!(Reason::Blocked.state(), State::Warning);
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

#[test]
fn unreachable_and_paused_is_paused() {
    let both = with(|s| {
        s.cycle.sync_trouble = unreachable("cannot reach OneDrive (timed out); trying again");
        s.pause.paused_until = Some(0);
    });
    assert_eq!(both, about(Reason::Paused, "cannot reach OneDrive (timed out); trying again"));
    let held = with(|s| {
        s.cycle.sync_trouble = unreachable("cannot reach OneDrive (timed out); trying again");
        s.pause.held_back = "power-saver".into();
    });
    assert_eq!(held, about(Reason::HeldBack, "cannot reach OneDrive (timed out); trying again"));
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
    let conflicts = with(|s| {
        s.cycle.listing = true;
        s.local.conflict_count = 1;
    });
    assert_eq!(conflicts, plain(Reason::Conflicts));
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

#[test]
fn a_folder_not_up_yet_is_starting() {
    let waiting = with(|s| s.folder.root_state = RootState::Waiting);
    assert_eq!(waiting, plain(Reason::Starting), "calm: nothing is known to be wrong");
    assert_eq!(Reason::Starting.state(), State::Syncing);
}

#[test]
fn a_folder_stopped_on_an_error_says_everything_last_error_says() {
    let blocked = with(|s| s.cycle.sync_trouble = other("signed out: sign in again", true));
    assert_eq!(blocked, about(Reason::Stopped, "signed out: sign in again"));

    // A folder that could not be brought up: what the registration ran into.
    let mut s = healthy();
    s.folder.root_state = RootState::Error;
    s.folder.last_error = "cannot bring up the sync folder".into();
    let stopped = of(&s);
    assert_eq!(stopped.reason, Reason::Stopped);
    assert_eq!(stopped.trouble, published_error(&s));

    // A registration that failed, with nothing more to say than its state.
    assert_eq!(with(|s| s.folder.root_state = RootState::Error), plain(Reason::Stopped));
    assert_eq!(Reason::Stopped.state(), State::Warning);
}

#[test]
fn trouble_that_does_not_stop_the_folder_is_a_warning_that_says_it() {
    let said = "the folder could not be brought up to date: errno 5";
    assert_eq!(with(|s| s.cycle.sync_trouble = other(said, false)), about(Reason::Trouble, said));
    // Every part of `LastError` that is a problem of a running folder counts, joined as
    // `LastError` joins them.
    let all = with(|s| {
        s.cycle.sync_trouble = other(said, false);
        s.upload_queue.note = Some(UploadQueueNote::WritesNotAllowed("the sign-in cannot write".into()));
    });
    assert_eq!(all, about(Reason::Trouble, &format!("{said}. {}", UploadQueueNote::WritesNotAllowed("the sign-in cannot write".into()).text())));
    assert_eq!(Reason::Trouble.state(), State::Warning);
}

#[test]
fn the_first_rule_that_holds_decides() {
    type Change = fn(&mut SyncSnapshot);
    let rules: [(Reason, Change); 10] = [
        (Reason::Conflicts, |s| s.local.conflict_count = 1),
        (Reason::QuotaFull, |s| s.upload_queue.quota_full = true),
        (Reason::TooBig, |s| s.upload_queue.too_big_count = 1),
        (Reason::Blocked, |s| s.upload_queue.blocked_count = 1),
        (Reason::Paused, |s| s.pause.paused_until = Some(0)),
        (Reason::HeldBack, |s| s.pause.held_back = "metered".into()),
        (Reason::Throttled, |s| s.transfers.throughput.retry_after = 300),
        (Reason::Trouble, |s| s.upload_queue.note = Some(UploadQueueNote::WritesNotAllowed("the sign-in cannot write".into()))),
        (Reason::Transferring, |s| s.upload_queue.pending_count = 1),
        (Reason::UpToDate, |_| {}),
    ];
    for first in 0..rules.len() {
        let mut s = healthy();
        for (_, change) in &rules[first..] {
            change(&mut s);
        }
        assert_eq!(of(&s).reason, rules[first].0, "over every rule after it");
    }
    // The four before them, each over all ten.
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

/// A pause is said over trouble that does not stop the folder, and the trouble's sentence
/// is published with it; trouble that stops the folder is said over a pause.
#[test]
fn a_pause_and_trouble() {
    let paused = with(|s| {
        s.pause.paused_until = Some(0);
        s.upload_queue.note = Some(UploadQueueNote::WritesNotAllowed("the sign-in cannot write".into()));
    });
    assert_eq!(paused, about(Reason::Paused, &UploadQueueNote::WritesNotAllowed("the sign-in cannot write".into()).text()));
    let stopped = with(|s| {
        s.pause.paused_until = Some(0);
        s.cycle.sync_trouble = other("signed out", true);
    });
    assert_eq!(stopped, about(Reason::Stopped, "signed out"));
}

#[test]
fn unreachable_with_other_trouble_is_a_warning() {
    let both = with(|s| {
        s.cycle.sync_trouble = unreachable("cannot reach OneDrive (timed out); trying again");
        s.upload_queue.note = Some(UploadQueueNote::WritesNotAllowed("the sign-in cannot write".into()));
    });
    assert_eq!(both, about(Reason::Trouble, &format!("cannot reach OneDrive (timed out); trying again. {}", UploadQueueNote::WritesNotAllowed("the sign-in cannot write".into()).text())));
}

/// The sentence of the trouble is the folder's alone: it is there under every reason.
#[test]
fn the_sentences_are_published_under_every_reason() {
    let mut s = healthy();
    s.upload_queue.note = Some(UploadQueueNote::WritesNotAllowed("the sign-in cannot write".into()));
    s.local.conflict_count = 1;
    let said = |reason| about(reason, &UploadQueueNote::WritesNotAllowed("the sign-in cannot write".into()).text());
    assert_eq!(of(&s), said(Reason::Conflicts));
    assert_eq!(overall(SignInState::SignedOut, &s, 0), said(Reason::SignedOut));

    // A folder that has stopped says everything `LastError` says.
    s.cycle.sync_trouble = other("the tree store: disk I/O error", true);
    let stopped = of(&s);
    assert_eq!((stopped.reason, stopped.trouble), (Reason::Stopped, published_error(&s)));
}

/// While the first listing runs no trouble is published, as none is counted; nor for a
/// folder that is not up yet.
#[test]
fn no_trouble_is_published_while_listing_or_starting() {
    let listing = with(|s| {
        s.cycle.listing = true;
        s.upload_queue.note = Some(UploadQueueNote::WritesNotAllowed("the sign-in cannot write".into()));
        s.local.conflict_count = 1;
    });
    assert_eq!(listing, plain(Reason::Conflicts));
    let starting = with(|s| {
        s.folder.root_state = RootState::Waiting;
        s.upload_queue.note = Some(UploadQueueNote::WritesNotAllowed("the sign-in cannot write".into()));
    });
    assert_eq!(starting, plain(Reason::Starting));
}

#[test]
fn an_account_onedrive_asked_to_wait_is_paused_until_the_wait_is_over() {
    assert_eq!(with(|s| s.transfers.throughput.retry_after = 300), plain(Reason::Throttled));
    assert_eq!(with(|s| s.transfers.throughput.retry_after = 1), plain(Reason::Throttled), "to its last second");
    assert_eq!(Reason::Throttled.state(), State::Paused);
    // In place of the first listing, and of what moves.
    let listing = with(|s| {
        s.cycle.listing = true;
        s.transfers.throughput.retry_after = 300;
    });
    assert_eq!(listing.reason, Reason::Throttled);
    let mut moving = healthy();
    moving.transfers.throughput.retry_after = 300;
    assert_eq!(overall(SignInState::SignedIn, &moving, 3).reason, Reason::Throttled);
    // In place of the trouble its uploads' hold says, which is this wait; the sentence stays.
    let held = with(|s| {
        s.upload_queue.note = Some(UploadQueueNote::Throttled(1_700_000_300));
        s.transfers.throughput.retry_after = 300;
    });
    assert_eq!(held.reason, Reason::Throttled);
    assert!(!held.trouble.is_empty());
    // What needs the user, and a pause, come first; the wait over, the account is what it was.
    let conflicts = with(|s| {
        s.local.conflict_count = 1;
        s.transfers.throughput.retry_after = 300;
    });
    assert_eq!(conflicts.reason, Reason::Conflicts);
    assert_eq!(with(|s| s.cycle.listing = true).reason, Reason::Listing);
}
