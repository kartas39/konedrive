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
/// (`Reason::waits_for_space`) nor is of the group `waiting` (`Reason::group`).
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

/// Whether a wait for the user with `reason` is shown as blocked: not the reason of an item the check at start left
/// (`Reason::NotReadOnDisk`) nor of one the plan is at fault about
/// (`Reason::BeingChecked`) — the owner's decision, which comes before the groups —, not
/// a wait for space, and not a reason of the group `waiting`. Pure.
fn decide_is_reason_shown_blocked(reason: &Reason) -> bool {
    match reason {
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

/// The plan's answers of one folder.
pub struct PublishedPlan {
    answers: std::sync::RwLock<BTreeMap<LocalItemId, ItemAnswer>>,
}

impl PublishedPlan {
    /// Nothing published: before the first plan. Made by the folder's start and given to
    /// the mount's setup and to the plan's task.
    pub fn new() -> Self {
        PublishedPlan { answers: std::sync::RwLock::default() }
    }

    /// The answers of the plan made now replace those of the one before. Called by the
    /// plan's task after each plan.
    pub fn publish(&self, answers: BTreeMap<LocalItemId, ItemAnswer>) {
        *crate::panic::write(&self.answers) = answers;
    }

    /// The answer for `item`, when the last plan was given it. Called by the queue's
    /// answers per entry and by the mount for the sync mark.
    pub fn find_answer(&self, item: LocalItemId) -> Option<ItemAnswer> {
        crate::panic::read(&self.answers).get(&item).cloned()
    }

    /// Every answer, by item.
    pub fn list_answers(&self) -> BTreeMap<LocalItemId, ItemAnswer> {
        crate::panic::read(&self.answers).clone()
    }
}

#[cfg(test)]
mod tests;
