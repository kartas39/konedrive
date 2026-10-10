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

pub use sync_step::{FolderStep, ItemVersions, OneDriveStep, RowStep, SyncStep};

use plan_items::PlanItems;
use item_decision::{decide_item, ItemCase, ItemDecision, LocalDirectoryBinding, LocalPlaceAvailability};
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
}

/// Why an item's step cannot run now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitReason {
    /// It waits for another item: which. That item's step goes first, or its row was not
    /// given.
    AnotherItem(LocalItemId),
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
    if !items.find(place.parent_local_item_id).is_some_and(|directory| directory.local.is_some()) {
        return LocalPlaceAvailability::DirectoryNotShown(place.parent_local_item_id);
    }
    match items.find_shown_at(place) {
        Some(holder) if holder != item.local_item_id => LocalPlaceAvailability::NameTaken(holder),
        _ => LocalPlaceAvailability::Free,
    }
}

/// `item` as the plan decides on it: its row, with what `input` says of it and what the
/// other rows of the plan say about it.
fn describe_item<'a>(input: &SyncPlanInput, items: &PlanItems<'_>, item: &'a Item) -> ItemCase<'a> {
    let id = item.local_item_id;
    let visibility_of_listed_item = || input.folder_visibility.get(&id).copied().unwrap_or(FolderVisibility::NotKnownYet);
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
    /// The answer made of what was decided for each item, in the order given: its step, its
    /// wait, the rule that leaves it out, or its fault; nothing for an item nothing is done
    /// for.
    fn of_decisions(decisions: Vec<(LocalItemId, ItemDecision)>) -> Self {
        let mut plan = Self::default();
        for (item, decision) in decisions {
            match decision {
                ItemDecision::NothingToDo => {}
                ItemDecision::Step(step) => plan.steps.push(step),
                ItemDecision::Wait(reason) => plan.waits.push(ItemWait { item, reason }),
                ItemDecision::Exclusion(reason) => plan.exclusions.push(ItemExclusion { item, reason }),
                ItemDecision::Fault => plan.faults.push(PlanFault::RowAgainstRules(item)),
            }
        }
        plan
    }
}

/// The plan for what `input` gives: each item decided by itself, in the order given.
pub fn make_sync_plan(input: &SyncPlanInput) -> SyncPlan {
    let items = PlanItems::of(&input.items);
    let decisions = input.items.iter().map(|item| (item.local_item_id, decide_item(&describe_item(input, &items, item)))).collect();
    SyncPlan::of_decisions(decisions)
}

#[cfg(test)]
mod tests;
