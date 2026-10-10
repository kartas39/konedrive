//! A directory one side removed, and what the other side has in it.
//!
//! Removed here: its removal is one removal in OneDrive for all OneDrive has in it, and
//! stands whatever OneDrive changed there; it waits only for what was moved out of the
//! directory here, so that such an item is not removed with it.
//!
//! Removed in OneDrive: it goes here once the folder shows nothing in it; where what it
//! holds is the user's own, it stays, gives up its binding and is sent again. A directory
//! that has a binding and that no listing told of gives its binding up in the same order.
//!
//! A directory that gives up its binding: what the two sides agreed is in it, and what
//! OneDrive has in it, is then in the row that takes the binding, so the steps of those
//! items wait for it.

use std::collections::HashMap;

use super::item_decision::{ItemCase, ItemDecision, ItemDecisions, LocalPlaceAvailability};
use super::plan_items::PlanItems;
use super::sync_step::{ItemVersions, OneDriveStep, RowStep, SyncStep};
use super::WaitReason;
use crate::model::{Item, ItemKind, ItemPlace, LocalItemId};

/// Whether `item` is a directory that was removed here and that OneDrive has.
fn is_directory_removed_here(item: &Item) -> bool {
    item.kind == ItemKind::Directory && item.local.is_none() && item.onedrive.is_some() && item.synced_place.is_some()
}

/// Whether a directory OneDrive has above `item`, at any depth, was removed here.
pub(crate) fn is_in_directory_removed_here(items: &PlanItems<'_>, item: &Item) -> bool {
    let directory_of = |item: &Item| item.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref()).and_then(|place| items.find(place.parent_local_item_id));
    let mut above = directory_of(item);
    for _ in 0..items.count() {
        match above {
            Some(directory) if is_directory_removed_here(directory) => return true,
            Some(directory) => above = directory_of(directory),
            None => return false,
        }
    }
    false
}

