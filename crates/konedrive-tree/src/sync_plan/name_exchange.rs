//! Items that wait for each other's names: two that exchanged names, three in a circle, a
//! directory made at the name of an item that was moved into it. None can take its name
//! while another still has it, so one of them goes through a temporary name, on the side
//! the names were changed on.
//!
//! And the row a listing made of a new item under its temporary name, when the answer of
//! the request that made it there was lost: the two rows are one item.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

use super::item_decision::{ItemAnswers, ItemCase, ItemDecision, LocalPlaceAvailability, OverruledAnswers, Overruling};
use super::plan_items::PlanItems;
use super::side_change::{PlaceChange, SideChange};
use super::sync_step::{FolderStep, ItemVersions, OneDriveStep, RowStep, SyncStep};
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

/// An item that keeps a temporary name, and the row a listing made of what OneDrive has
/// under that name: a row the folder does not show, which waits for the item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ListedTemporaryName {
    /// The item has a binding: OneDrive has it as another item than the listed one.
    pub(crate) has_item_binding: bool,
    /// The two are both files, or both directories.
    pub(crate) is_one_kind: bool,
    /// Nothing is agreed on of the listed row: it is new in OneDrive.
    pub(crate) is_row_new_in_onedrive: bool,
    pub(crate) is_listing_under_way: bool,
}

/// What becomes of an item that keeps a temporary name a listed row is at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListedTemporaryNameOutcome {
    /// The listed row is the item itself, made in OneDrive by a request of this folder
    /// whose answer was lost: the two rows are merged.
    IsMerged,
    /// A listing is being written, and may yet say what OneDrive has under the name is
    /// elsewhere or gone.
    WaitsForListing,
    /// The listed row is not the item: the item's answer stands.
    IsAnotherItem,
}

/// What becomes of the item of `listed`. An item with no binding and a new row of OneDrive
/// of its kind under its temporary name are one item, once no listing is under way: the
/// name holds the item's id, and nothing but a request of this folder gave it. Sent again,
/// the item would be made in OneDrive a second time, under the next temporary name.
pub(crate) fn decide_listed_temporary_name(listed: &ListedTemporaryName) -> ListedTemporaryNameOutcome {
    if listed.has_item_binding || !listed.is_one_kind || !listed.is_row_new_in_onedrive {
        ListedTemporaryNameOutcome::IsAnotherItem
    } else if listed.is_listing_under_way {
        ListedTemporaryNameOutcome::WaitsForListing
    } else {
        ListedTemporaryNameOutcome::IsMerged
    }
}

/// Merges each item that keeps a temporary name with the row a listing made of it under
/// that name ([`decide_listed_temporary_name`]): of the rows that wait for such an item
/// ([`LocalPlaceAvailability::IsTemporaryNameOf`]), the one with the lowest id. The item
/// is merged in place of whatever it was answered, but a merge with another item; the
/// listed row goes on waiting for it. The plans made after the merge send the item on from
/// its temporary name to its place here.
pub(crate) fn merge_rows_listed_under_temporary_names(cases: &[ItemCase<'_>], answers: &ItemAnswers) -> Vec<Overruling> {
    let mut answers = OverruledAnswers::of(answers);
    let case_of: BTreeMap<LocalItemId, &ItemCase<'_>> = cases.iter().map(|case| (case.item.local_item_id, case)).collect();
    for (id, case) in &case_of {
        let LocalPlaceAvailability::IsTemporaryNameOf(keeper) = case.local_place_availability else { continue };
        let Some(keeper_case) = case_of.get(&keeper) else { continue };
        let waits_for_keeper = matches!(answers.find(*id), Some(ItemDecision::Wait(WaitReason::AnotherItem(waited_for))) if *waited_for == keeper);
        let is_merged_already = matches!(answers.find(keeper), Some(ItemDecision::Step(SyncStep::Row(RowStep::MergeItems { .. }))));
        if !waits_for_keeper || is_merged_already || keeper_case.item.local.is_none() {
            continue;
        }
        let listed = ListedTemporaryName {
            has_item_binding: keeper_case.item.onedrive_item.is_some(),
            is_one_kind: keeper_case.item.kind == case.item.kind,
            is_row_new_in_onedrive: case.item.synced_place.is_none() && case.item.synced_content.is_none(),
            is_listing_under_way: case.is_listing_under_way,
        };
        match decide_listed_temporary_name(&listed) {
            ListedTemporaryNameOutcome::IsMerged => answers.overrule(keeper, ItemDecision::Step(SyncStep::Row(RowStep::MergeItems { local: ItemVersions::of(keeper_case.item), onedrive: ItemVersions::of(case.item) }))),
            ListedTemporaryNameOutcome::WaitsForListing => answers.overrule(keeper, ItemDecision::Wait(WaitReason::Listing)),
            ListedTemporaryNameOutcome::IsAnotherItem => {}
        }
    }
    answers.into_overrulings()
}
