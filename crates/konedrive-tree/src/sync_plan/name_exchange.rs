//! Items that wait for each other's names: two that exchanged names, three in a circle, a
//! directory made at the name of an item that was moved into it. None can take its name
//! while another still has it, so one of them goes through a temporary name, on the side
//! the names were changed on.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

use super::item_decision::{ItemAnswers, ItemCase, ItemDecision, LocalPlaceAvailability, OverruledAnswers, Overruling};
use super::plan_items::PlanItems;
use super::side_change::{PlaceChange, SideChange};
use super::sync_step::{FolderStep, ItemVersions, OneDriveStep, SyncStep};
use super::WaitReason;
use crate::folded_name::FoldedName;
use crate::model::{ItemPlace, LocalItemId};

/// How many temporary names are tried for one item.
const TEMPORARY_NAME_TRIES: u32 = 100;

/// The `attempt`th temporary name of `item`, counted from 1. The item's id makes it another
/// name than any other item's.
///
/// The caller of the plan asks it too: the rows of what has the first such name in a
/// directory of an item, on either side, are given with the item
/// ([`SyncPlanInput`](super::SyncPlanInput)).
pub fn make_temporary_name(item: LocalItemId, attempt: u32) -> OsString {
    if attempt <= 1 {
        format!(".konedrive-swap-{item}").into()
    } else {
        format!(".konedrive-swap-{item}-{attempt}").into()
    }
}

/// Whether `name` is a temporary name of `item`.
fn is_temporary_name_of(item: LocalItemId, name: &OsStr) -> bool {
    (1..=TEMPORARY_NAME_TRIES).any(|attempt| make_temporary_name(item, attempt) == name)
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
fn find_circle_of_waits(decisions: &OverruledAnswers<'_>, item: LocalItemId) -> Option<Vec<LocalItemId>> {
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

/// The step that takes the item of `case` out of `circle`, items that wait for one another,
/// through a temporary name; `None` when a temporary name of this item ends nothing.
///
/// - Its sending waits for a name (`sending` is what it would send): the same sending,
///   under a temporary name in the same directory of OneDrive.
/// - OneDrive moved it, and the folder shows it at the name another item of the circle
///   waits for, or it waits for the name of an item of the circle in another directory
///   than the folder shows it in: it is moved here to a temporary name. In the directory
///   OneDrive has it in, where the folder can bring it there; in the directory the folder
///   shows it in, where it cannot yet (the folder does not show that directory, or shows
///   it in the item). An item OneDrive removed that is in the way likewise, where it is.
///
/// An item that is under a temporary name of its own in that directory already goes
/// through no other: a circle that a temporary name did not break is left to be said as a
/// fault.
fn go_through_temporary_name(case: &ItemCase<'_>, items: &PlanItems<'_>, sending: Option<&OneDriveStep>, decision: Option<&ItemDecision>, circle: &[(&ItemCase<'_>, Option<&ItemDecision>)]) -> Option<SyncStep> {
    let id = case.item.local_item_id;
    let directory_here = case.item.local.as_ref().and_then(|local| local.place.as_ref()).map(|place| place.parent_local_item_id)?;
    let is_under_temporary_name_in = |place: Option<&ItemPlace>, directory: LocalItemId| place.is_some_and(|place| place.parent_local_item_id == directory && is_temporary_name_of(id, &place.name));
    let place_in_onedrive = case.item.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref());
    let place_here = case.item.local.as_ref().and_then(|local| local.place.as_ref());
    if let Some(sending) = sending {
        if is_under_temporary_name_in(place_in_onedrive, directory_here) {
            return None;
        }
        let name = find_free_temporary_name(items, id, directory_here)?;
        return Some(SyncStep::OneDrive(match sending.clone() {
            OneDriveStep::MakeDirectory { item, parent, .. } => OneDriveStep::MakeDirectory { item, parent, name },
            OneDriveStep::SendNewFile { item, parent, .. } => OneDriveStep::SendNewFile { item, parent, name },
            OneDriveStep::SendMove { item, parent, .. } => OneDriveStep::SendMove { item, parent, name },
            OneDriveStep::SendContent { .. } | OneDriveStep::SendFileRemoval { .. } | OneDriveStep::SendDirectoryRemoval { .. } => return None,
        }));
    }
    let Some(ItemDecision::Wait(WaitReason::AnotherItem(waited_for))) = decision else { return None };
    let is_in_the_way = circle.iter().any(|(other, decision)| {
        other.local_place_availability == LocalPlaceAvailability::NameTaken(id) && matches!(decision, Some(ItemDecision::Wait(WaitReason::AnotherItem(waited_for))) if *waited_for == id)
    });
    let directory_in_onedrive = match SideChange::of_onedrive(case.item) {
        SideChange::IsThere { place: PlaceChange::Moved, .. } => case.item.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref()).map(|place| place.parent_local_item_id)?,
        // What OneDrive removed goes here once nothing is in it: until then it is in the way.
        SideChange::Removed if is_in_the_way && matches!(SideChange::of_local(case.item), SideChange::IsThere { place: PlaceChange::Same, .. }) => directory_here,
        _ => return None,
    };
    let can_be_brought_there = matches!(case.local_place_availability, LocalPlaceAvailability::Free | LocalPlaceAvailability::NameTaken(_)) && directory_in_onedrive != id && !items.is_shown_in(directory_in_onedrive, id);
    let leaves_its_directory = case.local_place_availability == LocalPlaceAvailability::NameTaken(*waited_for) && can_be_brought_there && directory_in_onedrive != directory_here;
    if !is_in_the_way && !leaves_its_directory {
        return None;
    }
    let directory = if can_be_brought_there { directory_in_onedrive } else { directory_here };
    if is_under_temporary_name_in(place_here, directory) {
        return None;
    }
    let name = find_free_temporary_name(items, id, directory)?;
    Some(SyncStep::Folder(FolderStep::MoveToTemporaryName { item: ItemVersions::of(case.item), place: ItemPlace { parent_local_item_id: directory, name } }))
}

/// Breaks every circle of items that wait for one another: of the items of a circle that
/// can go through a temporary name, the one with the lowest id does. `sendings` are the
/// steps towards OneDrive that wait for a name another item leaves, by item.
pub(crate) fn break_circles_of_names(cases: &[ItemCase<'_>], items: &PlanItems<'_>, sendings: &BTreeMap<LocalItemId, OneDriveStep>, answers: &ItemAnswers) -> Vec<Overruling> {
    let mut answers = OverruledAnswers::of(answers);
    let case_of: BTreeMap<LocalItemId, &ItemCase<'_>> = cases.iter().map(|case| (case.item.local_item_id, case)).collect();
    // Each circle is broken once; no more circles than items.
    for id in case_of.keys() {
        let Some(circle) = find_circle_of_waits(&answers, *id) else { continue };
        let members: Vec<(&ItemCase<'_>, Option<&ItemDecision>)> = circle.iter().filter_map(|member| Some((*case_of.get(member)?, answers.find(*member)))).collect();
        let through_temporary_name = members.iter().filter_map(|(member, decision)| Some((member.item.local_item_id, go_through_temporary_name(member, items, sendings.get(&member.item.local_item_id), *decision, &members)?))).min_by_key(|(member, _)| *member);
        if let Some((member, step)) = through_temporary_name {
            answers.overrule(member, ItemDecision::Step(step));
        }
    }
    answers.into_overrulings()
}
