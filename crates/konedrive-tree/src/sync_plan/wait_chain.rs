//! The items that wait for one another, followed from each to the one it waits for, and
//! where that ends: at an item's step, or at its own wait for a time, for the user, for a
//! step that is running or for a listing. A wait that ends at none of these is not left
//! standing: the item was not given, and the caller gives it; or a listing is still being
//! written; or the plan says it is at fault.

use konedrive_reason::Reason;

use super::item_decision::{ItemAnswers, ItemDecision, OverruledAnswers, Overruling};
use super::plan_items::PlanItems;
use super::{ExclusionReason, PlanFault, WaitReason};
use crate::model::LocalItemId;

/// What an item waits for, when it waits for another item.
fn find_item_waited_for(answers: &OverruledAnswers<'_>, item: LocalItemId) -> Option<LocalItemId> {
    match answers.find(item) {
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
pub(crate) fn end_waits_for_other_items(is_listing_under_way: bool, items: &PlanItems<'_>, answers: &ItemAnswers) -> Vec<Overruling> {
    let mut answers = OverruledAnswers::of(answers);
    let waiting: Vec<LocalItemId> = answers.items().iter().copied().filter(|item| find_item_waited_for(&answers, *item).is_some()).collect();
    // What an item waits for directly, until nothing changes: an item whose wait was given
    // an end here ends the waits of the items that wait for it.
    for _ in 0..=waiting.len() {
        let mut ended = Vec::new();
        for item in &waiting {
            let Some(another) = find_item_waited_for(&answers, *item) else { continue };
            let decision = match answers.find(another) {
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
            answers.overrule(item, decision);
        }
    }
    // What is left ends at a step, at a wait that ends or at a fault, unless it goes round.
    let in_circle: Vec<LocalItemId> = waiting
        .iter()
        .copied()
        .filter(|item| {
            let mut at = *item;
            (0..=waiting.len()).all(|_| find_item_waited_for(&answers, at).map(|another| at = another).is_some())
        })
        .collect();
    for item in in_circle {
        answers.overrule(item, end_wait_without_end(is_listing_under_way, item));
    }
    answers.into_overrulings()
}

/// The items whose wait ends at the user: an item that waits for the user itself, and one
/// that waits for another item whose wait, followed from item to item, ends so. A wait for
/// the user may have no end; what else is to do for such an item is not left behind it
/// ([`decide_content_while_place_waits`](super::item_decision::decide_content_while_place_waits)).
pub(crate) fn list_items_that_wait_for_the_user(answers: &ItemAnswers) -> Vec<LocalItemId> {
    let ends_at_the_user = |item: &LocalItemId| {
        let mut at = *item;
        for _ in 0..=answers.items().len() {
            match answers.find(at) {
                Some(ItemDecision::Wait(WaitReason::User(_))) => return true,
                Some(ItemDecision::Wait(WaitReason::AnotherItem(another))) => at = *another,
                Some(ItemDecision::Wait(WaitReason::Time { .. } | WaitReason::RunningStep | WaitReason::StartUpCheck | WaitReason::Listing | WaitReason::ItemNotGiven(_) | WaitReason::ItemsInDirectoryNotGiven(_))) => return false,
                Some(ItemDecision::Step(_) | ItemDecision::NothingToDo | ItemDecision::Exclusion(_) | ItemDecision::Fault(_)) | None => return false,
            }
        }
        false
    };
    answers.items().iter().filter(|item| ends_at_the_user(item)).copied().collect()
}
