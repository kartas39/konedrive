//! A name of a directory that an item of the folder is to have in OneDrive, and what else
//! has or wants that name there, as OneDrive compares names: what frees a name goes before
//! what takes it, two new items of one name are one, and otherwise the user's own item
//! steps aside under a conflict copy's name.

use std::collections::{BTreeMap, BTreeSet};

use konedrive_reason::Reason;

use super::conflict_copy_name::find_free_conflict_copy_name;
use super::item_decision::{ItemCase, ItemDecision, ItemDecisions};
use super::plan_items::PlanItems;
use super::side_change::{compare_local_content, ContentChange, PlaceChange, SideChange};
use super::sync_step::{FolderStep, ItemVersions, OneDriveStep, RowStep, SyncStep};
use super::{PlanFault, WaitReason};
use crate::folded_name::FoldedName;
use crate::model::{Item, ItemKind, ItemPlace, ItemVersion, LocalItemId};

/// The other item OneDrive has under the name an item is to have there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NameHolder<'a> {
    /// It leaves the name by a sending of its own, a move or a removal: which item.
    Leaves(LocalItemId),
    /// It stays under the name: its row.
    Stays(&'a Item),
}

/// An item whose place here is to be its place in OneDrive, and what stands against it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NameClaim<'a> {
    pub(crate) claimant: &'a Item,
    /// The claimant is left here, and so never sent.
    pub(crate) is_left_here: bool,
    pub(crate) holder: Option<NameHolder<'a>>,
    /// Another item of the folder is to have the name in OneDrive too, and came to be
    /// unsent earlier.
    pub(crate) has_earlier_rival: bool,
    /// OneDrive answered the sending of the claimant to this name that the name is taken.
    pub(crate) is_answered_taken: bool,
    pub(crate) is_listing_under_way: bool,
}

/// What becomes of an item that is to have a name in OneDrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NameContestOutcome {
    /// Nothing else has or wants the name.
    NameIsFree,
    /// It waits for the item that leaves the name.
    WaitsFor(LocalItemId),
    /// A listing is being written, and may yet say the holder is elsewhere.
    WaitsForListing,
    /// It and the holder, a new item of OneDrive, are one item.
    MergesWith(LocalItemId),
    /// Whether it and the holder are one file is known once its content is hashed.
    HashesLocalContent,
    /// It is renamed here to a conflict copy's name.
    StepsAside,
}

/// What becomes of the claimant of `claim`. The first that holds:
///
/// - another item of the folder is to have the name too, and was unsent earlier: the
///   claimant steps aside;
/// - nothing in OneDrive has the name, by the plan's rule for comparing names: it is free,
///   unless OneDrive answered the claimant's sending that the name is taken. OneDrive is
///   the judge: the claimant then steps aside, once no listing is under way that may yet
///   bring the item that has the name;
/// - the holder leaves the name: the claimant waits for it; one that is left here, and so
///   never sent, waits for nothing;
/// - the holder stays, and a listing is under way: the claimant waits for the listing;
/// - the holder stays, and both are new, each on its side, and of one kind: two
///   directories are one item, and so are two files of one content; where only hashing the
///   local content says whether they are, it is hashed first. An item that is left here is
///   never one with an item of OneDrive;
/// - otherwise the claimant steps aside.
pub(crate) fn decide_name_contest(claim: &NameClaim<'_>) -> NameContestOutcome {
    let claimant = claim.claimant;
    if claim.has_earlier_rival {
        return NameContestOutcome::StepsAside;
    }
    let holder = match claim.holder {
        None if claim.is_answered_taken && !claim.is_left_here => return if claim.is_listing_under_way { NameContestOutcome::WaitsForListing } else { NameContestOutcome::StepsAside },
        None => return NameContestOutcome::NameIsFree,
        Some(NameHolder::Leaves(_)) if claim.is_left_here => return NameContestOutcome::NameIsFree,
        Some(NameHolder::Leaves(holder)) => return NameContestOutcome::WaitsFor(holder),
        Some(NameHolder::Stays(_)) if claim.is_listing_under_way => return NameContestOutcome::WaitsForListing,
        Some(NameHolder::Stays(holder)) => holder,
    };
    let is_new_in_onedrive = holder.local.is_none() && holder.synced_place.is_none() && holder.synced_content.is_none();
    if claim.is_left_here || claimant.onedrive_item.is_some() || !is_new_in_onedrive || claimant.kind != holder.kind {
        return NameContestOutcome::StepsAside;
    }
    let content = |version: &'_ Option<ItemVersion>| version.as_ref().and_then(|version| version.content.clone());
    match (claimant.kind, compare_local_content(content(&claimant.local).as_ref(), content(&holder.onedrive).as_ref())) {
        (ItemKind::Directory, _) | (ItemKind::File, ContentChange::Same) => NameContestOutcome::MergesWith(holder.local_item_id),
        (ItemKind::File, ContentChange::NotKnownWithoutHash) => NameContestOutcome::HashesLocalContent,
        (ItemKind::File, ContentChange::Changed) => NameContestOutcome::StepsAside,
    }
}

