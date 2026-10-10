//! The sync plan: the steps that make the local and the OneDrive versions of the items agree
//! again. One pure function ([`make_sync_plan`]): it is given rows and a few facts as values
//! ([`SyncPlanInput`]) and answers the steps that can run now and, for every other item it
//! was given, why not ([`SyncPlan`]). No disk, no network, no clock and no store in it.
//!
//! It is laid out by what each piece decides:
//!
//! - what each side did to one item, from its row ([`side_change`]);
//! - what that means for the item by itself: the three tables ([`item_decision`]);
//! - what is not sent, and why ([`sending_obstacle`]);
//! - one thing to do ([`sync_step`]);
//! - the rows it is made over, as one item asks about another ([`plan_items`]).
//!
//! Each item is decided by itself here: one step, one wait, or nothing. Where an item needs
//! another (its directory is not in OneDrive yet, another item has the name it comes to,
//! the folder still shows something in a directory that goes), it waits for that item.
//!
//! Nothing in the daemon calls it yet.

use std::collections::{BTreeMap, BTreeSet};

use konedrive_reason::Reason;

use crate::model::{FolderVisibility, Item, LocalItemId};

/// The name of a conflict copy.
mod conflict_copy_name;
/// The guard against a mass removal.
mod mass_removal;
/// A directory moved here into what OneDrive has in it.
mod move_cycle;
/// A name of a directory of OneDrive, and the items that have or want it.
mod name_contest;
/// The items that wait for each other's names, and the temporary name one goes through.
mod name_exchange;
/// A directory one side removed, and what the other side has in it.
mod removed_directory;
/// Where a wait for another item ends.
mod wait_end;
/// The rows a plan is made over, as one item asks about another.
mod plan_items;
/// The three tables: what the two sides' changes mean for one item by itself.
mod item_decision;
/// What is not sent to OneDrive, and why.
mod sending_obstacle;
/// What one side did to one item since the two sides last agreed.
mod side_change;
/// One thing the plan does, with the versions it was planned from.
mod sync_step;

pub use conflict_copy_name::make_conflict_copy_name;
pub use sync_step::{FolderStep, ItemVersions, OneDriveStep, RowStep, SyncStep};

use plan_items::PlanItems;
use item_decision::{decide_item, ItemCase, ItemDecision, ItemDecisions, LocalDirectoryBinding, LocalPlaceAvailability};
use sending_obstacle::SendingConditions;

/// The folder's state of sending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendingState {
    /// Everything unsent is sent.
    ReadWrite,
    /// The folder refuses every change, and what was unsent when it was switched is sent:
    /// an item whose [`unsent_since`](Item::unsent_since) is `last_unsent_since`, the last
    /// stamp given before the switch
    /// ([`ItemStore::last_unsent_since`](crate::ItemStore::last_unsent_since)), or an
    /// earlier one. What came to be unsent later waits for the folder to be read-write
    /// again.
    ReadOnly { last_unsent_since: u64 },
}

