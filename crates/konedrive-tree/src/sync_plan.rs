//! The sync plan: the steps that make the local and the OneDrive versions of the items agree
//! again. One pure function ([`make_sync_plan`]): it is given rows and a few facts as values
//! ([`SyncPlanInput`]) and answers the steps that can run now and, for every other item it
//! was given, why not ([`SyncPlan`]). No disk, no network, no clock and no store in it.
//!
//! It is laid out by what each piece decides. For one item by itself:
//!
//! - what each side did to it, from its row ([`side_change`]);
//! - what that means for it: the three tables ([`item_decision`]);
//! - what is not sent, and why ([`sending_obstacle`]).
//!
//! Between items. Each of these decisions reads the rows, what the caller said of the
//! items and what was answered for every item so far, and answers what it overrules; none
//! changes another's answer itself ([`item_decision`], `ItemAnswers`):
//!
//! - a directory moved here into what OneDrive has in it ([`move_cycle`]);
//! - the items that have or want one name of a directory of OneDrive ([`name_contest`]),
//!   and the name the one that steps aside gets ([`conflict_copy_name`]);
//! - the items the rows say OneDrive has under one name ([`onedrive_name_holders`]);
//! - a directory one side removed, and what the other side has in it
//!   ([`removed_directory`]);
//! - a directory that gives up its binding, and what was agreed to be in it
//!   ([`directory_binding`]);
//! - the items that wait for each other's names, and the temporary name one of them goes
//!   through ([`name_exchange`]);
//! - the guard against a mass removal ([`mass_removal`]);
//! - the items that wait for one another, and where that ends ([`wait_chain`]).
//!
//! And what they are made of: one thing to do ([`sync_step`]), and the rows of one plan, as
//! one item asks about another ([`plan_items`]).
//!
//! All the steps of one plan can run now, in any order and at once: where one must go
//! before another, the other waits for its item, and the plan made after the step sees it
//! done. One step per item.
//!
//! Nothing in the daemon calls it yet.

use std::collections::{BTreeMap, BTreeSet};

use konedrive_reason::Reason;

use crate::model::{FolderVisibility, Item, ItemKind, ItemPlace, LocalItemId};

/// The name of a conflict copy.
mod conflict_copy_name;
/// The binding of a directory that gives it up, and what was agreed to be in it.
mod directory_binding;
/// The guard against a mass removal.
mod mass_removal;
/// A directory moved here into what OneDrive has in it.
mod move_cycle;
/// A name of a directory of OneDrive, and the items that have or want it.
mod name_contest;
/// The items that wait for each other's names, and the temporary name one goes through.
mod name_exchange;
/// The items the rows say OneDrive has under one name of a directory.
mod onedrive_name_holders;
/// A directory one side removed, and what the other side has in it.
mod removed_directory;
/// The items that wait for one another, and where that ends.
mod wait_chain;
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
pub use name_exchange::make_temporary_name;
pub use sync_step::{FolderStep, ItemVersions, OneDriveStep, RowStep, SyncStep};

