//! What holds all sending to OneDrive: the state of the sending as a whole, and its ONE
//! owner.
//!
//! **State owned:** the causes that hold now: the folder's sync stopping, the sign-out,
//! the account not being allowed to write, the folder being read-only, the pause, OneDrive's throttle with its end,
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
// test: a_read_only_folder_holds_what_is_sent_and_no_download_and_is_shown_after_the_accounts_refusal
#![allow(dead_code, unused_variables)]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use konedrive_graph::drive::MAX_RETRY_AFTER;

use crate::sync_parts::running_steps::StepClass;
use crate::sync_parts::sync_plan_wake::{SyncPlanWake, WakeCause};

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
    /// The folder is read-only: what is unsent stays here, not sent and not lost, and is
    /// sent when the folder is read-write again. Set by a switch to read-only
    /// (`sync_tasks::access_mode_change`), before the account gives up the token that
    /// can write. Shown as waiting because the folder is read-only, never as blocked.
    FolderReadOnly,
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
/// alone; a step towards the folder or of the rows by none. Private: who asks whether a
/// step is held asks `SendingHolds::is_held` or `list_held_classes`, which ask this of
/// every hold.
fn decide_is_held(hold: Option<&SendingHold>, class: StepClass) -> bool {
    let Some(hold) = hold else { return false };
    match class {
        StepClass::Content | StepClass::Metadata => true,
        StepClass::Download => matches!(hold, SendingHold::Stopping | SendingHold::SignedOut | SendingHold::Paused { .. }),
        StepClass::Folder | StepClass::Row => false,
    }
}

#[derive(Default, Clone, PartialEq, Eq)]
struct Holds {
    is_stopping: bool,
    is_signed_out: bool,
    writes_refusal: Option<String>,
    paused_until: Option<Option<i64>>,
    throttled_until: Option<i64>,
    /// Whether the end of the throttle is one chosen here: OneDrive named none.
    is_throttle_end_chosen: bool,
    /// The wait chosen last when OneDrive named none, doubled each time.
    chosen_wait: Option<Duration>,
    has_listing_ended: bool,
}

impl Holds {
    /// Every cause that holds at `now`, in the order of the cases.
    fn list(&self, now: i64) -> Vec<SendingHold> {
        let paused = self.paused_until.filter(|until| until.is_none_or(|until| until > now));
        let holds = [
            self.is_stopping.then_some(SendingHold::Stopping),
            self.is_signed_out.then_some(SendingHold::SignedOut),
            self.writes_refusal.clone().map(|reason| SendingHold::WritesNotAllowed { reason }),
            paused.map(|until| SendingHold::Paused { until }),
            self.find_throttle_end(now).map(|until| SendingHold::Throttled { until }),
            (!self.has_listing_ended).then_some(SendingHold::FirstListingNotEnded),
        ];
        holds.into_iter().flatten().collect()
    }

    /// When the throttle ends, while it lasts at `now`.
    fn find_throttle_end(&self, now: i64) -> Option<i64> {
        self.throttled_until.filter(|until| *until > now)
    }

    /// OneDrive refused a request at `now` and asked to wait `retry_after` (at most an
    /// hour), or named no time. While a wait is under way: a time OneDrive named takes
    /// the place of a chosen one, even a shorter one; of two named times the later end
    /// stands; a refusal that names none adds nothing. With no wait under way a refusal
    /// that names none waits ten seconds, twice the last such wait each time, up to an
    /// hour.
    fn throttle(&mut self, retry_after: Option<Duration>, now: i64) {
        let end_under_way = self.find_throttle_end(now);
        match retry_after {
            Some(wait) => {
                let until = now + wait.min(MAX_RETRY_AFTER).as_secs().max(1) as i64;
                if self.is_throttle_end_chosen || end_under_way.is_none_or(|end| until > end) {
                    self.throttled_until = Some(until);
                }
                self.is_throttle_end_chosen = false;
            }
            None if end_under_way.is_some() => {}
            None => {
                let wait = self.chosen_wait.map_or(THROTTLE_FIRST, |last| (last * 2).min(BACKOFF_MAX));
                self.throttled_until = Some(now + wait.as_secs().max(1) as i64);
                self.is_throttle_end_chosen = true;
                self.chosen_wait = Some(wait);
            }
        }
    }
}

/// What holds one folder's sending.
pub struct SendingHolds {
    holds: std::sync::Mutex<Holds>,
    sync_plan_wake: Arc<SyncPlanWake>,
}

impl SendingHolds {
    /// Makes `change` to what holds, and wakes the plan when it changed something: after
    /// this part's lock is let go.
    fn change(&self, change: impl FnOnce(&mut Holds)) {
        let is_changed = {
            let mut holds = crate::panic::lock(&self.holds);
            let before = holds.clone();
            change(&mut holds);
            *holds != before
        };
        if is_changed {
            self.sync_plan_wake.wake(WakeCause::SendingHoldChanged);
        }
    }

    /// Nothing holds but the first listing. Made by the folder's start.
    pub fn new(sync_plan_wake: Arc<SyncPlanWake>) -> Self {
        SendingHolds { holds: std::sync::Mutex::default(), sync_plan_wake }
    }

