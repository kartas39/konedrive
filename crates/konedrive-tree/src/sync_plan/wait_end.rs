//! Where a wait for another item ends: at that item's step, or at its own wait for a time,
//! for the user, for a step that is running or for a listing. A wait that ends at none of
//! these is not left standing: the item was not given, and the caller gives it; or a
//! listing is still being written; or the plan says it is at fault.

use konedrive_reason::Reason;

use super::item_decision::{ItemDecision, ItemDecisions};
use super::plan_items::PlanItems;
use super::{ExclusionReason, PlanFault, WaitReason};
use crate::model::LocalItemId;

/// What an item waits for, when it waits for another item.
fn find_item_waited_for(decisions: &ItemDecisions, item: LocalItemId) -> Option<LocalItemId> {
    match decisions.find(item) {
        Some(ItemDecision::Wait(WaitReason::AnotherItem(another))) => Some(*another),
        _ => None,
    }
}

/// The answer of an item whose wait for another item nothing ends: while a listing is being
/// written the rows may yet change, and otherwise the plan is at fault.
fn end_wait_without_end(is_listing_under_way: bool, item: LocalItemId) -> ItemDecision {
    if is_listing_under_way {
        ItemDecision::Wait(WaitReason::Listing)
    } else {
        ItemDecision::Fault(PlanFault::WaitWithoutEnd(item))
    }
}

/// Gives every item that waits for another item a wait that ends, or says it has none.
///
/// - The other item was not given: the item says which, for the caller to give.
/// - The other item is left here, and so never in OneDrive: the item waits for the user, as
///   for a directory that is not in OneDrive.
/// - The other item has nothing to do, or a rule leaves it out, or the items wait for one
///   another in a circle nothing broke: a listing under way may yet say otherwise;
///   with none, a fault.
pub(crate) fn end_waits_for_other_items(is_listing_under_way: bool, items: &PlanItems<'_>, decisions: &mut ItemDecisions) {
    let waiting: Vec<LocalItemId> = decisions.items().iter().copied().filter(|item| find_item_waited_for(decisions, *item).is_some()).collect();
    // What an item waits for directly, until nothing changes: an item whose wait was given
    // an end here ends the waits of the items that wait for it.
    for _ in 0..=waiting.len() {
        let mut ended = Vec::new();
        for item in &waiting {
            let Some(another) = find_item_waited_for(decisions, *item) else { continue };
            let decision = match decisions.find(another) {
                None => ItemDecision::Wait(WaitReason::ItemNotGiven(another)),
                Some(_) if items.find(another).is_none() => ItemDecision::Wait(WaitReason::ItemNotGiven(another)),
                Some(ItemDecision::Exclusion(ExclusionReason::NameIgnored)) if !is_listing_under_way => ItemDecision::Wait(WaitReason::User(Reason::Parent)),
                Some(ItemDecision::NothingToDo | ItemDecision::Exclusion(_)) => end_wait_without_end(is_listing_under_way, *item),
                Some(ItemDecision::Step(_) | ItemDecision::Wait(_) | ItemDecision::Fault(_)) => continue,
            };
            ended.push((*item, decision));
        }
        if ended.is_empty() {
            break;
        }
        for (item, decision) in ended {
            decisions.replace(item, decision);
        }
    }
    // What is left ends at a step, at a wait that ends or at a fault, unless it goes round.
    let in_circle: Vec<LocalItemId> = waiting
        .iter()
        .copied()
        .filter(|item| {
            let mut at = *item;
            (0..=waiting.len()).all(|_| find_item_waited_for(decisions, at).map(|another| at = another).is_some())
        })
        .collect();
    for item in in_circle {
        decisions.replace(item, end_wait_without_end(is_listing_under_way, item));
    }
}