use plan_items::PlanItems;
use item_decision::{decide_content_while_place_waits, ItemAnswers, ItemCase, ItemDecision, LocalDirectoryBinding, LocalPlaceAvailability, Overruling};
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
/// **This is the whole of what a caller owes the plan.** What the plan can tell it was not
/// given, it answers instead of planning from a part; what it cannot tell is marked *not
/// enforced* below, and goes wrong silently.
///
/// **The rows.** `items` are a bounded number of the items that are unsent
/// ([`ItemStore::list_unsent`](crate::ItemStore::list_unsent)), whose OneDrive version is
/// not the synced one
/// ([`ItemStore::list_changed_in_onedrive`](crate::ItemStore::list_changed_in_onedrive)),
/// the pinned files that are only to be downloaded
/// ([`ItemStore::list_pinned_files_to_download`](crate::ItemStore::list_pinned_files_to_download))
/// and the shown items whose own placement says the folder cannot show them
/// ([`ItemStore::list_shown_items_that_cannot_be_shown`](crate::ItemStore::list_shown_items_that_cannot_be_shown):
/// they agree on everything and are in neither index; *not enforced*, an item not read so
/// stays in the folder). With every one of those that differs on a side — also one that
/// was given for another's sake — so that the set is closed:
///
/// 1. the directory it is in on each side and by what was agreed on, and the directories
///    above that one on each side up to the root. *Enforced*: an item waits for a directory
///    the plan needs and was not given ([`WaitReason::ItemNotGiven`]), and no removal is
///    planned for an item given without the directories above it in OneDrive.
/// 2. for a directory that was removed here, all OneDrive has in it and below it, at any
///    depth ([`ItemStore::list_below_in_onedrive`](crate::ItemStore::list_below_in_onedrive)),
///    and how many those are, in `onedrive_item_count_below`. *Enforced*: with another
///    number given than rows, or none, the directory waits
///    ([`WaitReason::ItemsInDirectoryNotGiven`]) and no removal is planned.
/// 3. for a directory the folder shows that OneDrive does not have, and for one OneDrive
///    has where the folder cannot show it (the caller asks
///    [`ItemStore::find_folder_visibility`](crate::ItemStore::find_folder_visibility) of
///    every directory it gives, also one that agrees on everything): what the folder shows
///    in it ([`ItemStore::list_children`](crate::ItemStore::list_children)), and how many
///    those are, in `shown_item_count_in`. *Enforced*, as for 2.
/// 4. the items that have its name in the same directory on either side, as OneDrive
///    compares names ([`FoldedName`](crate::FoldedName)), and those that have a conflict
///    copy's name it could take ([`make_conflict_copy_name`], from the first attempt on
///    until one is free on both sides) or its temporary name ([`make_temporary_name`], the
///    first). *Not enforced*: an item that has a name the plan asks about and was not given
///    is not known to the plan, and the name reads as free; the step that takes it is
///    refused by OneDrive or by its commit.
///
/// An item that agrees on everything is given for another's sake, or because it is a pinned
/// file whose content is not on disk. The root's row is among them once anything is in the
/// root.
///
/// **What only the caller knows of an item** is said by the item's id: a set names the
/// items it holds for, a map the items it has a value for. An item of which nothing is said
/// is one nothing holds for.
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
    /// How many items the unsent removals take from OneDrive that the user did not confirm,
    /// those of items that were not given too: every item that was removed here and that
    /// OneDrive has, a removed directory with all OneDrive has in it
    /// ([`ItemStore::count_unsent_removals`](crate::ItemStore::count_unsent_removals)).
    /// What the guard against a mass removal asks of.
    pub unsent_removal_count: u64,
    /// What the steps remember of a name OneDrive answered is taken: for each item whose
    /// sending OneDrive refused so, the place the folder showed it at when it was sent —
    /// its local place, also when the request named a temporary name. While the item is at
    /// that place here, its name there is another item's to the plan, whatever the plan's
    /// own rule for comparing names says.
    pub places_taken_in_onedrive: BTreeMap<LocalItemId, ItemPlace>,
    /// How many items the removals took from OneDrive that this client sent a short time
    /// ago and the user did not confirm: the rows whose removal OneDrive confirmed to a step
    /// since the count of confirmations the caller noted when that time began
    /// ([`ItemStore::count_removals_confirmed_since`](crate::ItemStore::count_removals_confirmed_since)).
    /// The guard against a mass removal counts them with the unsent ones, so that removals
    /// that come one by one, with a plan made between them, are held like those of one
    /// plan. The caller begins that time anew when the user confirms a mass removal.
    pub sent_removal_count: u64,
    /// For every directory given that was removed here: how many items the rows say
    /// OneDrive has in it and below it, at any depth.
    pub onedrive_item_count_below: BTreeMap<LocalItemId, u64>,
    /// For every directory given that the folder shows and that OneDrive does not have, or
    /// has where the folder cannot show it: how many items the folder shows in it.
    pub shown_item_count_in: BTreeMap<LocalItemId, u64>,
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
    /// It is a directory whose answer is made from what is in it, and the plan was not
    /// given all of that: which directory. The caller gives what [`SyncPlanInput`] says is
    /// given with such a directory, with the number of it, and asks again.
    ItemsInDirectoryNotGiven(LocalItemId),
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
///
/// When no step can run, something waits for a time, for the user, for a step that is
/// running, for a listing or for an item that was not given, or the plan says it is at
/// fault: an item never waits for another item whose own answer does not end the wait
/// ([`PlanFault::WaitWithoutEnd`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncPlan {
    /// The steps that can run now, one per item: those of what is unsent first, the item
    /// that came to be unsent first before the others, then those for what OneDrive changed,
    /// in the order the items were given.
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