/// What a plan is made from, all as values.
///
/// **The rows.** `items` are the items that are unsent or whose OneDrive version is not the
/// synced one, and with each, so that the set is closed: the directory it is in on each
/// side, the item the folder shows at the place OneDrive has it at, and for a directory
/// that leaves the folder what the folder shows in it. An item that agrees on everything is
/// given for another's sake, or because it is a pinned file whose content is not on disk.
/// The root's row is among them once anything is in the root.
///
/// **What only the caller knows of an item** is said by the item's id: a set names the
/// items it holds for, a map the items it has a value for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPlanInput {
    pub items: Vec<Item>,
    /// Whether the folder can show an item as OneDrive has it
    /// ([`ItemStore::find_folder_visibility`](crate::ItemStore::find_folder_visibility)),
    /// for every item OneDrive has. An item OneDrive has that is not named here is not
    /// known yet.
    pub folder_visibility: BTreeMap<LocalItemId, FolderVisibility>,
    /// The items that are left here ([`ItemStore::is_ignored`](crate::ItemStore::is_ignored)).
    pub ignored_items: BTreeSet<LocalItemId>,
    /// The items whose name here OneDrive cannot hold, each with the reason.
    pub name_refusals: BTreeMap<LocalItemId, Reason>,
    /// The files a program holds open for writing.
    pub items_open_for_writing: BTreeSet<LocalItemId>,
    /// The files whose content was changed within the quiet time, each with the time, in
    /// Unix seconds, at which its quiet time ends.
    pub quiet_until: BTreeMap<LocalItemId, i64>,
    /// The items a step is running for now.
    pub items_with_running_step: BTreeSet<LocalItemId>,
    /// The files a pin holds for, their own or that of a directory above them.
    pub pinned_items: BTreeSet<LocalItemId>,
    /// A listing is being written into the rows
    /// ([`ItemStore::find_listing_under_way`](crate::ItemStore::find_listing_under_way)).
    pub is_listing_under_way: bool,
    pub is_onedrive_full: bool,
    /// The largest size of a file OneDrive takes, in bytes.
    pub largest_file_size: u64,
    pub sending_state: SendingState,
    /// This computer's name, as a conflict copy's name holds it
    /// ([`make_conflict_copy_name`]).
    pub computer_name: String,
    /// How many items the folder shows: what the guard against a mass removal takes a share
    /// of.
    pub shown_item_count: u64,
    /// The items whose removal from OneDrive the user confirmed: a removed directory, for
    /// all OneDrive has in it.
    pub items_with_confirmed_removal: BTreeSet<LocalItemId>,
}

/// Why an item's step cannot run now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitReason {
    /// It waits for another item that was given: which. That item's step goes first.
    AnotherItem(LocalItemId),
    /// It waits for an item whose row was not given: which. The caller gives that row too,
    /// and asks again.
    ItemNotGiven(LocalItemId),
    /// It waits until a time, in Unix seconds, and why.
    Time { until: i64, reason: Reason },
    /// It waits for the user, and why.
    User(Reason),
    /// A step of the item is running.
    RunningStep,
    /// It waits for a listing to end: what the rows say of the item in OneDrive cannot be
    /// judged yet.
    Listing,
}

/// An item whose step cannot run now, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemWait {
    pub item: LocalItemId,
    pub reason: WaitReason,
}

/// The rule by which the plan leaves an item out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusionReason {
    /// The item is left here: its name, or that of a directory above it, is ignored.
    NameIgnored,
    /// The folder cannot show the item as OneDrive has it.
    CannotBeShown,
}

/// An item the plan leaves out by a rule: no step is planned for it, and it waits for
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemExclusion {
    pub item: LocalItemId,
    pub reason: ExclusionReason,
}

/// What a plan found that must never be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanFault {
    /// The row of this item, or what the plan was given about it, says what no operation
    /// leaves.
    RowAgainstRules(LocalItemId),
    /// What the rows say of this item in OneDrive cannot be judged, and no listing is under
    /// way that would say it: a full listing mends the rows.
    FullListingNeeded(LocalItemId),
    /// This item waits for another item, and nothing the plan answers ends that wait: no
    /// step, no time, nothing of the user's, no listing.
    WaitWithoutEnd(LocalItemId),
    /// This item is to be renamed here, and none of the names tried for it is free.
    NoFreeName(LocalItemId),
}

impl PlanFault {
    /// The item the fault is about.
    pub fn local_item_id(&self) -> LocalItemId {
        match self {
            PlanFault::RowAgainstRules(item) | PlanFault::FullListingNeeded(item) | PlanFault::WaitWithoutEnd(item) | PlanFault::NoFreeName(item) => *item,
        }
    }
}