/// Whether the place the folder shows `item` at is to be its place in OneDrive, by what the
/// plan answers for it: a new item of the folder, and an item whose move is sent.
fn claims_its_name(item: &Item, decision: Option<&ItemDecision>) -> bool {
    item.local.is_some() && (item.onedrive_item.is_none() || matches!(decision, Some(ItemDecision::Step(SyncStep::OneDrive(OneDriveStep::SendMove { .. })))))
}

/// Whether `holder`, which OneDrive has under the name `name` of `directory`, leaves that
/// name by a sending of its own, by what the plan answers for it: its move elsewhere or its
/// removal is a step, or waits. A file removed here that waits for another item comes back
/// instead, and stays.
fn leaves_its_name(holder: &Item, decision: Option<&ItemDecision>, directory: LocalItemId, name: &FoldedName) -> bool {
    let is_elsewhere_here = holder.local.as_ref().and_then(|local| local.place.as_ref()).is_some_and(|place| place.parent_local_item_id != directory || FoldedName::of(&place.name) != *name);
    match decision {
        Some(ItemDecision::Step(SyncStep::OneDrive(OneDriveStep::SendMove { .. }))) => is_elsewhere_here,
        Some(ItemDecision::Step(SyncStep::OneDrive(OneDriveStep::SendFileRemoval { .. } | OneDriveStep::SendDirectoryRemoval { .. }))) => true,
        Some(ItemDecision::Wait(reason)) => match SideChange::of_local(holder) {
            SideChange::Removed => !matches!(reason, WaitReason::AnotherItem(_)),
            SideChange::IsThere { place: PlaceChange::Moved, .. } => is_elsewhere_here,
            _ => false,
        },
        _ => false,
    }
}

