//! A directory one side removed, and what the other side has in it.
//!
//! Removed here: its removal is sent on a positive condition — all the rows say OneDrive
//! has below it, at any depth, was shown here and removed here by the user, and none of it
//! is a file whose content OneDrive changed since ([`decide_directory_removal`]). It is
//! then one removal in OneDrive for all of that, and stands where OneDrive renamed or
//! moved those items since. Where OneDrive has anything below it that the folder never
//! showed — it cannot be shown, it was not shown yet, another client put it there — or a
//! content the folder never showed — that of a file that was removed here and that
//! OneDrive changed, which by itself comes back —, the removal is of the folder alone:
//! what was agreed on of the directory is forgotten, it is an item OneDrive has that the
//! folder does not show, and each item the user did remove below it is decided by itself.
//! A removal thus takes with a directory only what its own removal would take. The same
//! function is asked by the plan, by the step before its request leaves and by the step's
//! commit
//! ([`ItemStore::can_directory_removal_be_sent`](crate::ItemStore::can_directory_removal_be_sent)).
//!
//! While the removal of a directory is still to be sent, nothing OneDrive has below it,
//! and the folder does not show, has a removal or any other step of its own: it waits for
//! the directory, the outermost of those removed in one another.
//!
//! Removed in OneDrive: it goes here once the folder shows nothing in it; where what it
//! holds is the user's own, it stays, gives up its binding and is sent again. A directory
//! that has a binding and that no listing told of gives its binding up in the same order.

use std::collections::{HashMap, HashSet};

use super::item_decision::{find_hold, ItemAnswers, ItemCase, ItemDecision, LocalPlaceAvailability, Overruling};
use super::side_change::{ContentChange, SideChange};
use super::plan_items::PlanItems;
use super::sync_step::{FolderStep, ItemVersions, RowStep, SyncStep};
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
/// that holds, in the order of [`of`](Self::of): shown, held back, a content changed in
/// OneDrive, removed here, never shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ItemBelowRemovedDirectory {
    /// The folder shows it: it was moved out of the directory here, or OneDrive moved it
    /// into the directory, where the folder cannot follow. Its place here is unsent.
    ShownHere(LocalItemId),
    /// It is held back: a request of this client about it, or one that sends an item into
    /// it, is not settled, or the check at start could not decide it.
    IsHeldBack(LocalItemId),
    /// A file the folder showed, which the user removed here, and whose content OneDrive
    /// changed since: by itself it comes back
    /// ([`decide_existence`](super::item_decision::decide_existence)), and the content
    /// OneDrive alone has is not removed with a directory either.
    ContentChangedInOneDrive(LocalItemId),
    /// The folder showed it, and the user removed it here: its versions. Something is
    /// agreed on of it still, and its own removal would be sent — OneDrive did nothing to
    /// it since, or moved it, into the directory too.
    RemovedHere(ItemVersions),
    /// The folder never showed it, as the rows say: nothing is agreed on of it. OneDrive
    /// made it there or moved it there since, the folder cannot show it, or the plan took
    /// it off the folder because it could not.
    NeverShownHere(LocalItemId),
}

impl ItemBelowRemovedDirectory {
    /// What `row` is, a row OneDrive has below a directory that was removed here.
    /// `is_held_back`: what the rows do not say — whether something holds the item back.
    pub(crate) fn of(row: &Item, is_held_back: bool) -> Self {
        if row.local.is_some() {
            ItemBelowRemovedDirectory::ShownHere(row.local_item_id)
        } else if is_held_back {
            ItemBelowRemovedDirectory::IsHeldBack(row.local_item_id)
        } else if row.kind == ItemKind::File && matches!((SideChange::of_local(row), SideChange::of_onedrive(row)), (SideChange::Removed, SideChange::IsThere { content: ContentChange::Changed | ContentChange::NotKnownWithoutHash, .. })) {
            ItemBelowRemovedDirectory::ContentChangedInOneDrive(row.local_item_id)
        } else if row.synced_place.is_some() || row.synced_content.is_some() {
            ItemBelowRemovedDirectory::RemovedHere(ItemVersions::of(row))
        } else {
            // TODO: contract — an item an upload of this folder left below the directory (`SyncPlanInput::items_left_by_uploads`) reads as never shown here, and makes the removal one of the folder alone: the directory then comes back. This function is asked by the store too (`ItemStore::can_directory_removal_be_sent`, item_store), which is not told what an upload left.
            ItemBelowRemovedDirectory::NeverShownHere(row.local_item_id)
        }
    }
}

/// What becomes of the removal of a directory that was removed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DirectoryRemovalOutcome {
    /// It is sent, one removal for all OneDrive has below the directory: what was removed
    /// here with it.
    IsSent { removed_with_it: Vec<ItemVersions> },
    /// It is of the folder alone, and nothing is sent: OneDrive has below the directory
    /// what the folder never showed — an item, or the content of a file that was removed
    /// here.
    IsOfFolderAlone,
    /// It waits for this item, which it would take with it.
    WaitsFor(LocalItemId),
    /// It waits for the listing under way to end: the rows may be behind OneDrive.
    WaitsForListing,
}

