//! What the two sides' changes mean for one item by itself: the three tables of the sync
//! plan (whether the item exists, its place, its content), cell by cell. A decision of typed
//! cases: it is given the item's row and what the plan knows about it as values
//! ([`ItemCase`]) and answers one step, one wait, or that nothing is done. It reads no other
//! row and decides nothing between items.
//!
//! One step per item: where an item needs more than one (a move and new content), the
//! place goes first, and the plan made after it decides the rest.

use std::collections::{HashMap, HashSet};

use konedrive_reason::Reason;

use super::removed_directory::{decide_directory_removal, is_directory_removed_here, DirectoryRemovalOutcome, ItemBelowRemovedDirectory};
use super::sending_obstacle::{find_sending_obstacle, SentPart, SendingConditions, SendingObstacle};
use super::side_change::{compare_local_content, ContentChange, PlaceChange, SideChange};
use super::sync_step::{FolderStep, ItemVersions, OneDriveStep, RowStep, SyncStep};
use super::{ExclusionReason, PlanFault, WaitReason};
use crate::model::{ContentPresence, FolderVisibility, Item, ItemKind, LocalItemId, OneDriveItemReference};

/// What the plan answers for one item by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ItemDecision {
    /// The two sides agree, or the row is no item: a row with a binding alone.
    NothingToDo,
    Step(SyncStep),
    Wait(WaitReason),
    /// A rule leaves the item out: nothing is done for it, and it waits for nothing.
    Exclusion(ExclusionReason),
    /// What the plan found of the item that must never be.
    Fault(PlanFault),
}

/// What a decision between items answers for one item, in place of what was answered for it
/// before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Overruling {
    pub(crate) item: LocalItemId,
    pub(crate) decision: ItemDecision,
}

impl Overruling {
    pub(crate) fn of(item: LocalItemId, decision: ItemDecision) -> Self {
        Self { item, decision }
    }
}

/// What the plan answers for each item it was given: the answer of every item by its id,
/// and the order the items were given in.
///
/// A decision between items reads it and changes nothing of it: it answers its
/// [`Overruling`]s, and [`make_sync_plan`](super::make_sync_plan) alone puts them in place
/// ([`overrule`](Self::overrule)). An item that is held back whatever its row and the other
/// items say — a step is running for it, a program holds it open for writing, the check at
/// start could not decide it ([`find_hold`]) — keeps that answer: no overruling takes its
/// place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ItemAnswers {
    order: Vec<LocalItemId>,
    by_item: HashMap<LocalItemId, ItemDecision>,
    held: HashSet<LocalItemId>,
}

impl ItemAnswers {
    /// What each of `cases` is answered by itself ([`decide_item`]).
    pub(crate) fn of(cases: &[ItemCase<'_>]) -> Self {
        let mut all = Self { order: Vec::new(), by_item: HashMap::new(), held: HashSet::new() };
        for case in cases {
            let (item, decision) = (case.item.local_item_id, decide_item(case));
            if find_hold(case).is_some_and(|hold| decision == ItemDecision::Wait(hold)) {
                all.held.insert(item);
            }
            if all.by_item.insert(item, decision).is_none() {
                all.order.push(item);
            }
        }
        all
    }

    /// The answer for `item`; `None` for an item that was not given.
    pub(crate) fn find(&self, item: LocalItemId) -> Option<&ItemDecision> {
        self.by_item.get(&item)
    }

    /// Whether `item` is held back whatever the other items say: its answer stands.
    pub(crate) fn is_held(&self, item: LocalItemId) -> bool {
        self.held.contains(&item)
    }

    /// The items, in the order they were given.
    pub(crate) fn items(&self) -> &[LocalItemId] {
        &self.order
    }

    /// Each of `overrulings` is the answer for its item from now on, in their order.
    /// Nothing for an item that was not given, and nothing for an item that is held back.
    pub(crate) fn overrule(&mut self, overrulings: Vec<Overruling>) {
        for Overruling { item, decision } in overrulings {
            if self.held.contains(&item) {
                continue;
            }
            if let Some(answer) = self.by_item.get_mut(&item) {
                *answer = decision;
            }
        }
    }

    /// Every item with its answer, in the order the items were given.
    pub(crate) fn into_ordered(mut self) -> Vec<(LocalItemId, ItemDecision)> {
        self.order.iter().filter_map(|item| self.by_item.remove(item).map(|decision| (*item, decision))).collect()
    }
}

/// The answers as one decision between items has them while it decides: those it was given,
/// with its own overrulings so far in place. A decision that settles one item after another
/// reads here what it answered for the earlier ones. Its overrulings are what it answers.
pub(crate) struct OverruledAnswers<'a> {
    given: &'a ItemAnswers,
    overrulings: Vec<Overruling>,
    overruled: HashMap<LocalItemId, ItemDecision>,
}

