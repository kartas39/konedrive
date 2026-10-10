//! A directory one side removed, and what the other side has in it.
//!
//! Removed here: its removal is one removal in OneDrive for all OneDrive has in it, and
//! stands whatever OneDrive changed there; it waits only for what was moved out of the
//! directory here, so that such an item is not removed with it.
//!
//! Removed in OneDrive: it goes here once the folder shows nothing in it; where what it
//! holds is the user's own, it stays, gives up its binding and is sent again. A directory
//! that has a binding and that no listing told of gives its binding up in the same order.

use super::item_decision::{ItemCase, ItemDecision, ItemDecisions};
use super::plan_items::PlanItems;
use super::sync_step::{ItemVersions, OneDriveStep, RowStep, SyncStep};
use super::{ExclusionReason, WaitReason};
use crate::model::{Item, ItemKind, LocalItemId};

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
/// - An item OneDrive has in it that was moved out of it here: the removal waits for that
///   item, and likewise for one a step is running for.
/// - Every other item OneDrive has in it goes with the removal, and waits for the
///   directory: what was removed here with it, what OneDrive made in it or moved into it
///   since, and a file removed here whose content OneDrive changed, which does not come
///   back. The step names those of them that were removed here.
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
            let was_moved_out_here = row.local.is_some() && row.local.as_ref().map(|local| &local.place) != Some(&row.synced_place);
            if was_moved_out_here || matches!(decisions.find(below), Some(ItemDecision::Wait(WaitReason::RunningStep))) {
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
/// - An item in it that waits for the directory to be in OneDrive (made in it, or moved
///   into it here), and one that is left here, is the user's own and stays.
/// - While the folder shows anything else in it, the directory waits for that item: the
///   clean items go first, and an item whose content was changed here gives up its binding
///   first. A directory that gave up its binding before them would have them read as moved
///   here, and sent again.
/// - Once all the folder shows in it stays, the directory gives up its binding; the plan
///   made after it sends it as a new one, and then what is in it.
pub(crate) fn keep_directories_that_hold_the_users_own(cases: &[ItemCase<'_>], items: &PlanItems<'_>, decisions: &mut ItemDecisions) {
    for case in cases {
        let (item, directory) = (case.item, case.item.local_item_id);
        let is_not_in_onedrive = item.kind == ItemKind::Directory && item.local.is_some() && item.onedrive.is_none() && item.onedrive_item.is_some();
        let is_decided_here = matches!(decisions.find(directory), Some(ItemDecision::Wait(WaitReason::AnotherItem(_)) | ItemDecision::Step(SyncStep::Row(RowStep::GiveUpBinding { .. }))));
        if !is_not_in_onedrive || !is_decided_here {
            continue;
        }
        let stays = |shown: &LocalItemId| match decisions.find(*shown) {
            Some(ItemDecision::Wait(WaitReason::AnotherItem(waited_for))) => *waited_for == directory,
            Some(ItemDecision::Exclusion(ExclusionReason::NameIgnored)) => true,
            _ => false,
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
