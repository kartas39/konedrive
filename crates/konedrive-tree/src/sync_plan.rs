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
//! - a directory whose binding moves to another row — it gives it up, or is merged —, and
//!   what was agreed to be in it ([`directory_binding`]);
//! - the items that wait for each other's names, and the temporary name one of them goes
//!   through, and the row a listing made of an item under that name ([`name_exchange`]);
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
use crate::{OwnUploadContent, TemporaryName};

/// The name of a conflict copy.
mod conflict_copy_name;
/// The binding of a directory that moves to another row, and what was agreed to be in it.
mod directory_binding;
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
/// Which steps of an item a next try holds, and the steps that wait for theirs.
mod tried_step;

pub use conflict_copy_name::make_conflict_copy_name;
pub use item_decision::READ_ONLY_FOLDER;
pub use name_exchange::make_temporary_name;
pub(crate) use removed_directory::{decide_directory_removal, DirectoryRemovalOutcome, ItemBelowRemovedDirectory};
pub use side_change::is_same_content_but_for_time;
pub(crate) use side_change::{compare_local_content, ContentChange};
pub use sync_step::{FolderStep, FolderStepKind, ItemVersions, OneDriveStep, RowStep, RowStepKind, SyncStep};
pub use tried_step::TriedStep;

use plan_items::PlanItems;
use item_decision::{decide_content_while_place_waits, ItemAnswers, ItemCase, ItemDecision, LocalDirectoryBinding, LocalPlaceAvailability, OneDriveContentOrigin, Overruling};
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
///    and how many those are, in `onedrive_item_count_below`: its removal is sent only when
///    all of that was shown here and removed here. *Enforced*: with another number given
///    than rows, or none, the directory waits ([`WaitReason::ItemsInDirectoryNotGiven`])
///    and no removal is planned. The rows and both numbers of this and the next point are
///    of one reading, one read transaction of the store: a number and rows read at one
///    time agree whatever the store says since, and a number taken from the rows given
///    (their length) agrees always, and enforces nothing. What covers the time after the
///    reading: the step asks the store again before its request leaves
///    ([`ItemStore::can_directory_removal_be_sent`](crate::ItemStore::can_directory_removal_be_sent)),
///    and its commit once more; for what OneDrive made in the directory or moved into
///    it after the last listing, nothing does: an item the folder shows that another
///    client moved into it goes with the removal, and is then taken off the folder as
///    one OneDrive removed.
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
/// is one nothing holds for: none of these is *enforced*, as the plan cannot see from the
/// rows what was left out. What a missing entry costs is said with each. Two of them guard
/// against a loss, and are asked a second time where the loss would happen:
/// `items_with_running_step` and `destinations_of_running_steps`, by the store before a
/// directory's removal leaves
/// ([`ItemStore::can_directory_removal_be_sent`](crate::ItemStore::can_directory_removal_be_sent)).
/// An entry that stands too long — a step that ended, a next try of an item that changed
/// since — only holds its item longer.
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
    /// The files a program holds open for writing. One left out is planned as its row
    /// says, while its content on disk is not the row's.
    pub items_open_for_writing: BTreeSet<LocalItemId>,
    /// The files whose content was changed within the quiet time, each with the time, in
    /// Unix seconds, at which its quiet time ends. One left out is sent at once.
    pub quiet_until: BTreeMap<LocalItemId, i64>,
    /// The items a step is running for: every item a request of this client is about that
    /// is not settled yet, and every item a step towards the folder or of the rows is
    /// running for. A request is not settled until its outcome is known — its commit was
    /// written, OneDrive refused it, or the item was read from OneDrive after it — and so
    /// also while its answer is lost: the caller owes that the step of such a request
    /// goes on running until the item was read. Across a stop of the daemon the caller
    /// keeps nothing, and owes instead that no plan's step towards OneDrive is started
    /// before a listing that began after the start has ended: that listing names what a
    /// request that was under way changed. One step per item:
    /// an item left out gets a second step beside the one that runs.
    pub items_with_running_step: BTreeSet<LocalItemId>,
    /// The directories the requests that are not settled yet send their items into: where
    /// a new directory is made, a new file sent, an item moved — as long as the request is
    /// not settled, as `items_with_running_step` says it, also when its answer is lost. The rows say such an item is there only once the step's commit is
    /// written; until then no removal of that directory, or of one above it, is sent, as
    /// it would take the item with it. A directory left out, with the item's own row
    /// agreeing on everything (it was moved back here meanwhile), is removed with the item
    /// in it: the one entry whose absence is a loss, and so asked of the caller again
    /// before the removal leaves.
    pub destinations_of_running_steps: BTreeSet<LocalItemId>,
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
    /// What the steps remember of a name OneDrive answered is taken: for each item whose
    /// sending OneDrive refused so, the place the folder showed it at when it was sent —
    /// its local place, also when the request named a temporary name. While the item is at
    /// that place here, its name there is another item's to the plan, whatever the plan's
    /// own rule for comparing names says.
    pub places_taken_in_onedrive: BTreeMap<LocalItemId, ItemPlace>,
    /// The items of OneDrive, each by the row a listing made for it, that an upload of
    /// this folder left with other content than was sent, with the file whose upload left
    /// it — none when that file's row went
    /// ([`ItemReader::find_mismatched_item`](crate::ItemReader::find_mismatched_item), asked
    /// by the caller of the binding of every row it gives; an item the folder has by now
    /// is not among them). Such an item is no item of another's: it is not shown, no file
    /// is merged with it or renamed to a conflict copy for it, and its name is not taken
    /// to the file it was left by. It waits to be removed in OneDrive, which is not the
    /// plan's: the step that sends the file again removes it first, and what is left when
    /// the file's row went is removed by the daemon by itself.
    // Once such an item was removed in OneDrive or found gone, its row loses its OneDrive version in the transaction that forgets the item (`StepMemory::forget_removed_mismatched_item`): the plan is never given a row that says OneDrive has a leftover that is gone.
    pub items_left_by_uploads: BTreeMap<LocalItemId, Option<LocalItemId>>,
    /// What the steps remember of a temporary name an item goes through in OneDrive
    /// ([`ItemMemory::temporary_name`](crate::ItemMemory)), for every item given that
    /// keeps one: the name, and the local place the item was on its way to. An item
    /// OneDrive has under its temporary name ([`TemporaryName::is_at`]) is there by this
    /// folder's own request: the folder is not moved to that name (`MoveItem`), and the
    /// item is sent on to its local place (`SendMove`) — also for a new item whose row
    /// was merged with the one a listing made of it under the temporary name.
    pub temporary_names: BTreeMap<LocalItemId, TemporaryName>,
    /// The files whose content in OneDrive is what an upload of this folder left there in
    /// place of what it sent, each with that content and the local one that was sent
    /// ([`ItemReader::find_own_upload_content`](crate::ItemReader::find_own_upload_content),
    /// asked by the caller of every file it gives). Such a content is no change of
    /// OneDrive's: the local content stands and is sent again (`SendContent`), the file
    /// does not take it (`TakeOneDriveContent`), no conflict copy is made against it, and
    /// a file removed here does not come back for it. *Enforced* as far as the row says
    /// it: an entry counts only while the row's OneDrive content is the one it names. The
    /// store answers the fact so too — a listing that wrote another content voids it —
    /// and forgets it once the content was sent again and confirmed or OneDrive's was
    /// taken; it goes with the item's row, and stays with the item when its binding moves.
    pub own_upload_contents: BTreeMap<LocalItemId, OwnUploadContent>,
    /// The time now, in Unix seconds: what a time the caller gives is compared with.
    pub now: i64,
    /// What is remembered of a try that failed: for each such item, and for the steps the
    /// try was of ([`TriedStep`]), the next try. While that is not before a time still to
    /// come, the item waits until then ([`WaitReason::Time`], with the reason of the
    /// failure) in place of such a step, and what is behind it waits for it. A try of
    /// what was sent holds every step towards OneDrive and no other: a step towards the
    /// folder is no try of a sending. A try of a step towards the folder or of the rows
    /// holds that one step, by its kind, and no other step of the item. The caller gives
    /// the first from what the store keeps of an unsent change, and the second from what
    /// it keeps itself of the steps it ran.
    pub next_tries: BTreeMap<(LocalItemId, TriedStep), NextTry>,
    /// What the steps remember of what OneDrive refused: the items it refuses for good as
    /// they are, and those too big for the space it has, each with the reason. Such an item
    /// waits for the user ([`WaitReason::User`]) in place of any step towards OneDrive; the
    /// caller forgets the entry when the item changes or OneDrive has more space.
    pub sending_refusals: BTreeMap<LocalItemId, Reason>,
    /// The items the check of the backing directory at start could not decide: nothing of
    /// such an item is sent, nothing is taken over for it, and no removal takes it from
    /// OneDrive ([`WaitReason::StartUpCheck`]). What the folder shows in such an item, or
    /// the two sides agreed is in it, at any depth, is held with it: *enforced*, the plan
    /// finds those by the rows it was given, and the caller names the items the check left
    /// and no others.
    pub items_left_by_start_up_check: BTreeSet<LocalItemId>,
    /// For every directory given that was removed here: how many items the rows say
    /// OneDrive has in it and below it, at any depth, counted by the store in the reading
    /// the rows are of.
    pub onedrive_item_count_below: BTreeMap<LocalItemId, u64>,
    /// For every directory given that the folder shows and that OneDrive does not have, or
    /// has where the folder cannot show it: how many items the folder shows in it, counted
    /// by the store in the reading the rows are of.
    pub shown_item_count_in: BTreeMap<LocalItemId, u64>,
}