impl<'a> OverruledAnswers<'a> {
    pub(crate) fn of(given: &'a ItemAnswers) -> Self {
        Self { given, overrulings: Vec::new(), overruled: HashMap::new() }
    }

    /// The answer for `item` as it stands now; `None` for an item that was not given.
    pub(crate) fn find(&self, item: LocalItemId) -> Option<&ItemDecision> {
        self.overruled.get(&item).or_else(|| self.given.find(item))
    }

    pub(crate) fn is_held(&self, item: LocalItemId) -> bool {
        self.given.is_held(item)
    }

    pub(crate) fn items(&self) -> &[LocalItemId] {
        self.given.items()
    }

    /// `decision` is the answer for `item` from now on, as [`ItemAnswers::overrule`] will
    /// put it: not for an item that was not given, nor for one that is held back.
    pub(crate) fn overrule(&mut self, item: LocalItemId, decision: ItemDecision) {
        if self.given.find(item).is_none() || self.given.is_held(item) {
            return;
        }
        self.overruled.insert(item, decision.clone());
        self.overrulings.push(Overruling::of(item, decision));
    }

    pub(crate) fn into_overrulings(self) -> Vec<Overruling> {
        self.overrulings
    }
}

/// The directory the folder shows an item in, as OneDrive knows that directory: where a
/// step towards OneDrive sends the item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalDirectoryBinding {
    /// OneDrive has the directory, as this item.
    Bound(OneDriveItemReference),
    /// The directory has no binding, or OneDrive does not have it by its row: which
    /// directory.
    NotBound(LocalItemId),
    /// The directory's row was not given: which directory.
    NotGiven(LocalItemId),
    /// The directory is left here, and so never in OneDrive: which directory.
    LeftHere(LocalItemId),
    /// OneDrive has the directory where the folder cannot show it: which directory. What
    /// the user made is not sent into it.
    CannotBeShown(LocalItemId),
    /// The folder does not show the item.
    NoLocalPlace,
}

/// Whether the folder can take the place OneDrive has an item at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalPlaceAvailability {
    /// The folder shows the directory, and nothing but the item itself has the name there.
    Free,
    /// The folder does not show the directory yet, or its row was not given: which directory.
    DirectoryNotShown(LocalItemId),
    /// The directory was removed here, or OneDrive has it in one that was, and the folder
    /// will not show it: which.
    DirectoryRemovedHere(LocalItemId),
    /// The folder shows another item under that name there: which.
    NameTaken(LocalItemId),
    /// The name is not the name of a directory entry.
    NameUnusable,
    /// OneDrive has no place for the item.
    NoOneDrivePlace,
}

/// One item as the plan decides on it: its row, and what the plan was given or found among
/// the other rows about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ItemCase<'a> {
    pub(crate) item: &'a Item,
    /// Whether the folder can show the item as OneDrive has it.
    pub(crate) folder_visibility: FolderVisibility,
    /// A listing is being written: what the rows say OneDrive removed, or the folder
    /// cannot show, a later page may take back.
    pub(crate) is_listing_under_way: bool,
    pub(crate) has_running_step: bool,
    pub(crate) is_open_for_writing: bool,
    /// The check of the backing directory at start could not decide the item, or a
    /// directory the folder shows it in or the two sides agreed it is in, at any depth.
    pub(crate) is_left_by_start_up_check: bool,
    /// A request that is not settled sends an item into the item, a directory.
    pub(crate) is_sent_into_by_running_step: bool,
    pub(crate) is_pinned: bool,
    pub(crate) local_directory: LocalDirectoryBinding,
    pub(crate) local_place_availability: LocalPlaceAvailability,
    /// One of the items the folder shows in the item, a directory.
    pub(crate) first_shown_in_it: Option<LocalItemId>,
    /// The item is no directory whose answer is made from what is in it, or the plan was
    /// given all of that: for a directory that was removed here, all OneDrive has in it
    /// and below it; for one that leaves the folder or gives up its binding, all the
    /// folder shows in it.
    pub(crate) is_given_with_all_in_it: bool,
    /// The first directory above the item in OneDrive, on the way up to the root, whose row
    /// was not given.
    pub(crate) onedrive_directory_not_given: Option<LocalItemId>,
    /// The item is a directory in which the folder shows an item the two sides agreed is
    /// in it, or OneDrive has in it: its binding is no guess, whatever its own row agrees
    /// on.
    pub(crate) holds_what_was_agreed_on_in_it: bool,
    /// For a directory that was removed here: what the rows and the caller say of each item
    /// OneDrive has below it, at any depth. What its removal is decided from.
    pub(crate) below_in_onedrive: Vec<ItemBelowRemovedDirectory>,
    pub(crate) sending: SendingConditions,
}