/// Whether the folder can show `item` as OneDrive has it, by what `input` says and by the
/// names of the directories above it.
fn find_folder_visibility(input: &SyncPlanInput, items: &PlanItems<'_>, item: &Item) -> FolderVisibility {
    if item.onedrive.is_none() {
        return FolderVisibility::NotInOneDrive;
    }
    match input.folder_visibility.get(&item.local_item_id).copied().unwrap_or(FolderVisibility::NotKnownYet) {
        FolderVisibility::CanBeShown if is_in_directory_the_folder_cannot_name(items, item) => FolderVisibility::CannotBeShown,
        visibility => visibility,
    }
}

/// How OneDrive knows the directory the folder shows `item` in. A binding counts only while
/// the rows say OneDrive has the directory: one it removed, and one no listing told of yet,
/// keep their binding and are nothing to send into. Nor is a directory that is left here,
/// or one OneDrive has where the folder cannot show it.
fn find_local_directory_binding(input: &SyncPlanInput, items: &PlanItems<'_>, item: &Item) -> LocalDirectoryBinding {
    let Some(place) = item.local.as_ref().and_then(|local| local.place.as_ref()) else { return LocalDirectoryBinding::NoLocalPlace };
    let id = place.parent_local_item_id;
    let Some(directory) = items.find(id) else { return LocalDirectoryBinding::NotGiven(id) };
    if input.ignored_items.contains(&id) {
        return LocalDirectoryBinding::LeftHere(id);
    }
    match directory.onedrive.as_ref().and(directory.onedrive_item.clone()) {
        Some(_) if find_folder_visibility(input, items, directory) == FolderVisibility::CannotBeShown => LocalDirectoryBinding::CannotBeShown(id),
        Some(directory) => LocalDirectoryBinding::Bound(directory),
        None => LocalDirectoryBinding::NotBound(id),
    }
}

/// Whether the plan was given all that is in `item` where its answer is made from that
/// ([`SyncPlanInput`]): the caller's number of it is the number of rows given.
fn is_given_with_all_in_it(input: &SyncPlanInput, items: &PlanItems<'_>, item: &Item, folder_visibility: FolderVisibility) -> bool {
    let id = item.local_item_id;
    if item.kind != ItemKind::Directory || id == LocalItemId::ROOT {
        return true;
    }
    if removed_directory::is_directory_removed_here(item) {
        return input.onedrive_item_count_below.get(&id) == Some(&(items.list_below_in_onedrive(id).len() as u64));
    }
    let leaves_the_folder = item.local.is_some() && item.onedrive_item.is_some() && (item.onedrive.is_none() || folder_visibility == FolderVisibility::CannotBeShown);
    !leaves_the_folder || input.shown_item_count_in.get(&id) == Some(&(items.list_shown_in(id).len() as u64))
}

/// The first directory above `item` in OneDrive, on the way up to the root, whose row was
/// not given.
fn find_onedrive_directory_not_given(items: &PlanItems<'_>, item: &Item) -> Option<LocalItemId> {
    let directory_of = |item: &Item| item.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref()).map(|place| place.parent_local_item_id);
    let mut above = directory_of(item);
    for _ in 0..=items.count() {
        let directory = above?;
        match items.find(directory) {
            Some(row) => above = directory_of(row),
            None => return Some(directory),
        }
    }
    None
}

