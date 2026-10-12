//! The first filling of a folder: from a store no listing ever ended in, until every
//! item the first listing named is shown.
//!
//! While it lasts the activity log gets no event per item shown: nothing was added in
//! OneDrive, the folder is being filled (`step_settlement::decide_events_to_log`). It gets
//! two events of the folder instead, one when the filling begins in this run
//! (`ActivityKind::FirstSyncBegan`) and one when it ends (`ActivityKind::FirstSyncEnded`).
//!
//! **State owned:** how far the filling is (`FirstFillingProgress`), for the life of the
//! task's run. Set when the run begins, from whether the store says a listing ever ended
//! (`TreeStore::last_checked`); changed by the plan's loop alone, at
//! `WakeCause::ListingEnded` and after each plan.
//!
//! What it does not see: a run that stopped after the first listing ended and before
//! every item was shown. The next run finds a listing that ended, so what is left to show
//! is logged item by item (capped per listing), and no end is logged. A full listing made
//! again later is not a first filling: what it brings is a change.
//!
//! **Tested without the rest:**
// test: a_store_no_listing_ended_in_begins_the_first_filling
// test: the_filling_ends_with_the_first_plan_after_the_listing_that_leaves_nothing_to_show
// test: a_filling_that_ended_stays_ended

use std::sync::Mutex;

use konedrive_tree::ActivityKind;

/// How far the first filling of the folder is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FirstFillingProgress {
    /// The first listing has not ended.
    ListingNotEnded,
    /// The first listing ended; items it named may still be to show.
    ItemsLeftToShow,
    /// The folder was filled: before this run, or in it.
    Ended,
}

/// What the plan's loop tells the filling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FirstFillingFact {
    /// The run begins. `has_listing_ended_before`: the store says a listing ended.
    RunBegan { has_listing_ended_before: bool },
    /// A listing ended (`WakeCause::ListingEnded`).
    ListingEnded,
    /// A plan was made. `has_folder_steps`: it planned a step towards the folder that is
    /// no download, or one runs.
    PlanMade { has_folder_steps: bool },
}

/// What `fact` makes of the filling that was at `progress`, and the event of the folder
/// it is logged with, if any. Pure.
pub(crate) fn decide_first_filling(progress: FirstFillingProgress, fact: FirstFillingFact) -> (FirstFillingProgress, Option<ActivityKind>) {
    use FirstFillingProgress::{Ended, ItemsLeftToShow, ListingNotEnded};
    match (progress, fact) {
        (_, FirstFillingFact::RunBegan { has_listing_ended_before: false }) => (ListingNotEnded, Some(ActivityKind::FirstSyncBegan)),
        (_, FirstFillingFact::RunBegan { has_listing_ended_before: true }) => (Ended, None),
        (ListingNotEnded, FirstFillingFact::ListingEnded) => (ItemsLeftToShow, None),
        (ItemsLeftToShow, FirstFillingFact::PlanMade { has_folder_steps: false }) => (Ended, Some(ActivityKind::FirstSyncEnded)),
        (progress, FirstFillingFact::ListingEnded | FirstFillingFact::PlanMade { .. }) => (progress, None),
    }
}

/// The first filling of one folder, for its plan's task.
pub(crate) struct FirstFilling {
    progress: Mutex<FirstFillingProgress>,
}

impl FirstFilling {
    /// A filling that ended: what the task has before its run begins.
    pub fn new() -> Self {
        Self { progress: Mutex::new(FirstFillingProgress::Ended) }
    }

    /// Takes `fact` ([`decide_first_filling`]); the event of the folder to log, if any.
    pub(crate) fn note(&self, fact: FirstFillingFact) -> Option<ActivityKind> {
        let mut progress = crate::panic::lock(&self.progress);
        let (next, event) = decide_first_filling(*progress, fact);
        *progress = next;
        event
    }

    /// Whether the folder is being filled for the first time.
    pub(crate) fn is_under_way(&self) -> bool {
        *crate::panic::lock(&self.progress) != FirstFillingProgress::Ended
    }
}

impl Default for FirstFilling {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