impl ItemCase<'_> {
    fn versions(&self) -> ItemVersions {
        ItemVersions::of(self.item)
    }

    /// What becomes of the removal of the item, a directory that was removed here
    /// ([`decide_directory_removal`]). `None` for any other item, and where the directory
    /// is answered before its removal is decided: it is held back, or the plan was not
    /// given all that is in it or the directories above it.
    pub(crate) fn find_directory_removal_outcome(&self) -> Option<DirectoryRemovalOutcome> {
        if !is_directory_removed_here(self.item) || find_hold(self).is_some() || !self.is_given_with_all_in_it || self.onedrive_directory_not_given.is_some() {
            return None;
        }
        Some(decide_directory_removal(self.is_listing_under_way, &self.below_in_onedrive))
    }

    /// Whether the item is a directory that was removed here whose removal is of the folder
    /// alone: nothing is sent for it, and nothing below it waits for a removal.
    pub(crate) fn is_removal_of_folder_alone(&self) -> bool {
        self.find_directory_removal_outcome() == Some(DirectoryRemovalOutcome::IsOfFolderAlone)
    }
}

/// The row of the item of `case`, or what the plan was given about it, says what no
/// operation leaves.
fn row_fault(case: &ItemCase<'_>) -> ItemDecision {
    ItemDecision::Fault(PlanFault::RowAgainstRules(case.item.local_item_id))
}

/// What the rows say of the item of `case` in OneDrive cannot be judged yet: it waits for the
/// listing that is under way, and with none, the rows are at fault and a full listing mends
/// them.
fn wait_for_listing(case: &ItemCase<'_>) -> ItemDecision {
    if case.is_listing_under_way {
        ItemDecision::Wait(WaitReason::Listing)
    } else {
        ItemDecision::Fault(PlanFault::FullListingNeeded(case.item.local_item_id))
    }
}

fn onedrive_step(step: OneDriveStep) -> ItemDecision {
    ItemDecision::Step(SyncStep::OneDrive(step))
}

fn folder_step(step: FolderStep) -> ItemDecision {
    ItemDecision::Step(SyncStep::Folder(step))
}

fn row_step(step: RowStep) -> ItemDecision {
    ItemDecision::Step(SyncStep::Row(step))
}

/// What holds the item of `case` back whatever its row and the other items say: a step is
/// running for it — a request about it is not settled —, a program holds it open for
/// writing, the check of the backing directory at start could not decide it or a directory
/// it is in, or it is a directory the folder does not show into which a request that is
/// not settled sends an item. Nothing of such an item is planned, by itself or between
/// items: nothing of it is sent, and nothing is taken over for it. These are what the rows
/// cannot say; what the rows do say of a directory's removal is the removal's own
/// condition ([`decide_directory_removal`]).
pub(crate) fn find_hold(case: &ItemCase<'_>) -> Option<WaitReason> {
    if case.has_running_step {
        Some(WaitReason::RunningStep)
    } else if case.is_open_for_writing {
        Some(WaitReason::User(Reason::OpenForWriting))
    } else if case.is_left_by_start_up_check {
        Some(WaitReason::StartUpCheck)
    } else if case.is_sent_into_by_running_step && case.item.local.is_none() {
        // A directory the folder does not show any more, into which a step still sends an
        // item: its removal would take that item with it.
        Some(WaitReason::RunningStep)
    } else {
        None
    }
}

/// What the plan answers for the item of `case`. The first that holds:
///
/// - the root of the folder is never planned, and a row the rules of the rows refuse is a
///   fault;
/// - a row with no version on either side and nothing agreed on is no item;
/// - an item with a step running waits for it;
/// - an item a program holds open for writing is not planned at all;
/// - a directory whose answer is made from what is in it, given without all of that, says
///   so;
/// - otherwise the tables decide ([`decide_existence`]).
pub(crate) fn decide_item(case: &ItemCase<'_>) -> ItemDecision {
    let item = case.item;
    if item.local_item_id == LocalItemId::ROOT {
        return ItemDecision::NothingToDo;
    }
    if item.onedrive_item.is_none() && (item.onedrive.is_some() || item.synced_place.is_some() || item.synced_content.is_some()) {
        return row_fault(case);
    }
    let (here, in_onedrive) = (SideChange::of_local(item), SideChange::of_onedrive(item));
    if (here, in_onedrive) == (SideChange::NeverWas, SideChange::NeverWas) {
        return ItemDecision::NothingToDo;
    }
    if let Some(hold) = find_hold(case) {
        return ItemDecision::Wait(hold);
    }
    if !case.is_given_with_all_in_it {
        return ItemDecision::Wait(WaitReason::ItemsInDirectoryNotGiven(item.local_item_id));
    }
    decide_existence(case, here, in_onedrive)
}