/// Makes the removal of each directory removed here, the outermost of those removed in one
/// another, one removal for all OneDrive has in it.
///
/// - An item OneDrive has in it that the folder shows is not removed with it: it was moved
///   out of the directory here, or OneDrive moved it into the directory, where the folder
///   cannot follow. Its place here is sent first, and the removal waits for it; likewise
///   for an item a step is running for.
/// - Every other item OneDrive has in it goes with the removal, and waits for the
///   directory: what was removed here with it, what OneDrive made in it since, and a file
///   removed here whose content OneDrive changed, which does not come back. The step names
///   those of them that were removed here.
pub(crate) fn send_one_removal_for_each_directory(cases: &[ItemCase<'_>], items: &PlanItems<'_>, decisions: &mut ItemDecisions) {
    for case in cases {
        let directory = case.item.local_item_id;
        if !is_directory_removed_here(case.item) || is_in_directory_removed_here(items, case.item) {
            continue;
        }
        let removal = match decisions.find(directory) {
            Some(ItemDecision::Step(SyncStep::OneDrive(OneDriveStep::SendDirectoryRemoval { item, .. }))) => Some(item.clone()),
            Some(ItemDecision::Wait(WaitReason::User(_) | WaitReason::Time { .. })) => None,
            _ => continue,
        };
        let mut kept: Option<LocalItemId> = None;
        let mut removed_with_it = Vec::new();
        for below in items.list_below_in_onedrive(directory) {
            let Some(row) = items.find(below) else { continue };
            if row.local.is_some() || matches!(decisions.find(below), Some(ItemDecision::Wait(WaitReason::RunningStep))) {
                kept.get_or_insert(below);
                continue;
            }
            decisions.replace(below, ItemDecision::Wait(WaitReason::AnotherItem(directory)));
            if row.synced_place.is_some() || row.synced_content.is_some() {
                removed_with_it.push(ItemVersions::of(row));
            }
        }
        if let Some(item) = removal {
            decisions.replace(
                directory,
                match kept {
                    Some(kept) => ItemDecision::Wait(WaitReason::AnotherItem(kept)),
                    None => ItemDecision::Step(SyncStep::OneDrive(OneDriveStep::SendDirectoryRemoval { item, below: removed_with_it })),
                },
            );
        }
    }
}

/// Decides each directory the folder shows that OneDrive does not have, though the directory
/// has a binding — OneDrive removed it, or no listing told of it — and in which the folder
/// still shows something.
///
/// - An item in it that the two sides never agreed on in it (made in it, or moved into it
///   here) is the user's own and stays; and so does one they agreed on there whose place
///   here is to be sent, and waits for the directory to be in OneDrive.
/// - While the folder shows anything else in it, the directory waits for that item: the
///   clean items go first, and an item whose content was changed here gives up its binding
///   first. A directory that gave up its binding before them would have them read as moved
///   here, and sent again.
/// - Once all the folder shows in it stays, the directory gives up its binding; the plan
///   made after it sends it as a new one, and then what is in it.
pub(crate) fn keep_directories_that_hold_the_users_own(cases: &[ItemCase<'_>], items: &PlanItems<'_>, decisions: &mut ItemDecisions) {
    let case_of: HashMap<LocalItemId, &ItemCase<'_>> = cases.iter().map(|case| (case.item.local_item_id, case)).collect();
    for case in cases {
        let (item, directory) = (case.item, case.item.local_item_id);
        let is_not_in_onedrive = item.kind == ItemKind::Directory && item.local.is_some() && item.onedrive.is_none() && item.onedrive_item.is_some();
        let is_decided_here = matches!(decisions.find(directory), Some(ItemDecision::Wait(WaitReason::AnotherItem(_)) | ItemDecision::Step(SyncStep::Row(RowStep::GiveUpBinding { .. }))));
        if !is_not_in_onedrive || !is_decided_here {
            continue;
        }
        // Nothing of an item goes with the binding when neither side had it in the directory
        // when they last agreed and OneDrive does not have it there now. An item that was
        // agreed on there stays only when it is sent into the directory again.
        let is_in_it = |place: Option<&ItemPlace>| place.is_some_and(|place| place.parent_local_item_id == directory);
        let waits_to_be_sent_into_it = |shown: LocalItemId| {
            matches!(decisions.find(shown), Some(ItemDecision::Wait(WaitReason::AnotherItem(waited_for))) if *waited_for == directory)
                && case_of.get(&shown).is_some_and(|case| case.local_place_availability != LocalPlaceAvailability::NameTaken(directory))
        };
        let stays = |shown: &LocalItemId| {
            items.find(*shown).is_some_and(|row| (!is_in_it(row.synced_place.as_ref()) && !is_in_it(row.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref()))) || waits_to_be_sent_into_it(*shown))
        };
        let shown = items.list_shown_in(directory);
        let decision = match shown.iter().find(|shown| !stays(shown)) {
            Some(goes_first) => ItemDecision::Wait(WaitReason::AnotherItem(*goes_first)),
            None if shown.is_empty() => continue,
            None => ItemDecision::Step(SyncStep::Row(RowStep::GiveUpBinding { item: ItemVersions::of(item) })),
        };
        decisions.replace(directory, decision);
    }
}

/// Lets every step wait whose item the two sides agreed is in a directory that gives up its
/// binding in this plan, or that OneDrive has in it: the commit of that directory's step
/// writes those places into the rows of such items (they follow the binding to the row
/// that takes it), and a step that ran beside it would find its row changed. Those are
/// items the folder does not show in the directory: what it shows there went before, or
/// stays.
pub(crate) fn wait_for_directories_that_give_up_their_binding(cases: &[ItemCase<'_>], decisions: &mut ItemDecisions) {
    let gives_up_its_binding = |decisions: &ItemDecisions, directory: LocalItemId| matches!(decisions.find(directory), Some(ItemDecision::Step(SyncStep::Row(RowStep::GiveUpBinding { .. }))));
    for case in cases {
        let (item, id) = (case.item, case.item.local_item_id);
        if !matches!(decisions.find(id), Some(ItemDecision::Step(_))) {
            continue;
        }
        let directories = [item.synced_place.as_ref(), item.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref())].map(|place| place.map(|place| place.parent_local_item_id));
        if let Some(directory) = directories.into_iter().flatten().find(|directory| *directory != id && gives_up_its_binding(decisions, *directory)) {
            decisions.replace(id, ItemDecision::Wait(WaitReason::AnotherItem(directory)));
        }
    }
}