/// Decides every item whose place here is to be its place in OneDrive against what else has
/// or wants that name there ([`decide_name_contest`]), the items by their ids. An item
/// that is left here is decided too: it steps aside for an item of OneDrive that stays at
/// its name. An item a step is running for, and a file open for writing, is not decided.
/// `places_taken_in_onedrive` is what OneDrive answered is taken
/// ([`SyncPlanInput`](super::SyncPlanInput)). `moves_taken_back` are the items whose move here does not stand
/// ([`move_cycle`](super::move_cycle)): such a holder stays at its name in OneDrive.
///
/// The steps towards OneDrive that now wait for a name another item leaves are returned,
/// by item: what waits in a circle goes through a temporary name
/// ([`name_exchange`](super::name_exchange)).
pub(crate) fn settle_name_contests(
    cases: &[ItemCase<'_>],
    items: &PlanItems<'_>,
    computer_name: &str,
    places_taken_in_onedrive: &BTreeMap<LocalItemId, ItemPlace>,
    moves_taken_back: &BTreeSet<LocalItemId>,
    decisions: &mut ItemDecisions,
) -> BTreeMap<LocalItemId, OneDriveStep> {
    let case_of: BTreeMap<LocalItemId, &ItemCase<'_>> = cases.iter().map(|case| (case.item.local_item_id, case)).collect();
    let is_left_here = |item: LocalItemId| case_of.get(&item).is_some_and(|case| case.sending.is_ignored);
    let mut sendings_that_wait = BTreeMap::new();
    let mut names_given: BTreeSet<(LocalItemId, FoldedName)> = BTreeSet::new();
    for (id, case) in &case_of {
        let (id, item) = (*id, case.item);
        let Some(place) = item.local.as_ref().and_then(|local| local.place.as_ref()) else { continue };
        let decision = decisions.find(id).cloned();
        let is_not_planned = matches!(decision, Some(ItemDecision::Wait(WaitReason::RunningStep | WaitReason::User(Reason::OpenForWriting))));
        if is_not_planned || !(is_left_here(id) || claims_its_name(item, decision.as_ref())) {
            continue;
        }
        let (directory, name) = (place.parent_local_item_id, FoldedName::of(&place.name));
        let in_onedrive = items.list_in_onedrive_with_name(directory, &name);
        let holder = in_onedrive.iter().filter(|holder| **holder != id).find_map(|holder| items.find(*holder)).map(|holder| {
            if !moves_taken_back.contains(&holder.local_item_id) && leaves_its_name(holder, decisions.find(holder.local_item_id), directory, &name) {
                NameHolder::Leaves(holder.local_item_id)
            } else {
                NameHolder::Stays(holder)
            }
        });
        let is_earlier_rival = |rival: &&Item| {
            let rival_id = rival.local_item_id;
            rival_id != id && !in_onedrive.contains(&rival_id) && !is_left_here(rival_id) && claims_its_name(rival, decisions.find(rival_id)) && (rival.unsent_since, rival_id) < (item.unsent_since, id)
        };
        let has_earlier_rival = !is_left_here(id) && items.list_shown_with_name(directory, &name).into_iter().filter_map(|rival| items.find(rival)).any(|rival| is_earlier_rival(&rival));
        let claim = NameClaim { claimant: item, is_left_here: is_left_here(id), holder, has_earlier_rival, is_answered_taken: places_taken_in_onedrive.get(&id) == Some(place), is_listing_under_way: case.is_listing_under_way };
        let sending = match &decision {
            Some(ItemDecision::Step(SyncStep::OneDrive(step @ (OneDriveStep::MakeDirectory { .. } | OneDriveStep::SendNewFile { .. } | OneDriveStep::SendMove { .. })))) => Some(step.clone()),
            _ => None,
        };
        match (decide_name_contest(&claim), sending) {
            (NameContestOutcome::NameIsFree, _) | (NameContestOutcome::WaitsFor(_) | NameContestOutcome::WaitsForListing, None) => {}
            (NameContestOutcome::WaitsFor(holder), Some(sending)) => {
                sendings_that_wait.insert(id, sending);
                decisions.replace(id, ItemDecision::Wait(WaitReason::AnotherItem(holder)));
            }
            (NameContestOutcome::WaitsForListing, Some(_)) => decisions.replace(id, ItemDecision::Wait(WaitReason::Listing)),
            (NameContestOutcome::MergesWith(holder), _) => {
                let Some(onedrive) = items.find(holder) else { continue };
                decisions.replace(id, ItemDecision::Step(SyncStep::Row(RowStep::MergeItems { local: ItemVersions::of(item), onedrive: ItemVersions::of(onedrive) })));
                decisions.replace(holder, ItemDecision::Wait(WaitReason::AnotherItem(id)));
            }
            (NameContestOutcome::HashesLocalContent, _) => {
                if !matches!(decision, Some(ItemDecision::Wait(WaitReason::Time { .. }))) {
                    decisions.replace(id, ItemDecision::Step(SyncStep::Row(RowStep::HashLocalContent { item: ItemVersions::of(item) })));
                }
            }
            (NameContestOutcome::StepsAside, _) => {
                let is_taken = |candidate: &FoldedName| {
                    !items.list_shown_with_name(directory, candidate).is_empty() || !items.list_in_onedrive_with_name(directory, candidate).is_empty() || names_given.contains(&(directory, candidate.clone()))
                };
                let decision = match find_free_conflict_copy_name(&place.name, computer_name, is_taken) {
                    Some(name) => {
                        names_given.insert((directory, FoldedName::of(&name)));
                        ItemDecision::Step(SyncStep::Folder(FolderStep::RenameToConflictCopy { item: ItemVersions::of(item), name }))
                    }
                    None => ItemDecision::Fault(PlanFault::NoFreeName(id)),
                };
                decisions.replace(id, decision);
            }
        }
    }
    sendings_that_wait
}

#[cfg(test)]
mod tests;
