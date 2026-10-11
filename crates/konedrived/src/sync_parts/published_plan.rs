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

use konedrive_tree::{ExclusionReason, Group, LocalItemId, PlanFault, Reason, WaitReason};

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

/// Whether an unsent item is shown as BLOCKED — needing the user —, the one rule for the
/// queue's word (`upload_queue::decide_queue_state`) and for the emblem
/// (`mount::item_lookup::decide_sync_mark`). Pure. True when the step memory keeps a
/// refusal for good of the item as it is (`has_refusal_for_good`:
/// `ItemMemory::refusal` is some, given to the last plan or not), or `answer` is
/// `WaitReason::User(reason)` with a reason that neither waits for space
/// (`Reason::waits_for_space`) nor is of the group `waiting` (`Reason::group`), and is
/// not the mass removal's (`Reason::MassDelete`: that is held, a state of its own).
/// False for everything else: a file open for writing, a wait for space, a read-only
/// folder's wait, an item the check at start left and one the plan is at fault about
/// (the owner's decision: neither is called blocked), every wait for a time, another
/// item or a listing, a planned step, and no answer. Both callers give the two values
/// from one read: the queue from its entry (`UnsentEntry::answer`, `memory`), the mount
/// in the read of the row for `user.konedrive.sync` (`ItemReader::find_item_memory` in
/// `ItemLookup::read_xattr_facts`, with `PublishedPlan::find_answer`).
pub fn decide_is_shown_blocked(answer: Option<&ItemAnswer>, has_refusal_for_good: bool) -> bool {
    if has_refusal_for_good {
        return true;
    }
    match answer {
        None => false,
        Some(ItemAnswer::Planned(_)) => false,
        Some(ItemAnswer::Excluded(ExclusionReason::NameIgnored | ExclusionReason::CannotBeShown)) => false,
        // An item an upload left in OneDrive is the daemon's own to remove: nothing for the user to do.
        Some(ItemAnswer::Excluded(ExclusionReason::LeftByUpload)) => false,
        // The owner's decision: an item the plan is at fault about is being checked, not blocked.
        Some(ItemAnswer::Fault(_)) => false,
        Some(ItemAnswer::Waiting(wait)) => match wait {
            WaitReason::User(reason) => decide_is_reason_shown_blocked(reason),
            // The owner's decision: an item the check at start left could not be read on disk, and is not called blocked.
            WaitReason::StartUpCheck => false,
            WaitReason::AnotherItem(_) | WaitReason::ItemNotGiven(_) | WaitReason::Time { .. } | WaitReason::RunningStep | WaitReason::Listing | WaitReason::ItemsInDirectoryNotGiven(_) => false,
        },
    }
}

/// Whether a wait for the user with `reason` is shown as blocked: not the mass removal's
/// (held, a state of its own), not the reason of an item the check at start left
/// (`Reason::NotReadOnDisk`) nor of one the plan is at fault about
/// (`Reason::BeingChecked`) — the owner's decision, which comes before the groups —, not
/// a wait for space, and not a reason of the group `waiting`. Pure.
fn decide_is_reason_shown_blocked(reason: &Reason) -> bool {
    match reason {
        // Held, a state of its own.
        Reason::MassDelete => return false,
        // The owner's decision, before the rule of the groups: an item the check at start left reads "could not be read on disk" and one the plan is at fault about "being checked", and neither is ever shown blocked, whatever group its reason is of.
        Reason::NotReadOnDisk | Reason::BeingChecked => return false,
        _ => {}
    }
    if reason.waits_for_space() {
        return false;
    }
    match reason.group() {
        Some(Group::Waiting) => false,
        Some(Group::OneAction) => true,
        Some(Group::PerFile) => true,
        Some(Group::Never) => true,
        // A reason in no table needs the user: nothing says it goes up by itself.
        None => true,
    }
}

/// What one plan published: its answers, and its counts of the removals it holds. One
/// value, written and read under one lock, so that no reader has the answers of one plan
/// with the counts of another.
#[derive(Default)]
struct PublishedAnswers {
    answers: BTreeMap<LocalItemId, ItemAnswer>,
    /// For each directory whose removal the plan holds for the user
    /// (`WaitReason::User(Reason::MassDelete)`): how many items OneDrive has in it and
    /// below it, as the plan was given it (`SyncPlanInput::onedrive_item_count_below`).
    held_removal_counts: BTreeMap<LocalItemId, u64>,
}

/// The plan's answers of one folder.
pub struct PublishedPlan {
    published: std::sync::RwLock<PublishedAnswers>,
}

impl PublishedPlan {
    /// Nothing published: before the first plan. Made by the folder's start and given to
    /// the mount's setup and to the plan's task.
    pub fn new() -> Self {
        PublishedPlan { published: std::sync::RwLock::default() }
    }

    /// The answers of the plan made now replace those of the one before, and with them,
    /// in the same write, its counts of held removals: for each directory whose removal
    /// that plan holds for the user, how many items the removal takes from OneDrive below
    /// it — the plan's own count (`PlannedRound::input.onedrive_item_count_below`, of the
    /// plan's reading); empty with none held. Called by the plan's task after each plan:
    /// the ONE publish, so that a reader between two plans has both of one plan.
    pub fn publish(&self, answers: BTreeMap<LocalItemId, ItemAnswer>, held_removal_counts: BTreeMap<LocalItemId, u64>) {
        *crate::panic::write(&self.published) = PublishedAnswers { answers, held_removal_counts };
    }

    /// How many items the held removal of the directory `item` takes from OneDrive below
    /// it; `None` for an item that is no directory whose removal the last plan holds.
    /// Called by the queue's answers: a held mass removal of a directory is ONE entry,
    /// the directory, with this number (`upload_queue::UnsentEntry::held_items_below`).
    pub fn find_held_removal_count(&self, item: LocalItemId) -> Option<u64> {
        crate::panic::read(&self.published).held_removal_counts.get(&item).copied()
    }

    /// Every count of [`find_held_removal_count`](Self::find_held_removal_count), by
    /// directory.
    pub fn list_held_removal_counts(&self) -> BTreeMap<LocalItemId, u64> {
        crate::panic::read(&self.published).held_removal_counts.clone()
    }

    /// The answer for `item`, when the last plan was given it. Called by the queue's
    /// answers per entry and by the mount for the sync mark.
    pub fn find_answer(&self, item: LocalItemId) -> Option<ItemAnswer> {
        crate::panic::read(&self.published).answers.get(&item).cloned()
    }

    /// Every answer, by item.
    pub fn list_answers(&self) -> BTreeMap<LocalItemId, ItemAnswer> {
        crate::panic::read(&self.published).answers.clone()
    }

    /// Every answer and every count of a held removal, of ONE plan: both under one read.
    /// What a reader of the bus takes once, before its read of the store
    /// (`account_folder::outbox`), and the plan's task for the queue's status.
    pub fn list_answers_and_held_removal_counts(&self) -> (BTreeMap<LocalItemId, ItemAnswer>, BTreeMap<LocalItemId, u64>) {
        let published = crate::panic::read(&self.published);
        (published.answers.clone(), published.held_removal_counts.clone())
    }
}

#[cfg(test)]
mod tests;
