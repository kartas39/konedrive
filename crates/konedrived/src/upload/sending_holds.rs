//! What holds all sending to OneDrive: the state of the sending as a whole, and its ONE
//! owner.
//!
//! **State owned:** the causes that hold now: the folder's sync stopping, the sign-out,
//! the account not being allowed to write, the pause, OneDrive's throttle with its end,
//! the first listing of this run not having ended. Each is set and cleared by its own
//! cause, through the functions here, and every change wakes the plan
//! (`WakeCause::SendingHoldChanged`); none is stored (as today: `engine/state.rs`,
//! `Throttle`, `Trouble`, `Cycle`). It is here, below the plan's task, because an upload
//! reads it between two fragments and must not know the task. Whether OneDrive is full
//! is NOT here: that is `OneDriveSpace`'s, and holds content alone.
//!
//! **Answers for:** design §7's third source of what the window shows — "paused, signed
//! out, held back by OneDrive" — and that no step towards OneDrive, and no download of a
//! pinned file, starts or goes on meanwhile. Steps towards the folder and of the rows are
//! never held by it.
//!
//! **Tested without the rest** (today's `engine/state/tests.rs` are the reference):
// test: a_time_onedrive_named_beats_one_that_was_chosen_and_the_later_of_two_stands
// test: with_no_time_named_the_wait_doubles_from_ten_seconds_to_an_hour
// test: a_step_that_went_through_ends_the_chosen_wait
// test: each_cause_is_set_and_cleared_by_itself_and_wakes_the_plan
// test: the_first_cause_in_the_order_of_the_cases_is_the_one_shown
// test: a_throttle_holds_no_download_and_a_pause_does
// test: a_read_only_folder_that_still_sends_what_was_unsent_is_not_held_for_being_read_only
#![allow(dead_code, unused_variables)]

use std::sync::Arc;
use std::time::Duration;

use crate::folder::running_steps::StepClass;
use crate::folder::sync_plan_wake::SyncPlanWake;

/// Why nothing is sent to OneDrive now; the first that holds, in this order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendingHold {
    /// The folder's sync is stopping.
    Stopping,
    /// The account is signed out.
    SignedOut,
    /// The account's sign-in grants no writes, or the token reaches another drive than
    /// the folder's: with the reason as the account says it.
    WritesNotAllowed { reason: String },
    /// The account's sync is paused, until a time or until resumed.
    Paused { until: Option<i64> },
    /// OneDrive asked to wait, until this time.
    Throttled { until: i64 },
    /// No listing of this run of the sync has ended yet: nothing is sent on rows a
    /// stopped daemon left behind OneDrive (as today: `engine.rs`, `Cycle`).
    FirstListingNotEnded,
}

/// Whether `hold` holds a step of `class`. Pure. Every hold holds what is sent
/// (`Content`, `Metadata`); a download is held by the stop, the sign-out and the pause
/// alone; a step towards the folder or of the rows by none.
pub fn decide_is_held(hold: Option<&SendingHold>, class: StepClass) -> bool {
    todo!()
}

#[derive(Default)]
struct Holds {
    is_stopping: bool,
    is_signed_out: bool,
    writes_refusal: Option<String>,
    paused_until: Option<Option<i64>>,
    throttled_until: Option<i64>,
    /// The wait chosen last when OneDrive named none, doubled each time.
    chosen_wait: Option<Duration>,
    has_listing_ended: bool,
}

/// What holds one folder's sending.
pub struct SendingHolds {
    holds: std::sync::Mutex<Holds>,
    sync_plan_wake: Arc<SyncPlanWake>,
}

impl SendingHolds {
    /// Nothing holds but the first listing. Made by the folder's start.
    pub fn new(sync_plan_wake: Arc<SyncPlanWake>) -> Self {
        todo!()
    }

    /// The hold that counts now, at the time `now`; `None` when sending may go on. A
    /// throttle or a timed pause whose end passed holds no more. Called by the plan's
    /// task, by an upload between two fragments, and by the bus.
    pub fn find(&self, now: i64) -> Option<SendingHold> {
        todo!()
    }

    /// When the hold that counts now ends by itself, Unix seconds; `None` when none does.
    /// For the plan's task's timer.
    pub fn find_end(&self, now: i64) -> Option<i64> {
        todo!()
    }

    /// The account's pause began or ended. Called by `sync/pause.rs`.
    pub fn set_paused(&self, paused_until: Option<Option<i64>>) {
        todo!()
    }

    /// A step was answered that the account is signed out; or the account signed in.
    pub fn set_signed_out(&self, is_signed_out: bool) {
        todo!()
    }

    /// What the write gate says of the account (`sync/mode.rs`, `Gate`): its refusal, or
    /// none. The gate's two refusals of a read-only folder and account are not given
    /// here: a read-only folder sends what was unsent at its switch (design §13 (3); the
    /// map's RISK-7.f).
    pub fn set_writes_refusal(&self, refusal: Option<String>) {
        todo!()
    }

    /// OneDrive throttled a step, naming `retry_after` or nothing: today's rules
    /// (`engine/state.rs:42`, `Throttle::refused`).
    pub fn throttle(&self, retry_after: Option<Duration>, now: i64) {
        todo!()
    }

    /// A step towards OneDrive went through: a chosen wait starts from ten seconds again.
    pub fn pass(&self) {
        todo!()
    }

    /// A listing ended. Called by the plan's task on `WakeCause::ListingEnded`.
    pub fn set_listing_ended(&self) {
        todo!()
    }

    /// The folder's sync is stopping: nothing new is started, uploads stop after their
    /// fragment.
    pub fn set_stopping(&self) {
        todo!()
    }
}