/// The next try of a step of an item, after a try that failed: of what is sent of it, as
/// the steps remember it ([`ItemStore::find_item_memory`](crate::ItemStore::find_item_memory)),
/// or of a step towards the folder or of the rows, as the caller remembers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextTry {
    /// The time, in Unix seconds, before which the step is not tried again.
    pub not_before: i64,
    /// Why the last try failed: what is said of the item while it waits.
    pub reason: Reason,
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
    /// The check of the backing directory at start could not decide the item: it waits for
    /// a check that does.
    StartUpCheck,
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
    /// An upload of this folder left the item in OneDrive with other content than was
    /// sent ([`SyncPlanInput::items_left_by_uploads`]): the folder does not show it, and
    /// its removal in OneDrive is not the plan's.
    LeftByUpload,
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
/// running, for a check at start, for a listing or for what was not given, or the plan says
/// it is at fault: an item never waits for another item whose own answer does not end the
/// wait ([`PlanFault::WaitWithoutEnd`]).
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

/// Whose the content is that OneDrive has of `item`: this folder's own upload while the
/// content `input` names for it is the one the row says OneDrive has.
fn find_onedrive_content_origin(input: &SyncPlanInput, item: &Item) -> OneDriveContentOrigin {
    let in_onedrive = item.onedrive.as_ref().and_then(|onedrive| onedrive.content.as_ref());
    match input.own_upload_contents.get(&item.local_item_id) {
        Some(upload) if in_onedrive == Some(&upload.onedrive_content) => OneDriveContentOrigin::OwnUpload,
        _ => OneDriveContentOrigin::OneDrive,
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

/// Whether the folder can take the place OneDrive has `item` at. The place is no place for
/// the folder to take where it is the temporary name `input` says the item goes through
/// ([`TemporaryName::is_at`]) and the folder shows the item: asked first, whatever the
/// directory and the name are here. Nor is it where the folder does not show `item` and
/// the place is the temporary name of another item that has no binding yet: the row a
/// listing made of what that item's request put there. An item that has a binding is
/// another item of OneDrive than the listed one, whatever it remembers.
fn find_local_place_availability(input: &SyncPlanInput, items: &PlanItems<'_>, item: &Item) -> LocalPlaceAvailability {
    let Some(place) = item.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref()) else { return LocalPlaceAvailability::NoOneDrivePlace };
    // TODO: check what becomes of an item the folder does not show that OneDrive has under its temporary name (removed here, its content changed in OneDrive: it comes back), and fix if needed: it has no local place to be sent on to, and is shown under the temporary name as the place of any item.
    if item.local.is_some() && input.temporary_names.get(&item.local_item_id).is_some_and(|temporary_name| temporary_name.is_at(place)) {
        return LocalPlaceAvailability::IsTemporaryName;
    }
    if item.local.is_none() {
        let is_new_here = |another: LocalItemId| items.find(another).is_some_and(|keeper| keeper.local.is_some() && keeper.onedrive_item.is_none());
        if let Some((another, _)) = input.temporary_names.iter().find(|(another, temporary_name)| **another != item.local_item_id && temporary_name.is_at(place) && is_new_here(**another)) {
            return LocalPlaceAvailability::IsTemporaryNameOf(*another);
        }
    }
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

/// Whether the check at start left `item`: the caller named it, or a directory the folder
/// shows it in, or the two sides agreed it is in, at any depth.
fn is_left_by_start_up_check(input: &SyncPlanInput, items: &PlanItems<'_>, item: &Item) -> bool {
    let left = &input.items_left_by_start_up_check;
    let ways_up: [fn(&Item) -> Option<&ItemPlace>; 2] = [|item| item.local.as_ref().and_then(|local| local.place.as_ref()), |item| item.synced_place.as_ref()];
    left.contains(&item.local_item_id)
        || (!left.is_empty()
            && ways_up.iter().any(|place_of| {
                let mut above = place_of(item).map(|place| place.parent_local_item_id);
                for _ in 0..items.count() {
                    match above {
                        Some(directory) if left.contains(&directory) => return true,
                        Some(directory) => above = items.find(directory).and_then(place_of).map(|place| place.parent_local_item_id),
                        None => return false,
                    }
                }
                false
            }))
}

/// Every item of `input` as the plan decides on it ([`describe_item`]), and with a
/// directory that was removed here, what the rows and the caller say of all OneDrive has
/// below it.
fn describe_items<'a>(input: &'a SyncPlanInput, items: &PlanItems<'_>) -> Vec<ItemCase<'a>> {
    let mut cases: Vec<ItemCase<'_>> = input.items.iter().map(|item| describe_item(input, items, item)).collect();
    let mut below = removed_directory::list_below_removed_directories(&cases, items);
    for case in &mut cases {
        case.below_in_onedrive = below.remove(&case.item.local_item_id).unwrap_or_default();
    }
    cases
}

/// `item` as the plan decides on it: its row, with what `input` says of it and what the
/// other rows of the plan say about it; but what is below a directory that was removed
/// here ([`describe_items`]).
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
        local_place_availability: find_local_place_availability(input, items, item),
        is_left_by_upload: input.items_left_by_uploads.contains_key(&id),
        onedrive_content_origin: find_onedrive_content_origin(input, item),
        is_left_by_start_up_check: is_left_by_start_up_check(input, items, item),
        is_sent_into_by_running_step: input.destinations_of_running_steps.contains(&id),
        first_shown_in_it: items.find_first_shown_in(id),
        is_given_with_all_in_it: is_given_with_all_in_it(input, items, item, folder_visibility),
        onedrive_directory_not_given: find_onedrive_directory_not_given(items, item),
        holds_what_was_agreed_on_in_it: items.list_shown_in(id).iter().filter_map(|shown| items.find(*shown)).any(|shown| {
            [shown.synced_place.as_ref(), shown.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref())].into_iter().flatten().any(|place| place.parent_local_item_id == id)
        }),
        below_in_onedrive: Vec::new(),
        sending: SendingConditions {
            is_ignored: input.ignored_items.contains(&id),
            name_refusal: input.name_refusals.get(&id).cloned(),
            size: item.local.as_ref().and_then(|local| local.content.as_ref()).map(|content| content.size()),
            largest_file_size: input.largest_file_size,
            quiet_until: input.quiet_until.get(&id).copied(),
            is_onedrive_full: input.is_onedrive_full,
            sending_state: input.sending_state,
            unsent_since: item.unsent_since,
            refusal: input.sending_refusals.get(&id).cloned(),
            next_try: input.next_tries.get(&(id, TriedStep::TowardsOneDrive)).cloned(),
            now: input.now,
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
///    then a new item and the row a listing made of it under its temporary name
///    (`name_exchange`), which are one: after the names, so that the merge stands in
///    place of the item's sending or its wait for a name, and before the circles of names
///    (5), which would send the item again under the next temporary name;
/// 3. the items the rows say OneDrive has under one name (`onedrive_name_holders`);
/// 4. what OneDrive has below a directory that was removed here, while that directory's
///    removal is still to be sent, and a directory OneDrive removed that holds the user's
///    own (`removed_directory`); then a directory whose binding moves to another row, and
///    what was agreed to be in it (`directory_binding`);
/// 5. the items that wait for each other's names (`name_exchange`);
/// 6. where each wait for another item ends (`wait_chain`); and, for a file whose wait
///    ends at the user, what the content table says meanwhile.
///
/// A step towards the folder or of the rows whose own next try is later waits until then
/// (`tried_step`): asked of what each item is answered by itself, before the items are
/// decided for each other, so that those decisions read a wait; and once more of the steps
/// those decisions made, before the waits are followed to their ends.
///
/// Each of them is given the answers so far to read and answers what it overrules; the
/// answers are changed here and nowhere else, and never for an item that is held back
/// (`ItemAnswers::overrule`).
///
/// What depends on the order: the names (2) are settled before what is below a removed
/// directory is made to wait for it (4), and read a file removed here that waits for
/// another item as one that comes back and stays at its name — true of what an item is
/// answered by itself, and not of the wait (4) gives everything below a removed directory,
/// whose removal frees its name. The waits are followed to their ends (6) last, over the
/// answers as they then stand.
pub fn make_sync_plan(input: &SyncPlanInput) -> SyncPlan {
    let items = PlanItems::of(&input.items);
    let cases = describe_items(input, &items);
    let mut answers = ItemAnswers::of(&cases);
    answers.overrule(tried_step::hold_steps_until_next_try(&input.next_tries, input.now, &answers));
    let move_cycles = move_cycle::take_back_moves_into_own_content(&cases, &items, &answers);
    answers.overrule(move_cycles.overrulings);
    let name_contests = name_contest::settle_name_contests(&cases, &items, &answers, &input.computer_name, &input.places_taken_in_onedrive, &input.items_left_by_uploads, &move_cycles.moves_taken_back);
    answers.overrule(name_contests.overrulings);
    answers.overrule(name_exchange::merge_rows_listed_under_temporary_names(&cases, &answers));
    answers.overrule(onedrive_name_holders::bring_one_holder_of_each_name_here(&cases, &items, &answers));
    answers.overrule(removed_directory::wait_for_removals_of_directories(&cases, &items));
    answers.overrule(removed_directory::keep_directories_that_hold_the_users_own(&cases, &items, &answers));
    answers.overrule(directory_binding::wait_for_directories_that_give_up_their_binding(&cases, &answers));
    answers.overrule(name_exchange::break_circles_of_names(&cases, &items, &name_contests.sendings_that_wait, &answers));
    answers.overrule(tried_step::hold_steps_until_next_try(&input.next_tries, input.now, &answers));
    answers.overrule(wait_chain::end_waits_for_other_items(input.is_listing_under_way, &items, &answers));
    let waiting_for_the_user: BTreeSet<LocalItemId> = wait_chain::list_items_that_wait_for_the_user(&answers).into_iter().collect();
    let content_meanwhile = cases.iter().filter(|case| waiting_for_the_user.contains(&case.item.local_item_id)).filter_map(|case| Some(Overruling::of(case.item.local_item_id, ItemDecision::Step(decide_content_while_place_waits(case)?))));
    answers.overrule(content_meanwhile.collect());
    SyncPlan::of_answers(&input.items, answers)
}

#[cfg(test)]
pub(crate) mod tests;
