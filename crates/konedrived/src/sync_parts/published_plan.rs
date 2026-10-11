//! What the plan's task last answered for each item it planned: what the bus and the
//! mount read of the plan without asking the task.
//!
//! **State owned:** the last plan's answer per item, and nothing else: which steps run is
//! `RunningSteps`', what holds all sending is `SendingHolds`'. Written by the plan's task
//! alone ([`PublishedPlan::publish`]); read by the queue's answers
//! (`upload_queue`) and by the mount's `user.konedrive.sync`. It is here,
//! below the mount, because the mount reads it and must not know the task.
//!
//! **Answers for:** design §7, "what the window and `konedrivectl` show": the first of the
//! three sources — the plan's answer for each unsent item. An item the last plan was not
//! given has no answer here.
//!
//! **Tested without the rest:**
// test: what_was_published_is_read_back_per_item
// test: a_new_plan_replaces_the_answers_of_the_one_before
#![allow(dead_code, unused_variables)]

use std::collections::BTreeMap;

use konedrive_tree::{ExclusionReason, LocalItemId, PlanFault, WaitReason};

use super::running_steps::StepClass;

/// What the plan answers for one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemAnswer {
    /// A step of this class is planned and can run.
    Planned(StepClass),
    /// It waits, as the plan said: for another item, a time, the user, a running step, a
    /// listing. What the steps remember and what the check at start holds are among the
    /// plan's own waits: the plan is told them.
    Waiting(WaitReason),
    /// The plan leaves it out by a rule.
    Excluded(ExclusionReason),
    /// The plan is at fault about it.
    Fault(PlanFault),
}

/// The plan's answers of one folder.
pub struct PublishedPlan {
    answers: std::sync::RwLock<BTreeMap<LocalItemId, ItemAnswer>>,
    /// For each directory whose removal the last plan holds for the user
    /// (`WaitReason::User(Reason::MassDelete)`): how many items OneDrive has in it and
    /// below it, as the plan was given it (`SyncPlanInput::onedrive_item_count_below`).
    held_removal_counts: std::sync::RwLock<BTreeMap<LocalItemId, u64>>,
}

impl PublishedPlan {
    /// Nothing published: before the first plan. Made by the folder's start and given to
    /// the mount's setup and to the plan's task.
    pub fn new() -> Self {
        PublishedPlan { answers: std::sync::RwLock::default(), held_removal_counts: std::sync::RwLock::default() }
    }

    /// The answers of the plan made now replace those of the one before. Called by the
    /// plan's task after each plan.
    pub fn publish(&self, answers: BTreeMap<LocalItemId, ItemAnswer>) {
        *crate::panic::write(&self.answers) = answers;
    }

    /// The counts of the plan made now replace those of the one before: for each
    /// directory whose removal that plan holds for the user, how many items the removal
    /// takes from OneDrive below it — the plan's own count
    /// (`PlannedRound::input.onedrive_item_count_below`, of the plan's reading). Called by
    /// the plan's task right after [`publish`](Self::publish), with every plan, also with
    /// none held.
    #[allow(unused_variables)]
    pub fn publish_held_removal_counts(&self, counts: BTreeMap<LocalItemId, u64>) {
        todo!()
    }

    /// How many items the held removal of the directory `item` takes from OneDrive below
    /// it; `None` for an item that is no directory whose removal the last plan holds.
    /// Called by the queue's answers: a held mass removal of a directory is ONE entry,
    /// the directory, with this number (`upload_queue::UnsentEntry::held_items_below`).
    #[allow(unused_variables)]
    pub fn find_held_removal_count(&self, item: LocalItemId) -> Option<u64> {
        todo!()
    }

    /// Every count of [`find_held_removal_count`](Self::find_held_removal_count), by
    /// directory: taken once, with the answers, before a read of the store.
    pub fn list_held_removal_counts(&self) -> BTreeMap<LocalItemId, u64> {
        todo!()
    }

    /// The answer for `item`, when the last plan was given it. Called by the queue's
    /// answers per entry and by the mount for the sync mark.
    pub fn find_answer(&self, item: LocalItemId) -> Option<ItemAnswer> {
        crate::panic::read(&self.answers).get(&item).cloned()
    }

    /// Every answer, by item. Called by the queue's counts and summaries.
    pub fn list_answers(&self) -> BTreeMap<LocalItemId, ItemAnswer> {
        crate::panic::read(&self.answers).clone()
    }
}

#[cfg(test)]
mod tests;