/// The answer: the steps that can run now, and for every other item given that the two
/// sides do not agree on, why not. An item that is in none of the four agrees, or its row
/// is no item.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncPlan {
    /// The steps that can run now, in the order the items were given; one step per item.
    pub steps: Vec<SyncStep>,
    pub waits: Vec<ItemWait>,
    pub exclusions: Vec<ItemExclusion>,
    pub faults: Vec<PlanFault>,
}

/// Whether `name` can be the name of a directory entry here: what a name of OneDrive is
/// asked before the folder takes it.
fn is_directory_entry_name(name: &std::ffi::OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    !bytes.is_empty() && bytes != b"." && bytes != b".." && !bytes.contains(&b'/') && !bytes.contains(&0) && bytes.len() <= konedrive_fs::NAME_MAX
}

/// How OneDrive knows the directory the folder shows `item` in. A binding counts only while
/// the rows say OneDrive has the directory: one it removed, and one no listing told of yet,
/// keep their binding and are nothing to send into.
fn find_local_directory_binding(items: &PlanItems<'_>, item: &Item) -> LocalDirectoryBinding {
    let Some(place) = item.local.as_ref().and_then(|local| local.place.as_ref()) else { return LocalDirectoryBinding::NoLocalPlace };
    match items.find(place.parent_local_item_id).filter(|directory| directory.onedrive.is_some()).and_then(|directory| directory.onedrive_item.clone()) {
        Some(directory) => LocalDirectoryBinding::Bound(directory),
        None => LocalDirectoryBinding::NotBound(place.parent_local_item_id),
    }
}

/// Whether the folder can take the place OneDrive has `item` at.
fn find_local_place_availability(items: &PlanItems<'_>, item: &Item) -> LocalPlaceAvailability {
    let Some(place) = item.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref()) else { return LocalPlaceAvailability::NoOneDrivePlace };
    if !is_directory_entry_name(&place.name) {
        return LocalPlaceAvailability::NameUnusable;
    }
    match items.find(place.parent_local_item_id) {
        Some(directory) if directory.local.is_some() => {}
        Some(directory) if directory.synced_place.is_some() => return LocalPlaceAvailability::DirectoryRemovedHere(place.parent_local_item_id),
        _ => return LocalPlaceAvailability::DirectoryNotShown(place.parent_local_item_id),
    }
    match items.find_shown_at(place) {
        Some(holder) if holder != item.local_item_id => LocalPlaceAvailability::NameTaken(holder),
        _ => LocalPlaceAvailability::Free,
    }
}

/// Whether a directory OneDrive has above `item`, at any depth, has a name that cannot be
/// the name of a directory entry here: the folder then cannot show `item` either.
fn is_in_directory_the_folder_cannot_name(items: &PlanItems<'_>, item: &Item) -> bool {
    let place_in_onedrive = |item: &Item| item.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone());
    let mut above = place_in_onedrive(item).and_then(|place| items.find(place.parent_local_item_id));
    for _ in 0..items.count() {
        let Some(place) = above.and_then(place_in_onedrive) else { return false };
        if !is_directory_entry_name(&place.name) {
            return true;
        }
        above = items.find(place.parent_local_item_id);
    }
    false
}

/// `item` as the plan decides on it: its row, with what `input` says of it and what the
/// other rows of the plan say about it.
fn describe_item<'a>(input: &SyncPlanInput, items: &PlanItems<'_>, item: &'a Item) -> ItemCase<'a> {
    let id = item.local_item_id;
    let visibility_of_listed_item = || match input.folder_visibility.get(&id).copied().unwrap_or(FolderVisibility::NotKnownYet) {
        FolderVisibility::CanBeShown if is_in_directory_the_folder_cannot_name(items, item) => FolderVisibility::CannotBeShown,
        visibility => visibility,
    };
    ItemCase {
        item,
        folder_visibility: if item.onedrive.is_some() { visibility_of_listed_item() } else { FolderVisibility::NotInOneDrive },
        is_listing_under_way: input.is_listing_under_way,
        has_running_step: input.items_with_running_step.contains(&id),
        is_open_for_writing: input.items_open_for_writing.contains(&id),
        is_pinned: input.pinned_items.contains(&id),
        local_directory: find_local_directory_binding(items, item),
        local_place_availability: find_local_place_availability(items, item),
        first_shown_in_it: items.find_first_shown_in(id),
        has_items_in_onedrive: !items.list_in_onedrive_in(id).is_empty(),
        sending: SendingConditions {
            is_ignored: input.ignored_items.contains(&id),
            name_refusal: input.name_refusals.get(&id).cloned(),
            size: item.local.as_ref().and_then(|local| local.content.as_ref()).map(|content| content.size()),
            largest_file_size: input.largest_file_size,
            quiet_until: input.quiet_until.get(&id).copied(),
            is_onedrive_full: input.is_onedrive_full,
            sending_state: input.sending_state,
            unsent_since: item.unsent_since,
        },
    }
}