/// What the content table answers for the file of `case` while its place waits: its step,
/// when the table has one. `None` for an item that is held back, for one that is not a file
/// both sides have, and when nothing of its content is to do or what is to do waits
/// itself.
///
/// One step per item, the place first: but a place that waits for the user may wait for
/// good, and the content of the file is OneDrive's and the folder's all the same.
pub(crate) fn decide_content_while_place_waits(case: &ItemCase<'_>) -> Option<SyncStep> {
    let item = case.item;
    if find_hold(case).is_some() || item.kind != ItemKind::File {
        return None;
    }
    // Of a file the folder cannot show, the content that was changed here alone is decided,
    // as where its place does not wait ([`decide_item_that_cannot_be_shown`]).
    let is_decided = |here: ContentChange| match case.folder_visibility {
        FolderVisibility::CanBeShown => true,
        FolderVisibility::CannotBeShown => !case.is_listing_under_way && here != ContentChange::Same,
        FolderVisibility::NotKnownYet | FolderVisibility::NotInOneDrive => false,
    };
    match (SideChange::of_local(item), SideChange::of_onedrive(item)) {
        (SideChange::IsThere { content: here, .. }, SideChange::IsThere { .. }) if !is_decided(here) => None,
        (SideChange::IsThere { content: here, .. }, SideChange::IsThere { content: in_onedrive, .. }) => match decide_content(case, here, in_onedrive) {
            ItemDecision::Step(step) => Some(step),
            ItemDecision::NothingToDo | ItemDecision::Wait(_) | ItemDecision::Exclusion(_) | ItemDecision::Fault(_) => None,
        },
        (SideChange::IsThere { .. } | SideChange::Removed | SideChange::NeverWas, _) => None,
    }
}

/// The table "whether the item exists": what was done here against what was done in
/// OneDrive.
pub(crate) fn decide_existence(case: &ItemCase<'_>, here: SideChange, in_onedrive: SideChange) -> ItemDecision {
    use SideChange::{IsThere, NeverWas, Removed};
    match (here, in_onedrive) {
        (IsThere { place: here_place, content: here_content }, IsThere { place: onedrive_place, content: onedrive_content }) => {
            decide_item_on_both_sides(case, (here_place, here_content), (onedrive_place, onedrive_content))
        }
        (IsThere { place, content }, Removed) => decide_removed_in_onedrive(case, place, content),
        (IsThere { .. }, NeverWas) if case.item.onedrive_item.is_some() => decide_bound_item_onedrive_never_listed(case),
        (IsThere { .. }, NeverWas) => decide_new_here(case),
        (Removed, IsThere { content, .. }) => decide_removed_here(case, content),
        (Removed, Removed) if case.is_listing_under_way => ItemDecision::Wait(WaitReason::Listing),
        (Removed, Removed) => row_step(RowStep::AgreeOnRemoval { item: case.versions() }),
        (NeverWas, IsThere { .. }) => show_onedrive_item(case),
        (NeverWas, NeverWas) => ItemDecision::NothingToDo,
        // Both sides are read against one synced version: where one side removed the item
        // something was agreed on, and the other cannot have never had it.
        (Removed, NeverWas) | (NeverWas, Removed) => row_fault(case),
    }
}

/// The folder shows the item no more; a directory waits for what the folder shows in it.
fn remove_item_here(case: &ItemCase<'_>) -> ItemDecision {
    match (case.item.kind, case.first_shown_in_it) {
        (ItemKind::Directory, Some(shown_in_it)) => ItemDecision::Wait(WaitReason::AnotherItem(shown_in_it)),
        _ => folder_step(FolderStep::RemoveItem { item: case.versions() }),
    }
}

