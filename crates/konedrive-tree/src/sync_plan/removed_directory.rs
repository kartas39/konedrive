//! A directory one side removed, and what the other side has in it.
//!
//! Removed here: its removal is one removal in OneDrive for all OneDrive has in it, and
//! stands whatever OneDrive changed there. It is the removal of the outermost of the
//! directories removed in one another, and it is not sent while anything it would take
//! with it has a step running or is shown by the folder ([`decide_directory_removal`]).
//! Whatever is answered for that directory, nothing OneDrive has below it, and the folder
//! does not show, has a removal or any other step of its own: it waits for the directory.
//!
//! Removed in OneDrive: it goes here once the folder shows nothing in it; where what it
//! holds is the user's own, it stays, gives up its binding and is sent again. A directory
//! that has a binding and that no listing told of gives its binding up in the same order.

use std::collections::HashMap;

use super::item_decision::{ItemAnswers, ItemCase, ItemDecision, LocalPlaceAvailability, Overruling};
use super::plan_items::PlanItems;
use super::sync_step::{FolderStep, ItemVersions, OneDriveStep, RowStep, SyncStep};
use super::WaitReason;
use crate::model::{Item, ItemKind, ItemPlace, LocalItemId};

/// Whether `item` is a directory that was removed here and that OneDrive has.
pub(crate) fn is_directory_removed_here(item: &Item) -> bool {
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

/// One item the rows say OneDrive has below a directory that was removed here, at any
/// depth: what the removal of that directory would take from OneDrive with it. The first
/// that holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ItemBelowRemovedDirectory {
    /// The folder shows it: it was moved out of the directory here, or OneDrive moved it
    /// into the directory, where the folder cannot follow. Its place here is unsent.
    ShownHere(LocalItemId),
    /// A step is running for it.
    HasRunningStep(LocalItemId),
    /// It was removed here with the directory: its versions.
    RemovedHere(ItemVersions),
    /// OneDrive made it there, or moved it there, and the folder never showed it.
    NeverShownHere(LocalItemId),
}

/// What becomes of the removal of a directory that was removed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DirectoryRemovalOutcome {
    /// It is sent, one removal for all OneDrive has below the directory: what was removed
    /// here with it.
    IsSent { removed_with_it: Vec<ItemVersions> },
    /// It waits for this item, which it would take with it.
    WaitsFor(LocalItemId),
}

/// What becomes of the removal of a directory that was removed here, `below` being all the
/// rows say OneDrive has below it. It waits for the first item the folder shows or a step
/// is running for: such an item is not removed with the directory, its place here is sent
/// first, or its step ends first. Otherwise it is sent, and stands for what OneDrive made
/// there since.
pub(crate) fn decide_directory_removal(below: &[ItemBelowRemovedDirectory]) -> DirectoryRemovalOutcome {
    let mut removed_with_it = Vec::new();
    for item in below {
        match item {
            ItemBelowRemovedDirectory::ShownHere(kept) | ItemBelowRemovedDirectory::HasRunningStep(kept) => return DirectoryRemovalOutcome::WaitsFor(*kept),
            ItemBelowRemovedDirectory::RemovedHere(versions) => removed_with_it.push(versions.clone()),
            ItemBelowRemovedDirectory::NeverShownHere(_) => {}
        }
    }
    DirectoryRemovalOutcome::IsSent { removed_with_it }
}

/// What the rows and the caller say of each item OneDrive has below `directory`, each
/// directory before what is in it.
fn list_below_removed_directory(case_of: &HashMap<LocalItemId, &ItemCase<'_>>, items: &PlanItems<'_>, directory: LocalItemId) -> Vec<ItemBelowRemovedDirectory> {
    let describe = |below: LocalItemId| {
        let row = items.find(below)?;
        Some(if row.local.is_some() {
            ItemBelowRemovedDirectory::ShownHere(below)
        } else if case_of.get(&below).is_some_and(|case| case.has_running_step) {
            ItemBelowRemovedDirectory::HasRunningStep(below)
        } else if row.synced_place.is_some() || row.synced_content.is_some() {
            ItemBelowRemovedDirectory::RemovedHere(ItemVersions::of(row))
        } else {
            ItemBelowRemovedDirectory::NeverShownHere(below)
        })
    };
    items.list_below_in_onedrive(directory).into_iter().filter_map(describe).collect()
}