/// Whether the folder can take the place OneDrive has `item` at.
fn find_local_place_availability(items: &PlanItems<'_>, item: &Item) -> LocalPlaceAvailability {
    let Some(place) = item.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref()) else { return LocalPlaceAvailability::NoOneDrivePlace };
    if !is_directory_entry_name(&place.name) {
        return LocalPlaceAvailability::NameUnusable;
    }
    match items.find(place.parent_local_item_id) {
        Some(directory) if directory.local.is_some() => {}
        Some(directory) if directory.synced_place.is_some() || removed_directory::is_in_directory_removed_here(items, directory) => return LocalPlaceAvailability::DirectoryRemovedHere(place.parent_local_item_id),
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
    let folder_visibility = find_folder_visibility(input, items, item);
    ItemCase {
        item,
        folder_visibility,
        is_listing_under_way: input.is_listing_under_way,
        has_running_step: input.items_with_running_step.contains(&id),
        is_open_for_writing: input.items_open_for_writing.contains(&id),
        is_pinned: input.pinned_items.contains(&id),
        local_directory: find_local_directory_binding(input, items, item),
        local_place_availability: find_local_place_availability(items, item),
        first_shown_in_it: items.find_first_shown_in(id),
        is_given_with_all_in_it: is_given_with_all_in_it(input, items, item, folder_visibility),
        onedrive_directory_not_given: find_onedrive_directory_not_given(items, item),
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
    fn of_answers(items: &[Item], decisions: ItemAnswers) -> Self {
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
/// (`item_decision`); then the items are decided for each other, each kind of case by its
/// own module, in this order:
///
/// 1. a directory moved here into what OneDrive has in it (`move_cycle`);
/// 2. the items that have or want one name of a directory of OneDrive (`name_contest`);
/// 3. the items the rows say OneDrive has under one name (`onedrive_name_holders`);
/// 4. a directory one side removed, and what the other has in it (`removed_directory`);
///    then a directory that gives up its binding, and what was agreed to be in it
///    (`directory_binding`);
/// 5. the items that wait for each other's names (`name_exchange`);
/// 6. the guard against a mass removal (`mass_removal`);
/// 7. where each wait for another item ends (`wait_chain`); and, for a file whose wait
///    ends at the user, what the content table says meanwhile.
///
/// Each of them is given the answers so far to read and answers what it overrules; the
/// answers are changed here and nowhere else, and never for an item that is held back
/// (`ItemAnswers::overrule`).
pub fn make_sync_plan(input: &SyncPlanInput) -> SyncPlan {
    let items = PlanItems::of(&input.items);
    let cases: Vec<ItemCase<'_>> = input.items.iter().map(|item| describe_item(input, &items, item)).collect();
    let mut answers = ItemAnswers::of(&cases);
    let move_cycles = move_cycle::take_back_moves_into_own_content(&cases, &items, &answers);
    answers.overrule(move_cycles.overrulings);
    let name_contests = name_contest::settle_name_contests(&cases, &items, &answers, &input.computer_name, &input.places_taken_in_onedrive, &move_cycles.moves_taken_back);
    answers.overrule(name_contests.overrulings);
    answers.overrule(onedrive_name_holders::bring_one_holder_of_each_name_here(&cases, &items, &answers));
    answers.overrule(removed_directory::decide_removals_of_directories(&cases, &items, &answers));
    answers.overrule(removed_directory::keep_directories_that_hold_the_users_own(&cases, &items, &answers));
    answers.overrule(directory_binding::wait_for_directories_that_give_up_their_binding(&cases, &answers));
    answers.overrule(name_exchange::break_circles_of_names(&cases, &items, &name_contests.sendings_that_wait, &answers));
    answers.overrule(mass_removal::hold_mass_removal(input.unsent_removal_count + input.sent_removal_count, input.shown_item_count, &input.items_with_confirmed_removal, &answers));
    answers.overrule(wait_chain::end_waits_for_other_items(input.is_listing_under_way, &items, &answers));
    let waiting_for_the_user: BTreeSet<LocalItemId> = wait_chain::list_items_that_wait_for_the_user(&answers).into_iter().collect();
    let content_meanwhile = cases.iter().filter(|case| waiting_for_the_user.contains(&case.item.local_item_id)).filter_map(|case| Some(Overruling::of(case.item.local_item_id, ItemDecision::Step(decide_content_while_place_waits(case)?))));
    answers.overrule(content_meanwhile.collect());
    SyncPlan::of_answers(&input.items, answers)
}

#[cfg(test)]
mod tests;
