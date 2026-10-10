//! The items the rows say OneDrive has under one name of a directory.
//!
//! Under the same name, byte for byte, OneDrive has one item: two rows that say otherwise
//! are rows of a listing that is being written, page by page — the page that moves the
//! first away is still to come — or rows a full listing mends.
//!
//! Under two names the plan's own rule folds to one ([`FoldedName`]), OneDrive is the
//! judge: once no listing is under way it has listed both, so they are two items, and each
//! is shown under its own name. While a listing is being written the second waits for it,
//! as the first may only have been renamed.

use super::item_decision::{ItemAnswers, ItemCase, ItemDecision, Overruling};
use super::plan_items::PlanItems;
use super::sync_step::{FolderStep, SyncStep};
use super::{PlanFault, WaitReason};
use crate::folded_name::FoldedName;
use crate::model::LocalItemId;

/// What stands against an item coming to the place OneDrive has it at, among the other
/// items OneDrive is said to have there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EarlierNameHolder {
    /// No row with a lower id has a name that folds to the item's.
    None,
    /// One has the very name.
    OfSameName,
    /// One has another name that folds to the item's.
    OfNameFoldedToOne,
}

/// What becomes of an item that comes to OneDrive's place here, when another row says
/// OneDrive has an item under that name; `None` when it comes.
///
/// - While a listing is under way it waits for the listing, whichever way the names are
///   one.
/// - With none under way, the very name on two rows is what OneDrive cannot have: a full
///   listing mends the rows. Two names that fold to one are two items.
pub(crate) fn decide_second_holder(item: LocalItemId, earlier: EarlierNameHolder, is_listing_under_way: bool) -> Option<ItemDecision> {
    match (earlier, is_listing_under_way) {
        (EarlierNameHolder::None, _) => None,
        (EarlierNameHolder::OfSameName | EarlierNameHolder::OfNameFoldedToOne, true) => Some(ItemDecision::Wait(WaitReason::Listing)),
        (EarlierNameHolder::OfSameName, false) => Some(ItemDecision::Fault(PlanFault::FullListingNeeded(item))),
        (EarlierNameHolder::OfNameFoldedToOne, false) => None,
    }
}

/// Of the items OneDrive is said to have under one name of a directory, the one with the
/// lowest id is shown or moved here to that place by this plan; what becomes of every other
/// one is [`decide_second_holder`].
pub(crate) fn bring_one_holder_of_each_name_here(cases: &[ItemCase<'_>], items: &PlanItems<'_>, answers: &ItemAnswers) -> Vec<Overruling> {
    let mut overrulings = Vec::new();
    for case in cases {
        let id = case.item.local_item_id;
        let comes_to_onedrives_place = matches!(answers.find(id), Some(ItemDecision::Step(SyncStep::Folder(FolderStep::ShowItem { .. } | FolderStep::MoveItem { .. } | FolderStep::TakeMoveBack { .. }))));
        let Some(place) = case.item.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref()).filter(|_| comes_to_onedrives_place) else { continue };
        let earlier: Vec<LocalItemId> = items.list_in_onedrive_with_name(place.parent_local_item_id, &FoldedName::of(&place.name)).into_iter().filter(|holder| *holder < id).collect();
        let has_same_name = |holder: &LocalItemId| items.find(*holder).and_then(|row| row.onedrive.as_ref()).and_then(|onedrive| onedrive.place.as_ref()).is_some_and(|held| held.name == place.name);
        let earlier = match (earlier.is_empty(), earlier.iter().any(has_same_name)) {
            (true, _) => EarlierNameHolder::None,
            (false, true) => EarlierNameHolder::OfSameName,
            (false, false) => EarlierNameHolder::OfNameFoldedToOne,
        };
        overrulings.extend(decide_second_holder(id, earlier, case.is_listing_under_way).map(|decision| Overruling::of(id, decision)));
    }
    overrulings
}