    /// The hold that is SHOWN now, at the time `now`: the first that holds, in the order
    /// of the cases; `None` when sending may go on. A throttle or a timed pause whose end
    /// passed holds no more. For what the bus and the published plan say. Whether a step
    /// is held is not asked of it ([`is_held`](Self::is_held)): a later cause may hold a
    /// class the first does not.
    pub fn find(&self, now: i64) -> Option<SendingHold> {
        crate::panic::lock(&self.holds).list(now).into_iter().next()
    }

    /// The soonest time at which something that holds at `now` ends by itself, Unix
    /// seconds; `None` when nothing that holds does. Of every hold, not of the one shown
    /// alone: a timed pause behind a hold that ends by nothing still frees the downloads
    /// at its end. For the plan's task's timer.
    pub fn find_end(&self, now: i64) -> Option<i64> {
        let ends = crate::panic::lock(&self.holds).list(now).into_iter().filter_map(|hold| match hold {
            SendingHold::Paused { until } => until,
            SendingHold::Throttled { until } => Some(until),
            SendingHold::Stopping | SendingHold::SignedOut | SendingHold::WritesNotAllowed { .. } | SendingHold::FolderReadOnly | SendingHold::FirstListingNotEnded => None,
        });
        ends.min()
    }

    /// The account's pause began or ended. Called by `account_folder/pause.rs`.
    pub fn set_paused(&self, paused_until: Option<Option<i64>>) {
        self.change(|holds| holds.paused_until = paused_until);
    }

    /// A step was answered that the account is signed out; or the account signed in.
    pub fn set_signed_out(&self, is_signed_out: bool) {
        self.change(|holds| holds.is_signed_out = is_signed_out);
    }

    /// Why the account may not write to OneDrive now, or none
    /// (`account_folder::mode::decide_account_holds`: the sign-in grants no writes,
    /// `config.toml` cannot be read or records no drive, the token reaches another drive
    /// than the recorded one). Called by the owner's watcher of the account
    /// (`account_folder::mode::watch_account`) whenever what that function answers
    /// changed; what stands at the start is given with the start
    /// (`StandingSendingHolds::writes_refusal`). That the folder is read-only is NOT
    /// given here: that is a hold of its own
    /// ([`set_folder_read_only`](Self::set_folder_read_only)), with another setter.
    pub fn set_writes_refusal(&self, refusal: Option<String>) {
        self.change(|holds| holds.writes_refusal = refusal);
    }

    /// The folder was switched to read-only, or back to read-write: the hold
    /// `SendingHold::FolderReadOnly`, shown after `WritesNotAllowed` and before `Paused`
    /// in the order of the cases; it holds what is sent and no download. Called by
    /// `sync_tasks::access_mode_change` alone — at a switch, and at a start for a folder
    /// wanted read-only — which is its one setter: the account's own refusal is
    /// `set_writes_refusal`'s, and neither clears the other.
    #[allow(unused_variables)]
    pub fn set_folder_read_only(&self, is_read_only: bool) {
        todo!()
    }

    /// OneDrive throttled a step, naming `retry_after` or nothing: today's rules
    /// (`engine/state.rs:42`, `Throttle::refused`).
    pub fn throttle(&self, retry_after: Option<Duration>, now: i64) {
        self.change(|holds| holds.throttle(retry_after, now));
    }

    /// A step towards OneDrive went through: a chosen wait starts from ten seconds again.
    pub fn pass(&self) {
        // Nothing that holds changes: the plan is not woken.
        crate::panic::lock(&self.holds).chosen_wait = None;
    }

    /// A listing ended. Called by the plan's task on `WakeCause::ListingEnded`.
    pub fn set_listing_ended(&self) {
        self.change(|holds| holds.has_listing_ended = true);
    }

    /// Whether a step of `class` is held now, at the time `now`: by ANY cause that holds,
    /// not by the first alone — a pause holds a download also while the hold that is
    /// shown is one that holds no download. What the plan's task and an upload between
    /// two fragments ask; [`find`](Self::find) is what is shown.
    pub fn is_held(&self, class: StepClass, now: i64) -> bool {
        crate::panic::lock(&self.holds).list(now).iter().any(|hold| decide_is_held(Some(hold), class))
    }

    /// Every class of step that is held at `now`, as [`is_held`](Self::is_held) answers
    /// each: read once, under one lock, for the pure choice of which steps start
    /// (`sync_plan_task::step_choice::decide_steps_to_start`).
    pub fn list_held_classes(&self, now: i64) -> BTreeSet<StepClass> {
        let holds = crate::panic::lock(&self.holds).list(now);
        let classes = [StepClass::Content, StepClass::Metadata, StepClass::Download, StepClass::Folder, StepClass::Row];
        classes.into_iter().filter(|class| holds.iter().any(|hold| decide_is_held(Some(hold), *class))).collect()
    }

    /// The folder's sync is stopping: nothing new is started, uploads stop after their
    /// fragment.
    pub fn set_stopping(&self) {
        self.change(|holds| holds.is_stopping = true);
    }
}

/// Retries of a step that failed for a reason expected to pass: 1 s,
/// doubling, at most an hour (provisional). Never dropped.
pub const BACKOFF_FIRST: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(3600);
/// Throttled without `Retry-After`: 10 s, doubling, at most an hour (provisional).
pub const THROTTLE_FIRST: Duration = Duration::from_secs(10);

#[cfg(test)]
mod tests;
