//! Items that wait for each other's names: two that exchanged names, three in a circle, a
//! directory made at the name of an item that was moved into it. None can take its name
//! while another still has it, so one of them goes through a temporary name, on the side
//! the names were changed on.

use std::collections::BTreeMap;
use std::ffi::OsString;

use super::item_decision::{ItemCase, ItemDecision, ItemDecisions, LocalPlaceAvailability};
use super::plan_items::PlanItems;
use super::side_change::{PlaceChange, SideChange};
use super::sync_step::{FolderStep, ItemVersions, OneDriveStep, SyncStep};
use super::WaitReason;
use crate::folded_name::FoldedName;
use crate::model::LocalItemId;

/// How many temporary names are tried for one item.
const TEMPORARY_NAME_TRIES: u32 = 100;

/// The `attempt`th temporary name of `item`, counted from 1. The item's id makes it another
/// name than any other item's.
pub(crate) fn make_temporary_name(item: LocalItemId, attempt: u32) -> OsString {
    if attempt <= 1 {
        format!(".konedrive-swap-{item}").into()
    } else {
        format!(".konedrive-swap-{item}-{attempt}").into()
    }
}

/// The first temporary name of `item` that neither side has in `directory`, as OneDrive
/// compares names.
fn find_free_temporary_name(items: &PlanItems<'_>, item: LocalItemId, directory: LocalItemId) -> Option<OsString> {
    (1..=TEMPORARY_NAME_TRIES).map(|attempt| make_temporary_name(item, attempt)).find(|name| {
        let folded = FoldedName::of(name);
        items.list_shown_with_name(directory, &folded).is_empty() && items.list_in_onedrive_with_name(directory, &folded).is_empty()
    })
}

/// The items that wait for one another in a circle that `item` is in: `item`, the item it
/// waits for, and so on until one waits for `item` again. `None` when the way does not come
/// back to `item`.
fn find_circle_of_waits(decisions: &ItemDecisions, item: LocalItemId) -> Option<Vec<LocalItemId>> {
    let mut circle = vec![item];
    loop {
        let Some(ItemDecision::Wait(WaitReason::AnotherItem(another))) = decisions.find(*circle.last()?) else { return None };
        if *another == item {
            return Some(circle);
        }
        if circle.contains(another) {
            return None;
        }
        circle.push(*another);
    }
}

/// The step that takes the item of `case` out of a circle of waits through a temporary
/// name; `None` when the item waits for something else than a name.
///
/// - Its sending waits for the name (`sending` is what it would send): the same sending,
///   under a temporary name in the same directory of OneDrive.
/// - OneDrive moved it to a name the folder shows another item under: it is renamed here to
///   a temporary name, in the directory the folder shows it in.
fn go_through_temporary_name(case: &ItemCase<'_>, items: &PlanItems<'_>, sending: Option<&OneDriveStep>, decision: Option<&ItemDecision>) -> Option<SyncStep> {
    let id = case.item.local_item_id;
    let directory_here = case.item.local.as_ref().and_then(|local| local.place.as_ref()).map(|place| place.parent_local_item_id);
    if let Some(sending) = sending {
        let name = find_free_temporary_name(items, id, directory_here?)?;
        return Some(SyncStep::OneDrive(match sending.clone() {
            OneDriveStep::MakeDirectory { item, parent, .. } => OneDriveStep::MakeDirectory { item, parent, name },
            OneDriveStep::SendNewFile { item, parent, .. } => OneDriveStep::SendNewFile { item, parent, name },
            OneDriveStep::SendMove { item, parent, .. } => OneDriveStep::SendMove { item, parent, name },
            OneDriveStep::SendContent { .. } | OneDriveStep::SendFileRemoval { .. } | OneDriveStep::SendDirectoryRemoval { .. } => return None,
        }));
    }
    let follows_onedrive = matches!(SideChange::of_local(case.item), SideChange::IsThere { place: PlaceChange::Same, .. }) && matches!(SideChange::of_onedrive(case.item), SideChange::IsThere { place: PlaceChange::Moved, .. });
    match (case.local_place_availability, decision) {
        (LocalPlaceAvailability::NameTaken(holder), Some(ItemDecision::Wait(WaitReason::AnotherItem(waited_for)))) if holder == *waited_for && follows_onedrive => {
            let name = find_free_temporary_name(items, id, directory_here?)?;
            Some(SyncStep::Folder(FolderStep::MoveToTemporaryName { item: ItemVersions::of(case.item), name }))
        }
        _ => None,
    }
}

/// Breaks every circle of items that wait for one another's names: of the items of a circle
/// that can go through a temporary name, the one with the lowest id does. `sendings` are
/// the steps towards OneDrive that wait for a name another item leaves, by item.
pub(crate) fn break_circles_of_names(cases: &[ItemCase<'_>], items: &PlanItems<'_>, sendings: &BTreeMap<LocalItemId, OneDriveStep>, decisions: &mut ItemDecisions) {
    let case_of: BTreeMap<LocalItemId, &ItemCase<'_>> = cases.iter().map(|case| (case.item.local_item_id, case)).collect();
    // Each circle is broken once; no more circles than items.
    for id in case_of.keys() {
        let Some(circle) = find_circle_of_waits(decisions, *id) else { continue };
        let through_temporary_name = circle.iter().filter_map(|member| Some((*member, go_through_temporary_name(case_of.get(member)?, items, sendings.get(member), decisions.find(*member))?))).min_by_key(|(member, _)| *member);
        if let Some((member, step)) = through_temporary_name {
            decisions.replace(member, ItemDecision::Step(step));
        }
    }
}