impl SyncPlan {
    /// The answer made of what was decided for each of `items`: its step, its wait, the
    /// rule that leaves it out, or its fault; nothing for an item nothing is done for. The
    /// steps of what is unsent come first, the item that was unsent first before the
    /// others; the steps for what OneDrive changed follow, in the order the items were
    /// given. Everything else is in the order given.
    fn of_decisions(items: &[Item], decisions: ItemDecisions) -> Self {
        let unsent_since: BTreeMap<LocalItemId, u64> = items.iter().filter_map(|item| Some((item.local_item_id, item.unsent_since?))).collect();
        let mut plan = Self::default();
        for (item, decision) in decisions.into_ordered() {
            match decision {
                ItemDecision::NothingToDo => {}
                ItemDecision::Step(step) => plan.steps.push(step),
                ItemDecision::Wait(reason) => plan.waits.push(ItemWait { item, reason }),
                ItemDecision::Exclusion(reason) => plan.exclusions.push(ItemExclusion { item, reason }),
                ItemDecision::Fault(fault) => plan.faults.push(fault),
            }
        }
        plan.steps.sort_by_key(|step| unsent_since.get(&step.local_item_id()).map_or((true, 0), |since| (false, *since)));
        plan
    }
}

/// The plan for what `input` gives. Each item is decided by itself first, from its row
/// ([`item_decision`]); then the items are decided for each other, each kind of case by its
/// own module, in this order:
///
/// 1. a directory moved here into what OneDrive has in it ([`move_cycle`]);
/// 2. the items that have or want one name of a directory of OneDrive ([`name_contest`]);
/// 3. a directory one side removed, and what the other has in it ([`removed_directory`]);
/// 4. the items that wait for each other's names ([`name_exchange`]);
/// 5. the guard against a mass removal ([`mass_removal`]);
/// 6. where each wait for another item ends ([`wait_end`]).
pub fn make_sync_plan(input: &SyncPlanInput) -> SyncPlan {
    let items = PlanItems::of(&input.items);
    let cases: Vec<ItemCase<'_>> = input.items.iter().map(|item| describe_item(input, &items, item)).collect();
    let mut decisions = ItemDecisions::of(cases.iter().map(|case| (case.item.local_item_id, decide_item(case))));
    move_cycle::take_back_moves_into_own_content(&cases, &items, &mut decisions);
    let sendings_that_wait = name_contest::settle_name_contests(&cases, &items, &input.computer_name, &mut decisions);
    removed_directory::send_one_removal_for_each_directory(&cases, &items, &mut decisions);
    removed_directory::keep_directories_that_hold_the_users_own(&cases, &items, &mut decisions);
    name_exchange::break_circles_of_names(&cases, &items, &sendings_that_wait, &mut decisions);
    mass_removal::hold_mass_removal(&cases, &items, input.shown_item_count, &input.items_with_confirmed_removal, &mut decisions);
    wait_end::end_waits_for_other_items(input.is_listing_under_way, &items, &mut decisions);
    SyncPlan::of_decisions(&input.items, decisions)
}

#[cfg(test)]
mod tests;