/// What becomes of the removal of a directory that was removed here, `below` being all the
/// rows say OneDrive has below it. The first that holds:
///
/// - a listing is under way: nothing is decided, as a later page may say an item is
///   elsewhere, or is another one;
/// - the folder shows an item of `below`, or one is held back: the removal waits for the
///   first such item — its place here is sent first, or what holds it ends first;
/// - the folder never showed an item of `below`, or OneDrive changed the content of a
///   file of `below` that was removed here: the removal is of the folder alone. An
///   unguarded removal takes all that is in the directory, and what the user never saw
///   here — an item, or the content a file has now — is not theirs to have removed;
/// - otherwise all of `below` was shown here and removed here, as OneDrive still has it
///   but for its place: the removal is sent, and stands where OneDrive renamed or moved
///   those items since.
pub(crate) fn decide_directory_removal(is_listing_under_way: bool, below: &[ItemBelowRemovedDirectory]) -> DirectoryRemovalOutcome {
    if is_listing_under_way {
        return DirectoryRemovalOutcome::WaitsForListing;
    }
    let mut removed_with_it = Vec::new();
    let mut is_anything_never_shown = false;
    for item in below {
        match item {
            ItemBelowRemovedDirectory::ShownHere(kept) | ItemBelowRemovedDirectory::IsHeldBack(kept) => return DirectoryRemovalOutcome::WaitsFor(*kept),
            ItemBelowRemovedDirectory::RemovedHere(versions) => removed_with_it.push(versions.clone()),
            ItemBelowRemovedDirectory::ContentChangedInOneDrive(_) | ItemBelowRemovedDirectory::NeverShownHere(_) => is_anything_never_shown = true,
        }
    }
    if is_anything_never_shown {
        DirectoryRemovalOutcome::IsOfFolderAlone
    } else {
        DirectoryRemovalOutcome::IsSent { removed_with_it }
    }
}

/// What the rows and the caller say of each item OneDrive has below each directory of
/// `cases` that was removed here, each directory before what is in it: what its removal is
/// decided from, by the directory's id.
pub(crate) fn list_below_removed_directories(cases: &[ItemCase<'_>], items: &PlanItems<'_>) -> HashMap<LocalItemId, Vec<ItemBelowRemovedDirectory>> {
    let held: HashSet<LocalItemId> = cases.iter().filter(|case| find_hold(case).is_some()).map(|case| case.item.local_item_id).collect();
    let describe = |below: LocalItemId| Some(ItemBelowRemovedDirectory::of(items.find(below)?, held.contains(&below)));
    cases
        .iter()
        .filter(|case| is_directory_removed_here(case.item))
        .map(|case| (case.item.local_item_id, items.list_below_in_onedrive(case.item.local_item_id).into_iter().filter_map(describe).collect()))
        .collect()
}

/// How many directories OneDrive has above `item`, as far as the rows given say.
fn count_directories_above_in_onedrive(items: &PlanItems<'_>, item: &Item) -> usize {
    let directory_of = |item: &Item| item.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref()).and_then(|place| items.find(place.parent_local_item_id));
    let (mut above, mut count) = (directory_of(item), 0);
    while let Some(directory) = above.filter(|_| count < items.count()) {
        count += 1;
        above = directory_of(directory);
    }
    count
}

/// Lets what OneDrive has below a directory that was removed here wait for that directory
/// while its removal is still to be sent: the outermost of the directories removed in one
/// another first. Each directory's removal is decided for it by itself, from what the rows
/// and the caller say of all OneDrive has below it
/// ([`decide_item`](super::item_decision::decide_item)); here only what follows from it for
/// the items below.
///
/// - The directory's removal is of the folder alone
///   ([`ItemCase::is_removal_of_folder_alone`]): nothing below waits for it by this rule.
///   Each item below is decided by itself, a directory removed here among them by what is
///   below it in its turn.
/// - Otherwise — the removal is sent, it waits, the directory is held back, a rule keeps
///   it from being sent now, or the plan was not given all that is in it — everything
///   OneDrive has below it that the folder does not show waits for the directory: what was
///   removed here with it, the directories among that too, and a file removed here whose
///   content OneDrive changed, which comes back once the directory is decided. So none of
///   them has a step of its own while a directory above it is still to be removed.
pub(crate) fn wait_for_removals_of_directories(cases: &[ItemCase<'_>], items: &PlanItems<'_>) -> Vec<Overruling> {
    let mut removed: Vec<&ItemCase<'_>> = cases.iter().filter(|case| is_directory_removed_here(case.item)).collect();
    removed.sort_by_key(|case| (count_directories_above_in_onedrive(items, case.item), case.item.local_item_id));
    let mut waiting: HashSet<LocalItemId> = HashSet::new();
    let mut overrulings = Vec::new();
    for case in removed {
        let directory = case.item.local_item_id;
        if waiting.contains(&directory) || case.is_removal_of_folder_alone() {
            continue;
        }
        for item in &case.below_in_onedrive {
            let goes_with_it = match item {
                ItemBelowRemovedDirectory::ShownHere(_) => continue,
                ItemBelowRemovedDirectory::IsHeldBack(item) | ItemBelowRemovedDirectory::NeverShownHere(item) | ItemBelowRemovedDirectory::ContentChangedInOneDrive(item) => *item,
                ItemBelowRemovedDirectory::RemovedHere(item) => item.local_item_id,
            };
            if waiting.insert(goes_with_it) {
                overrulings.push(Overruling::of(goes_with_it, ItemDecision::Wait(WaitReason::AnotherItem(directory))));
            }
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
        Some(ItemDecision::Wait(WaitReason::RunningStep | WaitReason::StartUpCheck | WaitReason::User(_) | WaitReason::Time { .. } | WaitReason::Listing | WaitReason::ItemNotGiven(_) | WaitReason::ItemsInDirectoryNotGiven(_))) => false,
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
