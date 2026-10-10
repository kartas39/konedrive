//! A cycle of moves: a directory moved here into what OneDrive has, or is about to have, in
//! it. Sending the move would put the directory into itself, so OneDrive's move stands and
//! the local one is taken back; and a directory OneDrive moved into what the folder shows
//! in it waits until that is out of it.

use std::collections::BTreeSet;

use super::item_decision::{ItemAnswers, ItemCase, ItemDecision, LocalPlaceAvailability, OverruledAnswers, Overruling};
use super::plan_items::PlanItems;
use super::side_change::{PlaceChange, SideChange};
use super::sync_step::{FolderStep, ItemVersions, OneDriveStep, SyncStep};
use super::WaitReason;
use crate::model::{Item, ItemKind, LocalItemId};

fn directory_here(item: &Item) -> Option<LocalItemId> {
    item.local.as_ref().and_then(|local| local.place.as_ref()).map(|place| place.parent_local_item_id)
}

fn directory_in_onedrive(item: &Item) -> Option<LocalItemId> {
    item.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref()).map(|place| place.parent_local_item_id)
}

/// Whether `item` was moved here and OneDrive did not move it: its place here is what is to
/// be sent.
fn is_moved_here_alone(item: &Item) -> bool {
    item.onedrive.is_some() && matches!(SideChange::of_local(item), SideChange::IsThere { place: PlaceChange::Moved, .. }) && matches!(SideChange::of_onedrive(item), SideChange::IsThere { place: PlaceChange::Same, .. })
}

/// The directory `item` is in once the two sides agree, as the plan decides each item by
/// itself: the directory the folder shows it in when it is new here or was moved here
/// alone, and the one OneDrive has it in otherwise.
fn find_directory_to_be(item: &Item) -> Option<LocalItemId> {
    if item.onedrive.is_none() || is_moved_here_alone(item) {
        directory_here(item)
    } else {
        directory_in_onedrive(item)
    }
}

/// Whether sending the move of `item`, a directory moved here, would put it into what is to
/// be in it: the way up from the directory it was moved into, each directory where it is to
/// be, passes `item`.
fn is_moved_into_own_content(items: &PlanItems<'_>, item: &Item) -> bool {
    let mut above = directory_here(item);
    for _ in 0..items.count() {
        match above {
            Some(directory) if directory == item.local_item_id => return true,
            Some(directory) => above = items.find(directory).and_then(find_directory_to_be),
            None => return false,
        }
    }
    false
}

/// The first directory on the way up from `directory`, each directory in the one `directory_of`
/// gives for it, that is not at one place on the two sides; `directory` itself when all are.
fn find_directory_out_of_place(items: &PlanItems<'_>, directory: LocalItemId, directory_of: fn(&Item) -> Option<LocalItemId>) -> LocalItemId {
    let mut at = Some(directory);
    for _ in 0..items.count() {
        let Some(row) = at.and_then(|at| items.find(at)) else { break };
        if row.local.as_ref().map(|local| &local.place) != row.onedrive.as_ref().map(|onedrive| &onedrive.place) {
            return row.local_item_id;
        }
        at = directory_of(row);
    }
    directory
}

/// A move made here is taken back where the folder can take OneDrive's place again; where
/// it cannot yet, the item waits for what is in the way. `None` when no step takes it back.
fn take_move_back(case: &ItemCase<'_>) -> Option<ItemDecision> {
    match case.local_place_availability {
        LocalPlaceAvailability::Free => Some(ItemDecision::Step(SyncStep::Folder(FolderStep::TakeMoveBack { item: ItemVersions::of(case.item) }))),
        LocalPlaceAvailability::DirectoryNotShown(another) | LocalPlaceAvailability::DirectoryRemovedHere(another) | LocalPlaceAvailability::NameTaken(another) => Some(ItemDecision::Wait(WaitReason::AnotherItem(another))),
        LocalPlaceAvailability::NameUnusable | LocalPlaceAvailability::NoOneDrivePlace => None,
    }
}