/// Decides the removal of each directory that was removed here, the outermost of those
/// removed in one another, from what the rows and the caller say of all OneDrive has below
/// it ([`decide_directory_removal`]) — never from what was answered for another item.
///
/// - Everything OneDrive has below it that the folder does not show waits for the
///   directory, whatever is answered for the directory itself: what was removed here with
///   it, the directories among that too, what OneDrive made in it since, and a file
///   removed here whose content OneDrive changed, which does not come back. So none of
///   them has a removal of its own while a directory above it is still to be removed.
/// - Where the removal of the directory is what its row answers, it is sent or waits; any
///   other answer of the directory stands — it is held back, a rule keeps its removal from
///   being sent now, or the plan was not given all that is in it.
pub(crate) fn decide_removals_of_directories(cases: &[ItemCase<'_>], items: &PlanItems<'_>, answers: &ItemAnswers) -> Vec<Overruling> {
    let case_of: HashMap<LocalItemId, &ItemCase<'_>> = cases.iter().map(|case| (case.item.local_item_id, case)).collect();
    let mut overrulings = Vec::new();
    for case in cases {
        let directory = case.item.local_item_id;
        if !is_directory_removed_here(case.item) || is_in_directory_removed_here(items, case.item) {
            continue;
        }
        let below = list_below_removed_directory(&case_of, items, directory);
        for item in &below {
            match item {
                ItemBelowRemovedDirectory::ShownHere(_) => {}
                ItemBelowRemovedDirectory::HasRunningStep(goes_with_it) | ItemBelowRemovedDirectory::NeverShownHere(goes_with_it) => overrulings.push(Overruling::of(*goes_with_it, ItemDecision::Wait(WaitReason::AnotherItem(directory)))),
                ItemBelowRemovedDirectory::RemovedHere(goes_with_it) => overrulings.push(Overruling::of(goes_with_it.local_item_id, ItemDecision::Wait(WaitReason::AnotherItem(directory)))),
            }
        }
        match answers.find(directory) {
            Some(ItemDecision::Step(SyncStep::OneDrive(OneDriveStep::SendDirectoryRemoval { item, .. }))) => overrulings.push(Overruling::of(
                directory,
                match decide_directory_removal(&below) {
                    DirectoryRemovalOutcome::IsSent { removed_with_it } => ItemDecision::Step(SyncStep::OneDrive(OneDriveStep::SendDirectoryRemoval { item: item.clone(), below: removed_with_it })),
                    DirectoryRemovalOutcome::WaitsFor(kept) => ItemDecision::Wait(WaitReason::AnotherItem(kept)),
                },
            )),
            // The row of a directory that was removed here answers its removal or a wait.
            Some(ItemDecision::Step(_) | ItemDecision::Wait(_) | ItemDecision::NothingToDo | ItemDecision::Exclusion(_) | ItemDecision::Fault(_)) | None => {}
        }
    }
    overrulings
}

/// Whether the answer for a directory the folder shows and OneDrive does not have is one
/// this module decides again from what the folder shows in it: it goes here, waiting for
/// what is in it, or gives up its binding. Every other answer stands.
fn is_decided_by_what_it_holds(answer: Option<&ItemDecision>) -> bool {
    match answer {
        Some(ItemDecision::Wait(WaitReason::AnotherItem(_)) | ItemDecision::Step(SyncStep::Row(RowStep::GiveUpBinding { .. }))) => true,
        // Nothing is shown in it, and it goes.
        Some(ItemDecision::Step(SyncStep::Folder(FolderStep::RemoveItem { .. }))) => false,
        // Held back, not judged yet, or not given with all that is in it.
        Some(ItemDecision::Wait(WaitReason::RunningStep | WaitReason::User(_) | WaitReason::Time { .. } | WaitReason::Listing | WaitReason::ItemNotGiven(_) | WaitReason::ItemsInDirectoryNotGiven(_))) => false,
        Some(ItemDecision::Step(_) | ItemDecision::NothingToDo | ItemDecision::Exclusion(_) | ItemDecision::Fault(_)) | None => false,
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
pub(crate) fn keep_directories_that_hold_the_users_own(cases: &[ItemCase<'_>], items: &PlanItems<'_>, answers: &ItemAnswers) -> Vec<Overruling> {
    let case_of: HashMap<LocalItemId, &ItemCase<'_>> = cases.iter().map(|case| (case.item.local_item_id, case)).collect();
    let mut overrulings = Vec::new();
    for case in cases {
        let (item, directory) = (case.item, case.item.local_item_id);
        let is_not_in_onedrive = item.kind == ItemKind::Directory && item.local.is_some() && item.onedrive.is_none() && item.onedrive_item.is_some();
        if !is_not_in_onedrive || !is_decided_by_what_it_holds(answers.find(directory)) {
            continue;
        }
        // Nothing of an item goes with the binding when neither side had it in the directory
        // when they last agreed and OneDrive does not have it there now. An item that was
        // agreed on there stays only when it is sent into the directory again.
        let is_in_it = |place: Option<&ItemPlace>| place.is_some_and(|place| place.parent_local_item_id == directory);
        let waits_to_be_sent_into_it = |shown: LocalItemId| {
            matches!(answers.find(shown), Some(ItemDecision::Wait(WaitReason::AnotherItem(waited_for))) if *waited_for == directory)
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
        overrulings.push(Overruling::of(directory, decision));
    }
    overrulings
}

#[cfg(test)]
mod tests;
