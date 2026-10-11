//! What wakes a folder's sync plan, and why.
//!
//! **State owned:** the causes given since the plan's task last took them, and the signal
//! the task sleeps on. Anyone who holds the wake may add a cause ([`SyncPlanWake::wake`]);
//! the plan's task alone takes them ([`SyncPlanWake::wait`]). It is here, below every part
//! that wakes the plan — the mount, the steps, the listing — because each of them is built
//! before the task is, and none of them knows the task.
//!
//! **Answers for:** the map's MISSING-2.i: in place of the upload queue's waker
//! (`fuse/outbox_recorder.rs:23`), of the cycle's `cycled` (`account_folder/mode.rs:85`) and of
//! SQLite's update hook on `outbox` (`konedrive-tree`'s `outbox/changes.rs:76`), none of
//! which sees a write of `items`.
//!
//! **Tested without the rest:**
// test: a_cause_given_before_the_wait_is_answered_at_once
// test: causes_given_while_nobody_waits_are_answered_together_and_once
// test: a_wait_with_nothing_given_ends_when_a_cause_is_given
#![allow(dead_code, unused_variables)]

use std::collections::BTreeSet;

/// Why the plan is made again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WakeCause {
    /// A write operation of the mount wrote a local version, or the last writer closed.
    LocalVersionWritten,
    /// The listing cycle wrote a page into the rows.
    ListingPageWritten,
    /// A listing ended: what waited for it can be judged.
    ListingEnded,
    /// A step finished, whatever came of it.
    StepFinished,
    /// The user answered: removals confirmed or taken back, a pin set or taken off.
    UserAnswered,
    /// A refresh was asked for, by the user or because the network is back: every next
    /// try is due.
    RefreshAsked,
    /// The quota was read: OneDrive may have space again, or none.
    QuotaRead,
    /// What holds all sending changed: the pause, the sign-in, the network, the access
    /// mode, the list of ignored names.
    SendingHoldChanged,
    /// A request whose answer was never committed was settled by reading OneDrive
    /// (`onedrive_steps::lost_answers`): its item and the directory it was sent into are
    /// busy no more, and its answer may be written. The task makes a plan, and nothing
    /// else.
    RequestRecordSettled,
    /// The next try of what is left to clean in OneDrive — a cancel, a removal of an
    /// item an upload left, the reading of a request record — is due at another time
    /// than the task's timer was set with. The task makes a plan, which sets its timer
    /// (`sync_plan_task::decide_next_wake`), and nothing else.
    NextCleaningTryChanged,
}

/// The wake of one folder's plan.
pub struct SyncPlanWake {
    given: std::sync::Mutex<BTreeSet<WakeCause>>,
    signal: tokio::sync::Notify,
}

impl SyncPlanWake {
    /// A wake nobody was given yet. Made by the folder's start before the mount, so that
    /// the mount's write operations hold it from their first job (design §9; the map's
    /// 7.1, 4d).
    pub fn new() -> Self {
        SyncPlanWake { given: std::sync::Mutex::new(BTreeSet::new()), signal: tokio::sync::Notify::new() }
    }

    /// The plan is to be made again, for `cause`. Called from any thread, async or not;
    /// never waits. Called after the row is written, never before: the task reads the
    /// rows when it wakes.
    pub fn wake(&self, cause: WakeCause) {
        crate::panic::lock(&self.given).insert(cause);
        // After the lock is let go. With nobody waiting the signal is kept for the next
        // wait, which finds the cause either way.
        self.signal.notify_one();
    }

    /// Waits until a cause is given and takes all that were. Called by the plan's task
    /// alone, in a `select` with its timer and its stop (`sync_plan_task`).
    pub async fn wait(&self) -> BTreeSet<WakeCause> {
        loop {
            let given = std::mem::take(&mut *crate::panic::lock(&self.given));
            if !given.is_empty() {
                return given;
            }
            // A signal kept from a cause that was taken already ends this at once, and the
            // next round finds nothing and waits again.
            self.signal.notified().await;
        }
    }
}

#[cfg(test)]
mod tests;