/// Is there here, removed in OneDrive. Nothing is done while a listing is under way.
///
/// - A file neither moved nor changed here goes, its clean copy with it.
/// - A file whose content was changed here is sent again as a new item.
/// - A file moved here is sent again at its new place when its content is on disk, and goes
///   when it is not.
/// - A directory goes, moved here or not, once the folder shows nothing in it.
fn decide_removed_in_onedrive(case: &ItemCase<'_>, place: PlaceChange, content: ContentChange) -> ItemDecision {
    if case.is_listing_under_way {
        return ItemDecision::Wait(WaitReason::Listing);
    }
    match (case.item.kind, place, content, case.item.content_presence) {
        (ItemKind::Directory, _, _, _) => remove_item_here(case),
        (ItemKind::File, _, ContentChange::Changed | ContentChange::NotKnownWithoutHash, _) | (ItemKind::File, PlaceChange::Moved, ContentChange::Same, ContentPresence::OnDisk) => {
            row_step(RowStep::GiveUpBinding { item: case.versions() })
        }
        (ItemKind::File, PlaceChange::Same, ContentChange::Same, _) | (ItemKind::File, PlaceChange::Moved, ContentChange::Same, ContentPresence::NotOnDisk) => remove_item_here(case),
    }
}

/// The folder shows an item that has a binding, and no listing said what OneDrive has of
/// it: a store that was lost, or a daemon that died between marking an uploaded file and
/// its row. A listing under way will say; with none, OneDrive does not have the item, and
/// it is sent as a new one.
fn decide_bound_item_onedrive_never_listed(case: &ItemCase<'_>) -> ItemDecision {
    if case.is_listing_under_way {
        return ItemDecision::Wait(WaitReason::Listing);
    }
    row_step(RowStep::GiveUpBinding { item: case.versions() })
}

/// What keeps `sent_part` of the item from being sent, as the plan answers it; `None` when it
/// is sent.
fn find_decision_not_to_send(case: &ItemCase<'_>, sent_part: SentPart) -> Option<ItemDecision> {
    Some(match find_sending_obstacle(sent_part, &case.sending)? {
        SendingObstacle::Ignored => ItemDecision::Exclusion(ExclusionReason::NameIgnored),
        SendingObstacle::ReadOnlyFolder => ItemDecision::Wait(WaitReason::User(Reason::NotAllowed(Some(READ_ONLY_FOLDER.to_owned())))),
        SendingObstacle::NameRefused(reason) => ItemDecision::Wait(WaitReason::User(reason)),
        SendingObstacle::TooLarge => ItemDecision::Wait(WaitReason::User(Reason::TooLarge)),
        SendingObstacle::OneDriveFull => ItemDecision::Wait(WaitReason::User(Reason::WaitingForSpace)),
        SendingObstacle::QuietTime { until } => ItemDecision::Wait(WaitReason::Time { until, reason: Reason::Changed }),
        SendingObstacle::Refused(reason) => ItemDecision::Wait(WaitReason::User(reason)),
        SendingObstacle::TriedAgainLater { until, reason } => ItemDecision::Wait(WaitReason::Time { until, reason }),
    })
}

/// What stands behind "not allowed now" for an item that waits for its folder to be
/// read-write again.
pub(crate) const READ_ONLY_FOLDER: &str = "the folder is read-only";

/// A step towards OneDrive that sends the item into the directory the folder shows it in,
/// under its name here: made by `step` from that directory's binding and the name. Waits
/// for the directory while OneDrive does not have it or its row was not given, and for a
/// listing while OneDrive's root is not known for the root of the folder. Waits for the user
/// where the directory is left here, and where OneDrive has it and the folder cannot show
/// it: what the user made is not sent into what the folder cannot show; a listing under way
/// may yet say otherwise of that directory.
fn send_into_local_directory(case: &ItemCase<'_>, step: impl FnOnce(OneDriveItemReference, std::ffi::OsString) -> OneDriveStep) -> ItemDecision {
    let Some(place) = case.item.local.as_ref().and_then(|local| local.place.as_ref()) else { return row_fault(case) };
    match &case.local_directory {
        LocalDirectoryBinding::Bound(directory) => onedrive_step(step(directory.clone(), place.name.clone())),
        LocalDirectoryBinding::NotBound(LocalItemId::ROOT) => wait_for_listing(case),
        LocalDirectoryBinding::NotBound(directory) | LocalDirectoryBinding::NotGiven(directory) => ItemDecision::Wait(WaitReason::AnotherItem(*directory)),
        LocalDirectoryBinding::CannotBeShown(_) if case.is_listing_under_way => ItemDecision::Wait(WaitReason::Listing),
        LocalDirectoryBinding::LeftHere(_) | LocalDirectoryBinding::CannotBeShown(_) => ItemDecision::Wait(WaitReason::User(Reason::Parent)),
        LocalDirectoryBinding::NoLocalPlace => row_fault(case),
    }
}

