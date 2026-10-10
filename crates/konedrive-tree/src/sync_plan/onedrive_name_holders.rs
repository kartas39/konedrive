//! The items the rows say OneDrive has under one name of a directory, as OneDrive compares
//! names. Once a listing has ended there is one. While a listing is being written, page by
//! page, two rows may say one name — the page that moves the first away is still to come —
//! and then only one of them is brought to that place here: the others wait for the
//! listing.

use super::item_decision::{ItemCase, ItemDecision, ItemDecisions};
use super::plan_items::PlanItems;
use super::sync_step::{FolderStep, SyncStep};
use super::{PlanFault, WaitReason};
use crate::folded_name::FoldedName;

/// Of the items OneDrive is said to have under one name of a directory, the one with the
/// lowest id alone is shown or moved here to that place by this plan. Every other one
/// waits for the listing that is under way; with none under way the rows say what OneDrive
/// cannot have, and a full listing mends them.
pub(crate) fn bring_one_holder_of_each_name_here(cases: &[ItemCase<'_>], items: &PlanItems<'_>, decisions: &mut ItemDecisions) {
    for case in cases {
        let id = case.item.local_item_id;
        let comes_to_onedrives_place = matches!(decisions.find(id), Some(ItemDecision::Step(SyncStep::Folder(FolderStep::ShowItem { .. } | FolderStep::MoveItem { .. } | FolderStep::TakeMoveBack { .. }))));
        let Some(place) = case.item.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref()).filter(|_| comes_to_onedrives_place) else { continue };
        let has_earlier_holder = items.list_in_onedrive_with_name(place.parent_local_item_id, &FoldedName::of(&place.name)).iter().any(|holder| *holder < id);
        if has_earlier_holder {
            decisions.replace(id, if case.is_listing_under_way { ItemDecision::Wait(WaitReason::Listing) } else { ItemDecision::Fault(PlanFault::FullListingNeeded(id)) });
        }
    }
}