/// Whether the place here of a directory that was moved here alone is to be its place in
/// OneDrive, by what is answered for it: its move is a step, or waits for what may let it
/// be sent later. Not so while a step is running for it — nothing of it is planned, and
/// its move is not taken back under that step — nor while its row cannot be judged.
fn is_move_to_be_sent(answer: Option<&ItemDecision>) -> bool {
    match answer {
        Some(ItemDecision::Step(SyncStep::OneDrive(OneDriveStep::SendMove { .. }))) => true,
        Some(ItemDecision::Wait(WaitReason::User(_) | WaitReason::Time { .. } | WaitReason::AnotherItem(_))) => true,
        Some(ItemDecision::Wait(WaitReason::RunningStep | WaitReason::StartUpCheck | WaitReason::Listing | WaitReason::ItemNotGiven(_) | WaitReason::ItemsInDirectoryNotGiven(_))) => false,
        Some(ItemDecision::Step(_) | ItemDecision::NothingToDo | ItemDecision::Exclusion(_) | ItemDecision::Fault(_)) | None => false,
    }
}

/// What was decided of the directories that were moved in a cycle.
pub(crate) struct MoveCycles {
    pub(crate) overrulings: Vec<Overruling>,
    /// The directories whose move here does not stand: the one whose move is taken back
    /// and those that wait for it.
    pub(crate) moves_taken_back: BTreeSet<LocalItemId>,
}

/// Decides the directories that were moved in a cycle.
///
/// - Of the directories moved here whose move would put them into what is to be in them,
///   the move of the one with the lowest id is taken back, and the others wait for it: the
///   plan made after it sees whether their moves still close a cycle. This holds also for a
///   move that is not sent now (a read-only folder, a name OneDrive cannot hold).
/// - A directory OneDrive moved into what the folder shows in it is not moved here yet: it
///   waits for the first directory on the way that is out of place. Likewise a directory
///   moved here into what OneDrive still has in it, whose move closes no cycle, is not
///   sent yet: a directory before what is in it.
pub(crate) fn take_back_moves_into_own_content(cases: &[ItemCase<'_>], items: &PlanItems<'_>, answers: &ItemAnswers) -> MoveCycles {
    let mut answers = OverruledAnswers::of(answers);
    let mut in_cycle: Vec<&ItemCase<'_>> = cases
        .iter()
        .filter(|case| case.item.kind == ItemKind::Directory && is_moved_here_alone(case.item))
        .filter(|case| is_move_to_be_sent(answers.find(case.item.local_item_id)) && is_moved_into_own_content(items, case.item))
        .collect();
    in_cycle.sort_by_key(|case| case.item.local_item_id);
    let mut moves_taken_back = BTreeSet::new();
    if let Some((first, others)) = in_cycle.split_first() {
        if let Some(taken_back) = take_move_back(first) {
            answers.overrule(first.item.local_item_id, taken_back);
            moves_taken_back.insert(first.item.local_item_id);
            for other in others {
                answers.overrule(other.item.local_item_id, ItemDecision::Wait(WaitReason::AnotherItem(first.item.local_item_id)));
                moves_taken_back.insert(other.item.local_item_id);
            }
        }
    }
    for case in cases {
        let id = case.item.local_item_id;
        if case.item.kind != ItemKind::Directory {
            continue;
        }
        match answers.find(id) {
            Some(ItemDecision::Step(SyncStep::Folder(FolderStep::MoveItem { .. } | FolderStep::TakeMoveBack { .. }))) => {
                let Some(directory) = directory_in_onedrive(case.item) else { continue };
                if directory == id || items.is_shown_in(directory, id) {
                    answers.overrule(id, ItemDecision::Wait(WaitReason::AnotherItem(find_directory_out_of_place(items, directory, directory_here))));
                }
            }
            Some(ItemDecision::Step(SyncStep::OneDrive(OneDriveStep::SendMove { .. }))) => {
                let Some(directory) = directory_here(case.item) else { continue };
                if directory == id || items.is_in_onedrive_in(directory, id) {
                    answers.overrule(id, ItemDecision::Wait(WaitReason::AnotherItem(find_directory_out_of_place(items, directory, directory_in_onedrive))));
                }
            }
            // No other step moves a directory, and what waits moves nothing now.
            Some(ItemDecision::Step(_) | ItemDecision::Wait(_) | ItemDecision::NothingToDo | ItemDecision::Exclusion(_) | ItemDecision::Fault(_)) | None => {}
        }
    }
    MoveCycles { overrulings: answers.into_overrulings(), moves_taken_back }
}