/// Is there here, never was in OneDrive: new here, it is sent, unless a rule says it is not.
fn decide_new_here(case: &ItemCase<'_>) -> ItemDecision {
    let item = case.versions();
    match case.item.kind {
        ItemKind::Directory => find_decision_not_to_send(case, SentPart::NewDirectory)
            .unwrap_or_else(|| send_into_local_directory(case, |parent, name| OneDriveStep::MakeDirectory { item, parent, name })),
        ItemKind::File => find_decision_not_to_send(case, SentPart::NewFile)
            .unwrap_or_else(|| send_into_local_directory(case, |parent, name| OneDriveStep::SendNewFile { item, parent, name })),
    }
}

/// Removed here, is there in OneDrive.
///
/// - A file whose content OneDrive changed comes back, and where the folder cannot show it,
///   what was agreed on is forgotten, so that its row is that of an item of OneDrive the
///   folder cannot show.
/// - The removal of any other file is sent, guarded by the eTag, also when OneDrive moved
///   it.
/// - A directory's removal is unguarded and takes all OneDrive has in the directory: it is
///   sent only when all of that was shown here and removed here by the user, waits while
///   something of it is shown or held back or a listing is under way, and is otherwise of
///   the folder alone — what was agreed on of the directory is forgotten
///   ([`decide_directory_removal`]).
///
/// No removal is planned for an item given without the directories above it in OneDrive:
/// one of them may be removed here too, and then the removal is that one's.
fn decide_removed_here(case: &ItemCase<'_>, onedrive_content: ContentChange) -> ItemDecision {
    if let Some(directory) = case.onedrive_directory_not_given {
        return ItemDecision::Wait(WaitReason::ItemNotGiven(directory));
    }
    match (case.item.kind, onedrive_content) {
        (ItemKind::File, ContentChange::Changed | ContentChange::NotKnownWithoutHash) => match show_onedrive_item(case) {
            ItemDecision::Exclusion(ExclusionReason::CannotBeShown) => row_step(RowStep::ForgetSyncedVersion { item: case.versions() }),
            comes_back => comes_back,
        },
        (ItemKind::File, ContentChange::Same) => {
            find_decision_not_to_send(case, SentPart::Removal).unwrap_or_else(|| onedrive_step(OneDriveStep::SendFileRemoval { item: case.versions() }))
        }
        (ItemKind::Directory, _) => match decide_directory_removal(case.is_listing_under_way, &case.below_in_onedrive) {
            DirectoryRemovalOutcome::WaitsForListing => ItemDecision::Wait(WaitReason::Listing),
            DirectoryRemovalOutcome::WaitsFor(kept) => ItemDecision::Wait(WaitReason::AnotherItem(kept)),
            DirectoryRemovalOutcome::IsOfFolderAlone => row_step(RowStep::ForgetSyncedVersion { item: case.versions() }),
            DirectoryRemovalOutcome::IsSent { removed_with_it } => {
                find_decision_not_to_send(case, SentPart::Removal).unwrap_or_else(|| onedrive_step(OneDriveStep::SendDirectoryRemoval { item: case.versions(), below: removed_with_it }))
            }
        },
    }
}

/// An item of OneDrive the folder does not show appears here, where the folder can show
/// it: new in OneDrive, or coming back. An item the folder cannot show does not, and that
/// is said only once no listing is under way.
fn show_onedrive_item(case: &ItemCase<'_>) -> ItemDecision {
    match case.folder_visibility {
        FolderVisibility::NotKnownYet => wait_for_listing(case),
        FolderVisibility::CannotBeShown if case.is_listing_under_way => ItemDecision::Wait(WaitReason::Listing),
        FolderVisibility::CannotBeShown => ItemDecision::Exclusion(ExclusionReason::CannotBeShown),
        FolderVisibility::NotInOneDrive => row_fault(case),
        FolderVisibility::CanBeShown => match case.local_place_availability {
            LocalPlaceAvailability::Free => folder_step(FolderStep::ShowItem { item: case.versions() }),
            LocalPlaceAvailability::DirectoryNotShown(directory) | LocalPlaceAvailability::DirectoryRemovedHere(directory) => ItemDecision::Wait(WaitReason::AnotherItem(directory)),
            LocalPlaceAvailability::NameTaken(holder) => ItemDecision::Wait(WaitReason::AnotherItem(holder)),
            LocalPlaceAvailability::NameUnusable => ItemDecision::Exclusion(ExclusionReason::CannotBeShown),
            LocalPlaceAvailability::NoOneDrivePlace => row_fault(case),
        },
    }
}

/// Whether both sides have `item` with nothing agreed on of it, at two places: in two
/// directories, or under names that are two names also as OneDrive compares them. Its
/// binding was then a guess — the check at start bound an entry to the item its mark
/// names, and the entry is a copy of that file, or a hard link to it, made while no daemon
/// ran. Nobody moved anything: the place here is no move to send, and OneDrive's place is
/// none to move the entry to. (Two new items that were merged are in one directory under
/// one name as OneDrive compares names, and are not this.)
fn is_bound_by_guess(item: &Item) -> bool {
    let place = |version: &Option<crate::model::ItemVersion>| version.as_ref().and_then(|version| version.place.as_ref()).map(|place| (place.parent_local_item_id, crate::FoldedName::of(&place.name)));
    item.synced_place.is_none() && item.synced_content.is_none() && item.local.is_some() && item.onedrive.is_some() && place(&item.local) != place(&item.onedrive)
}

/// Is there on both sides. An item of which nothing is agreed on, at two places, gives up
/// its binding, once no listing is under way ([`is_bound_by_guess`]): the entry here is a
/// new item of the folder, and OneDrive's item is shown at its own place by the plans
/// that follow; but a directory in which the folder shows what was agreed to be in it,
/// whose binding is then no guess. Otherwise: its place, then for a file its content, and last the download
/// of a pinned file whose content is not on disk. What the row says the upload rules say of
/// its name ([`Item::is_name_ignored`]) is not asked: an item OneDrive has is never left
/// here, and no removal is sent because a name is ignored.
fn decide_item_on_both_sides(case: &ItemCase<'_>, here: (PlaceChange, ContentChange), in_onedrive: (PlaceChange, ContentChange)) -> ItemDecision {
    if matches!(case.folder_visibility, FolderVisibility::NotKnownYet) {
        return wait_for_listing(case);
    }
    if matches!(case.folder_visibility, FolderVisibility::NotInOneDrive) {
        return row_fault(case);
    }
    if is_bound_by_guess(case.item) && !case.holds_what_was_agreed_on_in_it {
        return if case.is_listing_under_way { ItemDecision::Wait(WaitReason::Listing) } else { row_step(RowStep::GiveUpBinding { item: case.versions() }) };
    }
    let place = decide_place(case, here.0, in_onedrive.0, (here.1, in_onedrive.1));
    if place != ItemDecision::NothingToDo || case.item.kind == ItemKind::Directory {
        return place;
    }
    let content = decide_content(case, here.1, in_onedrive.1);
    if content == ItemDecision::NothingToDo && case.is_pinned && case.item.content_presence == ContentPresence::NotOnDisk {
        return folder_step(FolderStep::DownloadContent { item: case.versions() });
    }
    content
}

/// The step that sends the item's place here to OneDrive.
fn send_move(case: &ItemCase<'_>) -> ItemDecision {
    let item = case.versions();
    find_decision_not_to_send(case, SentPart::Place).unwrap_or_else(|| send_into_local_directory(case, |parent, name| OneDriveStep::SendMove { item, parent, name }))
}

/// OneDrive has the item where the folder cannot show it, and the item was not moved here:
/// it goes here as if removed, unless its content was changed here, which is then sent
/// into it where it is, the item staying here. Nothing is done while a listing is under way.
fn decide_item_that_cannot_be_shown(case: &ItemCase<'_>, here_content: ContentChange, onedrive_content: ContentChange) -> ItemDecision {
    if case.is_listing_under_way {
        return ItemDecision::Wait(WaitReason::Listing);
    }
    match (case.item.kind, here_content) {
        (ItemKind::File, ContentChange::Changed | ContentChange::NotKnownWithoutHash) => decide_content(case, here_content, onedrive_content),
        _ => remove_item_here(case),
    }
}

/// The table "its place", for an item both sides have.
///
/// - Same here, moved in OneDrive: it is moved here; moved to where the folder cannot show
///   it, [`decide_item_that_cannot_be_shown`]; moved into a directory that was removed
///   here, its place here stands and is sent, so that it is not removed with that
///   directory.
/// - Moved here, same in OneDrive: the move is sent.
/// - Moved on both sides to one place: agree. To different places: OneDrive's stands and
///   the folder follows, where it can; where it cannot (the name is taken here, the name
///   cannot be held, the folder cannot show the item there, OneDrive's directory was
///   removed here), the local place stands and is sent.
/// - A directory the folder cannot show, moved here: nothing of its place is sent. It is in
///   the folder only because the folder shows the user's own in it, and goes once it shows
///   nothing in it, as one that was not moved.
///
/// What was done to the content on each side is asked only where the item cannot be shown.
pub(crate) fn decide_place(case: &ItemCase<'_>, here: PlaceChange, in_onedrive: PlaceChange, (here_content, onedrive_content): (ContentChange, ContentChange)) -> ItemDecision {
    let can_be_shown = matches!(case.folder_visibility, FolderVisibility::CanBeShown);
    let item = case.item;
    match (here, in_onedrive) {
        (PlaceChange::Same, _) if !can_be_shown => decide_item_that_cannot_be_shown(case, here_content, onedrive_content),
        (PlaceChange::Same, PlaceChange::Same) => ItemDecision::NothingToDo,
        (PlaceChange::Same, PlaceChange::Moved) => match case.local_place_availability {
            LocalPlaceAvailability::Free => folder_step(FolderStep::MoveItem { item: case.versions() }),
            LocalPlaceAvailability::DirectoryNotShown(directory) => ItemDecision::Wait(WaitReason::AnotherItem(directory)),
            LocalPlaceAvailability::DirectoryRemovedHere(_) => send_move(case),
            LocalPlaceAvailability::NameTaken(holder) => ItemDecision::Wait(WaitReason::AnotherItem(holder)),
            LocalPlaceAvailability::NameUnusable => decide_item_that_cannot_be_shown(case, here_content, onedrive_content),
            LocalPlaceAvailability::NoOneDrivePlace => row_fault(case),
        },
        (PlaceChange::Moved, PlaceChange::Same) if !can_be_shown && item.kind == ItemKind::Directory => decide_item_that_cannot_be_shown(case, here_content, onedrive_content),
        (PlaceChange::Moved, PlaceChange::Same) => send_move(case),
        (PlaceChange::Moved, PlaceChange::Moved) => {
            let is_one_place = item.local.as_ref().map(|local| &local.place) == item.onedrive.as_ref().map(|onedrive| &onedrive.place);
            match case.local_place_availability {
                _ if is_one_place => row_step(RowStep::AgreeOnPlace { item: case.versions() }),
                _ if !can_be_shown && case.is_listing_under_way => ItemDecision::Wait(WaitReason::Listing),
                _ if !can_be_shown && item.kind == ItemKind::Directory => remove_item_here(case),
                _ if !can_be_shown => send_move(case),
                LocalPlaceAvailability::Free => folder_step(FolderStep::MoveItem { item: case.versions() }),
                LocalPlaceAvailability::DirectoryNotShown(directory) => ItemDecision::Wait(WaitReason::AnotherItem(directory)),
                LocalPlaceAvailability::NameTaken(_) | LocalPlaceAvailability::NameUnusable | LocalPlaceAvailability::DirectoryRemovedHere(_) => send_move(case),
                LocalPlaceAvailability::NoOneDrivePlace => row_fault(case),
            }
        }
    }
}

/// The table "its content", for a file both sides have.
///
/// - Same on both sides: nothing, unless the content versions are not one yet though the
///   bytes are (a hash computed here, a cTag or a time OneDrive changed): agree.
/// - Same here, changed in OneDrive: the file takes OneDrive's content, and the copy on
///   disk goes.
/// - Changed here, same in OneDrive: the content is sent.
/// - Changed on both sides: the same hash, agree; another, both are kept, the local one
///   giving up the binding; not known without a hash, the local content is hashed.
pub(crate) fn decide_content(case: &ItemCase<'_>, here: ContentChange, in_onedrive: ContentChange) -> ItemDecision {
    let item = case.item;
    let local = item.local.as_ref().and_then(|local| local.content.as_ref());
    let onedrive = item.onedrive.as_ref().and_then(|onedrive| onedrive.content.as_ref());
    match (here, in_onedrive) {
        (ContentChange::Same, ContentChange::Same) if local == onedrive && local == item.synced_content.as_ref() => ItemDecision::NothingToDo,
        (ContentChange::Same, ContentChange::Same) => row_step(RowStep::AgreeOnContent { item: case.versions() }),
        (ContentChange::Same, ContentChange::Changed | ContentChange::NotKnownWithoutHash) => folder_step(FolderStep::TakeOneDriveContent { item: case.versions() }),
        (ContentChange::Changed | ContentChange::NotKnownWithoutHash, ContentChange::Same) => {
            find_decision_not_to_send(case, SentPart::Content).unwrap_or_else(|| onedrive_step(OneDriveStep::SendContent { item: case.versions() }))
        }
        (ContentChange::Changed | ContentChange::NotKnownWithoutHash, ContentChange::Changed | ContentChange::NotKnownWithoutHash) => match compare_local_content(local, onedrive) {
            ContentChange::Same => row_step(RowStep::AgreeOnContent { item: case.versions() }),
            ContentChange::Changed => row_step(RowStep::GiveUpBinding { item: case.versions() }),
            ContentChange::NotKnownWithoutHash => row_step(RowStep::HashLocalContent { item: case.versions() }),
        },
    }
}
