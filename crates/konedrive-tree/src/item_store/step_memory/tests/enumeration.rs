//! Every fact a step remembers, against every operation of the store that can change what
//! the fact was learnt for: one cell each, run in a store of its own. What a cell is to be is
//! computed from the rules ([`decide_fate_by_rule`]), a function of the fact, of the item's
//! row when the fact was learnt and after the operation, and of what the rows do not say:
//! what happened to a binding, and what a commit was told ([`Happening`]). The test asserts
//! that the store does what the rule says, cell by cell. There is no table of expected
//! cells.
//!
//! The item of every cell is `report` in the directory `docs`, which is in the directory
//! `above`, both directories of OneDrive the folder shows. It is a file or a directory, in
//! one of five shapes ([`Shape`]). The fact is written by its own operation, with the stamp
//! of the row it is learnt under; a fact of an unsent change is not learnt of a row with
//! nothing unsent, and such a cell is no cell. Then the operation is done, and then the
//! first write of a new try for the item, which is where what the row does not say any more
//! is taken away.

use std::ffi::OsStr;

use konedrive_reason::Reason;

use super::*;
use crate::item_store::tests::listed_drive::{listed_directory, listed_file, listed_gone, reference, store_with_listed_root, write_listing_of_changes};
use crate::item_store::{ConfirmedItem, StepCommitOutcome};
use crate::model::{Change, Item, ItemKind, OneDriveItemId, OneDriveItemReference};
use crate::sync_plan::ItemVersions;

/// What the item of a cell is when the fact is learnt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Made here; OneDrive does not have it.
    New,
    /// An item of OneDrive the folder shows, the two sides agreeing on it: nothing unsent.
    Agreeing,
    /// A file of OneDrive whose content was changed here.
    Edited,
    /// An item of OneDrive renamed here, to `moved`.
    Moved,
    /// An item of OneDrive removed here, its removal not sent.
    Removed,
}

use Shape::{Agreeing, Edited, New, Removed};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fact {
    UploadSession,
    Opening,
    MismatchedItem,
    TemporaryName,
    FailedTries,
    SpaceRefusal,
    Refusal,
    TakenName,
    RemovalConfirmation,
    RemovalRefusal,
}

const FACTS: [Fact; 10] = [
    Fact::UploadSession,
    Fact::Opening,
    Fact::MismatchedItem,
    Fact::TemporaryName,
    Fact::FailedTries,
    Fact::SpaceRefusal,
    Fact::Refusal,
    Fact::TakenName,
    Fact::RemovalConfirmation,
    Fact::RemovalRefusal,
];

/// What becomes of a fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    /// Still answered for the item, and after the next write for it.
    Answered,
    /// Not answered, and nothing of it is kept after the next write for the item, or with
    /// its row.
    Gone,
    /// Kept for the row that has the binding now, or for the directory that has it.
    Moved,
    /// Not answered, and what is left to do in OneDrive is queued: the cancel of an upload
    /// session, the removal of the item an upload left.
    Queued,
    /// Not answered, and still in the item's row after the next write for it. Never right.
    KeptUnanswered,
}

/// What the rows before and after do not say of an operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Happening {
    /// An operation on the item's own binding was done: it was given one, took one over,
    /// was merged, or gave its own up. Seen by watching the row after every step of the
    /// operation, so that a binding taken and given up again is seen too.
    is_item_binding_changed: bool,
    /// The directory the item was in when the fact was learnt gave its binding up.
    is_directory_binding_changed: bool,
    /// A commit wrote that OneDrive confirmed a request of the item.
    is_request_confirmed: bool,
    /// A commit was told that the item's upload session ended.
    is_session_ended: bool,
    /// A commit wrote that OneDrive moved the item to this place.
    confirmed_move: Option<ItemPlace>,
    /// A commit wrote that OneDrive removed the item.
    is_removal_confirmed: bool,
    /// A commit wrote the hash of the same bytes.
    is_hash_computed: bool,
}

/// What the store says, beside the item's own row, when the cell is over.
#[derive(Debug, Clone, Default)]
struct RowsAfter {
    /// The item's row; none when it went.
    row: Option<Item>,
    /// The row that has the binding the item had when the fact was learnt, if any has.
    holder_of_learnt_binding: Option<LocalItemId>,
    /// A row is bound to the item an upload left, and the folder shows it or the two sides
    /// agreed on a place of it.
    is_mismatched_item_of_the_folder: bool,
}

/// The rules, as one function: what becomes of `fact`, learnt of an item whose row was
/// `learnt` then, when the row is `after.row` now and `happening` is what the rows do not
/// say.
///
/// 1. A fact of a local change that is not sent is answered while the row has the stamp it
///    was learnt under, and no operation was done on the item's binding. Beside the stamp:
///    an upload session and a refusal for lack of space while the bytes are the file's; a
///    refusal of a shown version while the item is at that place with those bytes; a taken
///    name while the item is at that place; and what OneDrive answered about sending into a
///    directory, while that directory has the binding it had. The failed tries end with a
///    request OneDrive confirmed. An upload session that is not answered is queued to be
///    cancelled, unless a commit was told it ended.
/// 2. A fact of what OneDrive has: the temporary name is with the row that has the binding,
///    until OneDrive confirmed the item at another place than the temporary name, or
///    removed; the item an upload left is void once a row the folder has is bound to it,
///    and is queued for removal when its file's row goes; an opening is of its place, and
///    with the row that has the directory's binding.
fn decide_fate_by_rule(fact: Fact, kind: ItemKind, learnt: &Item, after: &RowsAfter, happening: &Happening) -> Fate {
    let now = after.row.as_ref();
    match fact {
        Fact::Opening => return if happening.is_directory_binding_changed { Fate::Moved } else { Fate::Answered },
        Fact::MismatchedItem => {
            return match (after.is_mismatched_item_of_the_folder, now) {
                (true, _) => Fate::Gone,
                (false, None) => Fate::Queued,
                (false, Some(_)) => Fate::Answered,
            }
        }
        Fact::TemporaryName => {
            let temporary = temporary_toward_place(learnt.local.as_ref().and_then(|local| local.place.clone()).expect("learnt of a shown item"), learnt.local_item_id);
            let temporary_place = ItemPlace { parent_local_item_id: temporary.toward.parent_local_item_id, name: temporary.name };
            return match (learnt.onedrive_item.is_some(), after.holder_of_learnt_binding) {
                // It was of no item of OneDrive, and the row has another binding now.
                (false, _) if happening.is_item_binding_changed => Fate::Gone,
                (true, Some(holder)) if holder != learnt.local_item_id => Fate::Moved,
                (true, None) => Fate::Gone,
                _ if now.is_none() => Fate::Gone,
                _ if happening.is_removal_confirmed => Fate::Gone,
                _ if happening.confirmed_move.as_ref().is_some_and(|to| *to != temporary_place) => Fate::Gone,
                _ => Fate::Answered,
            };
        }
        _ => {}
    }
    // The facts of a local change that is not sent.
    let lost = if fact == Fact::UploadSession && !happening.is_session_ended { Fate::Queued } else { Fate::Gone };
    let Some(now) = now else { return lost };
    if happening.is_item_binding_changed || now.unsent_since.is_none() || now.unsent_since != learnt.unsent_since {
        return lost;
    }
    let (learnt_local, now_local) = (learnt.local.as_ref(), now.local.as_ref());
    let is_at_same_place = now_local.map(|local| &local.place) == learnt_local.map(|local| &local.place);
    let learnt_content = learnt_local.and_then(|local| local.content.as_ref());
    let has_same_bytes = happening.is_hash_computed
        || learnt_content.is_some_and(|content| now_local.is_some_and(|local| local.content.as_ref() == Some(content)) || (now_local.is_some() && now.content_before_write.as_ref() == Some(content)));
    let is_in_same_directory_of_onedrive = !happening.is_directory_binding_changed;
    let is_answered = match fact {
        Fact::UploadSession => !happening.is_session_ended && has_same_bytes && (learnt.onedrive_item.is_some() || is_in_same_directory_of_onedrive),
        Fact::SpaceRefusal => has_same_bytes,
        Fact::Refusal => is_at_same_place && (kind == ItemKind::Directory || has_same_bytes) && is_in_same_directory_of_onedrive,
        Fact::TakenName => is_at_same_place && is_in_same_directory_of_onedrive,
        Fact::FailedTries => !happening.is_request_confirmed,
        Fact::RemovalConfirmation | Fact::RemovalRefusal => true,
        Fact::Opening | Fact::MismatchedItem | Fact::TemporaryName => unreachable!("decided above"),
    };
    if is_answered {
        Fate::Answered
    } else {
        lost
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    // The local version changes.
    Renamed,
    RenamedAndRenamedBack,
    MovedBackToSyncedPlace,
    MovedToAnotherDirectory,
    SavedOverABoundFile,
    SavedOverANewFile,
    ANewFileSavedOverIt,
    ABoundFileSavedOverIt,
    RemovedHere,
    RemovedAndMadeAgain,
    OpenedForWriting,
    ClosedChanged,
    ClosedUnchanged,
    Truncated,
    ExchangedPlaces,
    // The operations on bindings.
    Merged,
    BoundFirst,
    BoundAndMerged,
    GaveUpBinding,
    DirectoryGaveUpBinding,
    DirectoryAboveGaveUpBinding,
    MergedAndGaveUpBinding,
    MergedWithTheMismatchedItemAndSavedOver,
    // The commits of the steps towards OneDrive.
    CommitSendNewItem,
    CommitSendNewItemAndMerged,
    CommitSendNewFileThatIsTheMismatchedItem,
    CommitSendNewFileChangedMeanwhile,
    CommitSendNewFileUnderItsSessionsName,
    CommitSendContent,
    CommitSendContentChangedMeanwhile,
    CommitSendContentToldNoSession,
    CommitSendMove,
    CommitSendMoveChangedMeanwhile,
    CommitSendMoveToTemporaryName,
    CommitSendMoveAfterAnotherRename,
    // The commits of the steps towards the folder and of the rows.
    CommitMoveItem,
    CommitTakeMoveBack,
    CommitTakeOneDriveContent,
    CommitAgreeOnContent,
    CommitRemoveItem,
    CommitDownloadContent,
    CommitRenameToConflictCopy,
    CommitMoveToTemporaryName,
    CommitGiveUpBinding,
    CommitMergeItems,
    CommitHashLocalContent,
    CommitAgreeOnPlace,
    // The rest.
    CeasedToBeShown,
    RowDeleted,
    // A listing changes the OneDrive version, and no step is committed.
    ListedAtAnotherPlace,
    ListedWithAnotherContent,
    ListedGone,
    ListedAnotherItemAtItsName,
    ListedTheMismatchedItem,
    ListedTheMismatchedItemAndShownIt,
    ShownTheMismatchedItemAndRemovedHere,
    RemovedHereAndShownTheMismatchedItem,
    // Of an item removed here.
    ShownAgain,
    ShownAgainAndRemovedAgain,
    MadeAgain,
    MadeAgainAndRemovedAgain,
    ShownAgainAndGaveUpBinding,
    ShownAgainAndCeasedToBeShown,
    CommitSendRemoval,
    CommitSendRemovalOfTheDirectory,
    CommitForgetSyncedVersion,
    CommitAgreeOnRemoval,
}

use Operation::*;

const OPERATIONS: [Operation; 67] = [
    Renamed,
    RenamedAndRenamedBack,
    MovedBackToSyncedPlace,
    MovedToAnotherDirectory,
    SavedOverABoundFile,
    SavedOverANewFile,
    ANewFileSavedOverIt,
    ABoundFileSavedOverIt,
    RemovedHere,
    RemovedAndMadeAgain,
    OpenedForWriting,
    ClosedChanged,
    ClosedUnchanged,
    Truncated,
    ExchangedPlaces,
    Merged,
    BoundFirst,
    BoundAndMerged,
    GaveUpBinding,
    DirectoryGaveUpBinding,
    DirectoryAboveGaveUpBinding,
    MergedAndGaveUpBinding,
    MergedWithTheMismatchedItemAndSavedOver,
    CommitSendNewItem,
    CommitSendNewItemAndMerged,
    CommitSendNewFileThatIsTheMismatchedItem,
    CommitSendNewFileChangedMeanwhile,
    CommitSendNewFileUnderItsSessionsName,
    CommitSendContent,
    CommitSendContentChangedMeanwhile,
    CommitSendContentToldNoSession,
    CommitSendMove,
    CommitSendMoveChangedMeanwhile,
    CommitSendMoveToTemporaryName,
    CommitSendMoveAfterAnotherRename,
    CommitMoveItem,
    CommitTakeMoveBack,
    CommitTakeOneDriveContent,
    CommitAgreeOnContent,
    CommitRemoveItem,
    CommitDownloadContent,
    CommitRenameToConflictCopy,
    CommitMoveToTemporaryName,
    CommitGiveUpBinding,
    CommitMergeItems,
    CommitHashLocalContent,
    CommitAgreeOnPlace,
    CeasedToBeShown,
    RowDeleted,
    ListedAtAnotherPlace,
    ListedWithAnotherContent,
    ListedGone,
    ListedAnotherItemAtItsName,
    ListedTheMismatchedItem,
    ListedTheMismatchedItemAndShownIt,
    ShownTheMismatchedItemAndRemovedHere,
    RemovedHereAndShownTheMismatchedItem,
    ShownAgain,
    ShownAgainAndRemovedAgain,
    MadeAgain,
    MadeAgainAndRemovedAgain,
    ShownAgainAndGaveUpBinding,
    ShownAgainAndCeasedToBeShown,
    CommitSendRemoval,
    CommitSendRemovalOfTheDirectory,
    CommitForgetSyncedVersion,
    CommitAgreeOnRemoval,
];

const SHOWN: &[Shape] = &[New, Agreeing, Edited, Shape::Moved];
const BOUND_AND_SHOWN: &[Shape] = &[Agreeing, Edited, Shape::Moved];
const BOUND: &[Shape] = &[Agreeing, Edited, Shape::Moved, Removed];
const EVERY_SHAPE: &[Shape] = &[New, Agreeing, Edited, Shape::Moved, Removed];

/// The shapes of a file, and of a directory, the store does the operation for. A directory
/// has no content, so it is never [`Edited`], and none of the operations that are of a
/// content or of a file saved over another are done for it.
fn shapes_for(operation: Operation, kind: ItemKind) -> &'static [Shape] {
    let is_file = kind == ItemKind::File;
    match operation {
        Renamed | RenamedAndRenamedBack | MovedToAnotherDirectory | RemovedHere | CommitRenameToConflictCopy => SHOWN,
        MovedBackToSyncedPlace | CommitSendMove | CommitSendMoveToTemporaryName | CommitSendMoveAfterAnotherRename | CommitTakeMoveBack | CommitAgreeOnPlace => &[Shape::Moved],
        SavedOverABoundFile | SavedOverANewFile | ANewFileSavedOverIt | ABoundFileSavedOverIt | OpenedForWriting | ClosedChanged | ClosedUnchanged | Truncated | ExchangedPlaces
        | CommitDownloadContent | CommitHashLocalContent
            if is_file =>
        {
            SHOWN
        }
        RemovedAndMadeAgain | CommitTakeOneDriveContent | CommitAgreeOnContent if is_file => BOUND_AND_SHOWN,
        Merged | BoundFirst | BoundAndMerged | MergedAndGaveUpBinding | CommitSendNewItem | CommitSendNewItemAndMerged | CommitMergeItems | ListedAnotherItemAtItsName => &[New],
        MergedWithTheMismatchedItemAndSavedOver
        | CommitSendNewFileThatIsTheMismatchedItem
        | CommitSendNewFileChangedMeanwhile
        | CommitSendNewFileUnderItsSessionsName
        | ListedTheMismatchedItem
        | ListedTheMismatchedItemAndShownIt
        | ShownTheMismatchedItemAndRemovedHere
        | RemovedHereAndShownTheMismatchedItem
            if is_file =>
        {
            &[New]
        }
        GaveUpBinding | CommitGiveUpBinding | CommitRemoveItem | CommitMoveToTemporaryName | CeasedToBeShown => BOUND_AND_SHOWN,
        DirectoryGaveUpBinding | DirectoryAboveGaveUpBinding | RowDeleted => EVERY_SHAPE,
        CommitSendContent | CommitSendContentChangedMeanwhile | CommitSendContentToldNoSession if is_file => &[Edited],
        CommitSendMoveChangedMeanwhile if is_file => &[Shape::Moved],
        CommitMoveItem => &[Agreeing, Edited],
        ListedAtAnotherPlace | ListedGone => BOUND,
        ListedWithAnotherContent if is_file => BOUND,
        ShownAgain | ShownAgainAndRemovedAgain | ShownAgainAndGaveUpBinding | ShownAgainAndCeasedToBeShown | CommitSendRemoval | CommitSendRemovalOfTheDirectory | CommitForgetSyncedVersion
        | CommitAgreeOnRemoval => &[Removed],
        MadeAgain | MadeAgainAndRemovedAgain if is_file => &[Removed],
        _ => &[],
    }
}

/// The store of a cell, the items in it, and what was seen of it.
struct Scene {
    store: TreeStore,
    kind: ItemKind,
    above: LocalItemId,
    directory: LocalItemId,
    /// The item the fact is of.
    item: LocalItemId,
    /// Another item the operation needs, when it needs one.
    other: Option<LocalItemId>,
    /// The item's row when the fact was learnt.
    learnt: Option<Item>,
    /// The bindings of the item and of its directory as last seen.
    seen_bindings: (Option<OneDriveItemReference>, Option<OneDriveItemReference>),
    happening: Happening,
}

fn versions(store: &mut TreeStore, id: LocalItemId) -> ItemVersions {
    ItemVersions::of(&item(store, id))
}

fn is_written(outcome: Result<StepCommitOutcome, crate::TreeError>) {
    assert_eq!(outcome.unwrap(), StepCommitOutcome::Written);
}

fn bound_row(store: &mut TreeStore, item_id: &str) -> LocalItemId {
    store.item_store().find_bound_item(&reference(item_id)).unwrap().expect("listed").local_item_id
}

/// The folder shows the item of OneDrive `item_id`, which a listing brought.
fn show(store: &mut TreeStore, item_id: &str) -> LocalItemId {
    let id = bound_row(store, item_id);
    let planned_from = versions(store, id);
    is_written(store.item_store().commit_show_item(&planned_from, false));
    id
}

fn listed_content(size: u64, hash: &str) -> ContentVersion {
    ContentVersion::Hashed { size, mtime: 1000, mtime_nsec: 0, hash: Some(hash.to_owned()), ctag: Some(format!("c-{hash}")) }
}

/// An item of the cell's kind as a listing gives it: a file with the hash `h-<id>`.
fn listed(kind: ItemKind, id: &str, name: &str, etag: &str) -> Change {
    match kind {
        ItemKind::File => listed_file(id, "D", name, (3, &format!("h-{id}")), etag),
        ItemKind::Directory => listed_directory(id, "D", name, etag),
    }
}

fn confirmed(kind: ItemKind, item_id: &str, name: &str, content: ContentVersion) -> ConfirmedItem {
    ConfirmedItem { item: reference(item_id), parent: reference("D"), name: name.into(), content: (kind == ItemKind::File).then_some(content), etag: format!("e-{item_id}-confirmed") }
}

fn apply(store: &mut TreeStore, change: LocalVersionChange) -> LocalItemId {
    store.item_store().apply(&change).unwrap()
}

fn move_to(store: &mut TreeStore, id: LocalItemId, to: ItemPlace) {
    apply(store, LocalVersionChange::Moved { item: id, place: to, is_name_ignored: false });
}

fn make(store: &mut TreeStore, kind: ItemKind, at: ItemPlace) -> LocalItemId {
    match kind {
        ItemKind::File => make_file(store, at.parent_local_item_id, at.name.to_str().unwrap()),
        ItemKind::Directory => make_directory(store, at.parent_local_item_id, at.name.to_str().unwrap()),
    }
}

/// The temporary name the cells give an item on its way to `toward`.
fn temporary_toward_place(toward: ItemPlace, id: LocalItemId) -> TemporaryName {
    TemporaryName { name: format!(".konedrive-swap-{id}").into(), toward }
}

const SESSION: &str = "of-the-cell";
const MISMATCHED: &str = "BAD";

fn scene(kind: ItemKind, shape: Shape) -> Scene {
    let mut store = store_with_listed_root();
    write_listing_of_changes(&mut store, &[listed_directory("G", "R", "above", "e-G"), listed_directory("D", "G", "docs", "e-D")]);
    let (above, directory) = (show(&mut store, "G"), show(&mut store, "D"));
    let item = match shape {
        New => make(&mut store, kind, place(directory, "report")),
        _ => {
            write_listing_of_changes(&mut store, &[listed(kind, "X", "report", "e-X")]);
            show(&mut store, "X")
        }
    };
    match shape {
        Edited => change_content(&mut store, item),
        Shape::Moved => move_to(&mut store, item, place(directory, "moved")),
        Removed => remove(&mut store, item),
        New | Agreeing => {}
    }
    Scene { store, kind, above, directory, item, other: None, learnt: None, seen_bindings: (None, None), happening: Happening::default() }
}

impl Scene {
    fn row(&mut self) -> Option<Item> {
        self.store.item_store().find_item(self.item).unwrap()
    }

    fn bindings(&mut self) -> (Option<OneDriveItemReference>, Option<OneDriveItemReference>) {
        let directory = self.store.item_store().find_item(self.directory).unwrap().and_then(|row| row.onedrive_item);
        (self.row().and_then(|row| row.onedrive_item), directory)
    }

    /// One step of the operation: `work` is done, and what became of the two bindings is
    /// seen.
    fn step<T>(&mut self, work: impl FnOnce(&mut TreeStore) -> T) -> T {
        let done = work(&mut self.store);
        let now = self.bindings();
        self.happening.is_item_binding_changed |= now.0 != self.seen_bindings.0;
        self.happening.is_directory_binding_changed |= now.1 != self.seen_bindings.1;
        self.seen_bindings = now;
        done
    }

    /// The fact is written of the item by its own operation, under the stamp its row has,
    /// and is answered. `None`, with nothing written, where it is no fact of such an item:
    /// a fact of an unsent change of a row with nothing unsent, a fact of a content of a
    /// directory, a fact of a removal of an item the folder shows and the other way.
    fn learn(&mut self, fact: Fact) -> Option<()> {
        let (id, is_file) = (self.item, self.kind == ItemKind::File);
        let row = self.row().expect("the item has a row when a fact is learnt of it");
        let (is_shown, is_bound, unsent_since) = (row.local.is_some(), row.onedrive_item.is_some(), row.unsent_since);
        let at = row.local.as_ref().and_then(|local| local.place.clone());
        let store = &mut self.store;
        match fact {
            Fact::UploadSession => {
                let _ = (is_file && is_shown).then_some(())?;
                let session = UploadSession { address: address(SESSION), expires_at: None, next_offset: 0, content: local_content(store, id), empty_file_place: (!is_bound).then_some(at?) };
                assert_eq!(remember_session(store, id, unsent_since?, &session), Continued);
            }
            Fact::Opening => {
                let _ = (is_file && is_shown).then_some(())?;
                store.step_memory().remember_upload_session_opening(id, &at?, 10).unwrap();
            }
            Fact::MismatchedItem => {
                let _ = is_file.then_some(())?;
                store.step_memory().remember_mismatched_item(id, &mismatched_item()).unwrap();
            }
            Fact::TemporaryName => {
                store.step_memory().remember_temporary_name(id, &temporary_toward_place(at?, id)).unwrap();
            }
            Fact::FailedTries => assert_eq!(remember_try(store, id, unsent_since?), Some(1)),
            Fact::SpaceRefusal => {
                let _ = (is_file && is_shown).then_some(())?;
                let refused = space_refusal(store, id, 1);
                assert_eq!(remember_space(store, id, unsent_since?, &refused), StillSaid);
            }
            Fact::Refusal => {
                let _ = is_shown.then_some(())?;
                let refusal = refusal_of(store, id, Reason::Forbidden);
                assert_eq!(remember_refused(store, id, unsent_since?, &refusal), StillSaid);
            }
            Fact::TakenName => {
                let _ = is_shown.then_some(())?;
                let taken = taken_at(store, id, "REPORT");
                assert_eq!(remember_taken(store, id, unsent_since?, &taken), StillSaid);
            }
            Fact::RemovalConfirmation => {
                let removal = UnsentRemoval::of(&row)?;
                assert_eq!(store.step_memory().remember_removals_confirmed(&[removal]).unwrap(), 1);
            }
            Fact::RemovalRefusal => {
                let removal = UnsentRemoval::of(&row)?;
                assert_eq!(remember_refused(store, id, unsent_since, &removal_refusal(removal)), StillSaid);
            }
        }
        self.learnt = Some(row);
        self.seen_bindings = self.bindings();
        self.happening = Happening::default();
        Some(())
    }
}

// What follows is how the enumeration gives a fact to the store.

fn mismatched_item() -> MismatchedItem {
    MismatchedItem::of_answer(reference(MISMATCHED), Some("c-h-BAD"), None)
}

fn remember_session(store: &mut TreeStore, id: LocalItemId, unsent_since: u64, session: &UploadSession) -> UploadSessionStanding {
    store.step_memory().remember_upload_session(id, Some(unsent_since), session).unwrap()
}

fn remember_try(store: &mut TreeStore, id: LocalItemId, unsent_since: u64) -> Option<u32> {
    store.step_memory().remember_failed_try(id, Some(unsent_since), &Reason::Network, Some(60)).unwrap()
}

fn remember_space(store: &mut TreeStore, id: LocalItemId, unsent_since: u64, refused: &SpaceRefusal) -> FactStanding {
    store.step_memory().remember_space_refusal(id, Some(unsent_since), refused).unwrap()
}

fn remember_refused(store: &mut TreeStore, id: LocalItemId, unsent_since: impl Into<Option<u64>>, refusal: &Refusal) -> FactStanding {
    store.step_memory().remember_refusal(id, unsent_since.into(), refusal).unwrap()
}

fn remember_taken(store: &mut TreeStore, id: LocalItemId, unsent_since: u64, taken: &TakenName) -> FactStanding {
    store.step_memory().remember_taken_name(id, Some(unsent_since), taken).unwrap()
}

fn removal_refusal(_removal: UnsentRemoval) -> Refusal {
    Refusal { reason: Reason::Forbidden, version: RefusedVersion::Removal }
}

fn is_removal_refusal(refusal: &Refusal) -> bool {
    refusal.version == RefusedVersion::Removal
}

/// The ids of the items uploads left that are queued for removal.
fn list_queued_mismatched_items(store: &mut TreeStore) -> Vec<OneDriveItemId> {
    store.step_memory().list_mismatched_item_removals(100).unwrap().into_iter().map(|left| left.item.item_id).collect()
}

/// The commit of the step that sent a new file or the content of a file, told the session
/// that ended with it, when one did.
fn commit_upload(store: &mut TreeStore, planned_from: &ItemVersions, kind: ItemKind, name: &str, sent: &ConfirmedItem, ended_session: Option<&UploadSessionAddress>) {
    let is_new = planned_from.onedrive_item.is_none();
    is_written(match (is_new, kind) {
        (true, ItemKind::File) => store.item_store().commit_send_new_file(planned_from, &reference("D"), OsStr::new(name), sent, ended_session),
        (true, ItemKind::Directory) => store.item_store().commit_make_directory(planned_from, &reference("D"), OsStr::new(name), sent),
        (false, _) => store.item_store().commit_send_content(planned_from, sent, ended_session),
    });
}

/// What the operation needs of the rows before the fact is learnt.
fn prepare(scene: &mut Scene, operation: Operation) {
    let (id, directory, kind) = (scene.item, scene.directory, scene.kind);
    let store = &mut scene.store;
    match operation {
        SavedOverABoundFile | ABoundFileSavedOverIt => {
            write_listing_of_changes(store, &[listed_file("O", "D", "other", (3, "h-O"), "e-O")]);
            scene.other = Some(show(store, "O"));
        }
        SavedOverANewFile | ANewFileSavedOverIt | ExchangedPlaces | MergedWithTheMismatchedItemAndSavedOver => scene.other = Some(make_file(store, directory, "other")),
        Merged | BoundAndMerged | CommitMergeItems | CommitSendNewItemAndMerged | MergedAndGaveUpBinding | ListedAnotherItemAtItsName => {
            write_listing_of_changes(store, &[listed(kind, "N", "report", "e-N")]);
        }
        CommitMoveItem => write_listing_of_changes(store, &[listed(kind, "X", "elsewhere", "e-X2")]),
        CommitTakeOneDriveContent | CommitAgreeOnContent => write_listing_of_changes(store, &[listed_file("X", "D", "report", (9, "h-X2"), "e-X2")]),
        CommitRemoveItem | CommitAgreeOnRemoval => write_listing_of_changes(store, &[listed_gone("X")]),
        CommitAgreeOnPlace => write_listing_of_changes(store, &[listed(kind, "X", "moved", "e-X2")]),
        CommitSendRemovalOfTheDirectory => remove(store, directory),
        _ => {}
    }
    let _ = id;
}

/// The operation of the cell, after the fact was learnt.
fn act(scene: &mut Scene, operation: Operation) {
    let (id, directory, above, kind, other) = (scene.item, scene.directory, scene.above, scene.kind, scene.other);
    let other = move || other.expect("prepared");
    let learnt = scene.learnt.clone().expect("learnt");
    let at = learnt.local.as_ref().and_then(|local| local.place.clone());
    let at = move || at.clone().expect("the folder showed the item when the fact was learnt");
    let planned_from = ItemVersions::of(&learnt);
    let show_again = |store: &mut TreeStore| {
        let planned_from = versions(store, id);
        is_written(store.item_store().commit_show_item(&planned_from, false));
    };
    let make_again = |store: &mut TreeStore| {
        let content = Some(ContentVersion::ChangedHere { size: 4, mtime: 400, mtime_nsec: 0, change_count: 1 });
        apply(store, LocalVersionChange::Made { kind: ItemKind::File, place: place(directory, "report"), content, is_name_ignored: false, replaced: Some(id) })
    };
    let made_content = ContentVersion::Hashed { size: 0, mtime: 200, mtime_nsec: 0, hash: Some("h-new".to_owned()), ctag: Some("c-new".to_owned()) };
    let session = address(SESSION);
    match operation {
        Renamed => scene.step(|store| move_to(store, id, place(directory, "renamed"))),
        RenamedAndRenamedBack => {
            scene.step(|store| move_to(store, id, place(directory, "renamed")));
            scene.step(|store| move_to(store, id, at()));
        }
        MovedBackToSyncedPlace => scene.step(|store| move_to(store, id, place(directory, "report"))),
        MovedToAnotherDirectory => scene.step(|store| move_to(store, id, place(above, "report"))),
        SavedOverABoundFile | SavedOverANewFile => {
            scene.step(|store| apply(store, LocalVersionChange::SavedOver { replacing: id, replaced: other() }));
        }
        ANewFileSavedOverIt | ABoundFileSavedOverIt => {
            scene.step(|store| apply(store, LocalVersionChange::SavedOver { replacing: other(), replaced: id }));
        }
        RemovedHere => scene.step(|store| remove(store, id)),
        RemovedAndMadeAgain => {
            scene.step(|store| remove(store, id));
            scene.step(make_again);
        }
        OpenedForWriting => {
            scene.step(|store| apply(store, LocalVersionChange::OpenedForWriting { item: id }));
        }
        ClosedChanged => scene.step(|store| change_content(store, id)),
        ClosedUnchanged => {
            scene.step(|store| apply(store, LocalVersionChange::OpenedForWriting { item: id }));
            scene.step(|store| apply(store, LocalVersionChange::ClosedUnchanged { item: id }));
        }
        Truncated => {
            scene.step(|store| apply(store, LocalVersionChange::Truncated { item: id, size: 1, mtime: 300, mtime_nsec: 1 }));
        }
        ExchangedPlaces => scene.step(|store| store.item_store().exchange_local_places(id, other()).unwrap()),
        Merged => scene.step(|store| {
            let listed = bound_row(store, "N");
            store.item_store().merge_items(id, listed).unwrap();
        }),
        BoundFirst => scene.step(|store| store.item_store().bind_item(id, &reference("NEW")).unwrap()),
        BoundAndMerged => scene.step(|store| store.item_store().bind_item(id, &reference("N")).unwrap()),
        GaveUpBinding => {
            scene.step(|store| store.item_store().give_up_binding(id, false).unwrap());
        }
        DirectoryGaveUpBinding => {
            scene.step(|store| store.item_store().give_up_binding(directory, false).unwrap());
        }
        DirectoryAboveGaveUpBinding => {
            scene.step(|store| store.item_store().give_up_binding(above, false).unwrap());
        }
        MergedAndGaveUpBinding => {
            scene.step(|store| {
                let listed = bound_row(store, "N");
                store.item_store().merge_items(id, listed).unwrap();
            });
            scene.step(|store| store.item_store().give_up_binding(id, false).unwrap());
        }
        MergedWithTheMismatchedItemAndSavedOver => {
            scene.step(|store| {
                write_listing_of_changes(store, &[listed_file(MISMATCHED, "D", "report", (0, "h-BAD"), "e-BAD")]);
                let listed = bound_row(store, MISMATCHED);
                store.item_store().merge_items(id, listed).unwrap();
            });
            scene.step(|store| apply(store, LocalVersionChange::SavedOver { replacing: other(), replaced: id }));
        }
        CommitSendNewItem | CommitSendNewItemAndMerged | CommitSendNewFileThatIsTheMismatchedItem => {
            let made = match operation {
                CommitSendNewItem => confirmed(kind, "NEW", "report", made_content),
                CommitSendNewItemAndMerged => confirmed(kind, "N", "report", listed_content(3, "h-N")),
                _ => confirmed(kind, MISMATCHED, "report", listed_content(0, "h-BAD")),
            };
            scene.step(|store| commit_upload(store, &planned_from, kind, "report", &made, Some(&session)));
            (scene.happening.is_request_confirmed, scene.happening.is_session_ended) = (true, true);
        }
        CommitSendNewFileChangedMeanwhile => {
            scene.step(|store| change_content(store, id));
            scene.step(|store| commit_upload(store, &planned_from, kind, "report", &confirmed(kind, "NEW", "report", made_content), Some(&session)));
            (scene.happening.is_request_confirmed, scene.happening.is_session_ended) = (true, true);
        }
        CommitSendNewFileUnderItsSessionsName => {
            scene.step(|store| move_to(store, id, place(directory, "final")));
            scene.step(|store| commit_upload(store, &planned_from, kind, "report", &confirmed(kind, "NEW", "report", made_content), Some(&session)));
            (scene.happening.is_request_confirmed, scene.happening.is_session_ended) = (true, true);
        }
        CommitSendContent => {
            scene.step(|store| commit_upload(store, &planned_from, kind, "report", &confirmed(kind, "X", "report", listed_content(77, "h-sent")), Some(&session)));
            (scene.happening.is_request_confirmed, scene.happening.is_session_ended) = (true, true);
        }
        CommitSendContentChangedMeanwhile => {
            scene.step(|store| {
                apply(store, LocalVersionChange::Truncated { item: id, size: 1, mtime: 300, mtime_nsec: 1 });
            });
            scene.step(|store| commit_upload(store, &planned_from, kind, "report", &confirmed(kind, "X", "report", listed_content(77, "h-sent")), Some(&session)));
            (scene.happening.is_request_confirmed, scene.happening.is_session_ended) = (true, true);
        }
        CommitSendContentToldNoSession => {
            scene.step(|store| commit_upload(store, &planned_from, kind, "report", &confirmed(kind, "X", "report", listed_content(77, "h-sent")), None));
            scene.happening.is_request_confirmed = true;
        }
        CommitSendMove | CommitSendMoveChangedMeanwhile | CommitSendMoveToTemporaryName | CommitSendMoveAfterAnotherRename => {
            let (planned_from, name) = match operation {
                CommitSendMoveChangedMeanwhile => {
                    scene.step(|store| change_content(store, id));
                    (planned_from, "moved".to_owned())
                }
                CommitSendMoveToTemporaryName => (planned_from, temporary_toward_place(at(), id).name.to_str().unwrap().to_owned()),
                CommitSendMoveAfterAnotherRename => {
                    scene.step(|store| move_to(store, id, place(directory, "renamed")));
                    (scene.step(|store| versions(store, id)), "renamed".to_owned())
                }
                _ => (planned_from, "moved".to_owned()),
            };
            let moved = confirmed(kind, "X", &name, listed_content(3, "h-X"));
            scene.step(|store| is_written(store.item_store().commit_send_move(&planned_from, &reference("D"), OsStr::new(&name), &moved)));
            scene.happening.is_request_confirmed = true;
            scene.happening.confirmed_move = Some(place(directory, &name));
        }
        CommitMoveItem => scene.step(|store| is_written(store.item_store().commit_move_item(&planned_from, false))),
        CommitTakeMoveBack => scene.step(|store| is_written(store.item_store().commit_take_move_back(&planned_from, false))),
        CommitTakeOneDriveContent => scene.step(|store| is_written(store.item_store().commit_take_onedrive_content(&planned_from))),
        CommitAgreeOnContent => scene.step(|store| is_written(store.item_store().commit_agree_on_content(&planned_from))),
        CommitRemoveItem => scene.step(|store| is_written(store.item_store().commit_remove_item(&planned_from))),
        CommitDownloadContent => scene.step(|store| is_written(store.item_store().commit_download_content(&planned_from))),
        CommitRenameToConflictCopy => scene.step(|store| is_written(store.item_store().commit_rename_to_conflict_copy(&planned_from, OsStr::new("report (conflict)"), false))),
        CommitMoveToTemporaryName => scene.step(|store| is_written(store.item_store().commit_move_to_temporary_name(&planned_from, &place(directory, ".report.swap"), false))),
        CommitGiveUpBinding => scene.step(|store| is_written(store.item_store().commit_give_up_binding(&planned_from, false))),
        CommitMergeItems => scene.step(|store| {
            let onedrive = bound_row(store, "N");
            let onedrive = versions(store, onedrive);
            is_written(store.item_store().commit_merge_items(&planned_from, &onedrive));
        }),
        CommitHashLocalContent => {
            scene.step(|store| is_written(store.item_store().commit_hash_local_content(&planned_from, "h-local")));
            scene.happening.is_hash_computed = true;
        }
        CommitAgreeOnPlace => scene.step(|store| is_written(store.item_store().commit_agree_on_place(&planned_from))),
        CeasedToBeShown => scene.step(|store| store.item_store().clear_local_and_synced_version(id).unwrap()),
        RowDeleted => {
            scene.step(|store| store.conn.execute("DELETE FROM items WHERE local_item_id = ?1", [id.as_i64()]).unwrap());
        }
        ListedAtAnotherPlace => scene.step(|store| write_listing_of_changes(store, &[listed(kind, "X", "elsewhere", "e-X2")])),
        ListedWithAnotherContent => scene.step(|store| write_listing_of_changes(store, &[listed_file("X", "D", "report", (9, "h-X2"), "e-X2")])),
        ListedGone => scene.step(|store| write_listing_of_changes(store, &[listed_gone("X")])),
        ListedAnotherItemAtItsName => scene.step(|store| write_listing_of_changes(store, &[listed(kind, "N", "report", "e-N2")])),
        ListedTheMismatchedItem | ListedTheMismatchedItemAndShownIt | ShownTheMismatchedItemAndRemovedHere | RemovedHereAndShownTheMismatchedItem => {
            if operation == RemovedHereAndShownTheMismatchedItem {
                scene.step(|store| remove(store, id));
            }
            scene.step(|store| write_listing_of_changes(store, &[listed_file(MISMATCHED, "D", "report", (0, "h-BAD"), "e-BAD")]));
            if matches!(operation, ListedTheMismatchedItemAndShownIt | ShownTheMismatchedItemAndRemovedHere) {
                scene.step(|store| is_written(store.item_store().commit_rename_to_conflict_copy(&planned_from, OsStr::new("report (conflict)"), false)));
            }
            if operation != ListedTheMismatchedItem {
                scene.step(|store| show(store, MISMATCHED));
            }
            if operation == ShownTheMismatchedItemAndRemovedHere {
                scene.step(|store| remove(store, id));
            }
        }
        ShownAgain => scene.step(show_again),
        ShownAgainAndRemovedAgain => {
            scene.step(show_again);
            scene.step(|store| remove(store, id));
        }
        MadeAgain => {
            scene.step(make_again);
        }
        MadeAgainAndRemovedAgain => {
            let made = scene.step(make_again);
            scene.step(|store| remove(store, made));
        }
        ShownAgainAndGaveUpBinding => {
            scene.step(show_again);
            scene.step(|store| store.item_store().give_up_binding(id, false).unwrap());
        }
        ShownAgainAndCeasedToBeShown => {
            scene.step(show_again);
            scene.step(|store| store.item_store().clear_local_and_synced_version(id).unwrap());
        }
        CommitSendRemoval => {
            scene.step(|store| {
                is_written(match kind {
                    ItemKind::File => store.item_store().commit_send_file_removal(&planned_from),
                    ItemKind::Directory => store.item_store().commit_send_directory_removal(&planned_from, &[]),
                })
            });
            (scene.happening.is_request_confirmed, scene.happening.is_removal_confirmed) = (true, true);
        }
        CommitSendRemovalOfTheDirectory => {
            scene.step(|store| {
                let removed_directory = versions(store, directory);
                is_written(store.item_store().commit_send_directory_removal(&removed_directory, &[planned_from]));
            });
            (scene.happening.is_request_confirmed, scene.happening.is_removal_confirmed) = (true, true);
        }
        CommitForgetSyncedVersion => scene.step(|store| is_written(store.item_store().commit_forget_synced_version(&planned_from))),
        CommitAgreeOnRemoval => scene.step(|store| is_written(store.item_store().commit_agree_on_removal(&planned_from))),
    }
}

/// Whether `memory` holds the fact.
fn holds(memory: &ItemMemory, fact: Fact) -> bool {
    match fact {
        Fact::UploadSession => memory.upload_session.is_some(),
        Fact::Opening => false,
        Fact::MismatchedItem => memory.mismatched_item.is_some(),
        Fact::TemporaryName => memory.temporary_name.is_some(),
        Fact::FailedTries => memory.failed_tries.is_some(),
        Fact::SpaceRefusal => memory.space_refusal.is_some(),
        Fact::Refusal => memory.refusal.as_ref().is_some_and(|refusal| !is_removal_refusal(refusal)),
        Fact::TakenName => memory.taken_name.is_some(),
        Fact::RemovalConfirmation => memory.removal_confirmation.is_some(),
        Fact::RemovalRefusal => memory.refusal.as_ref().is_some_and(is_removal_refusal),
    }
}

/// Whether what the fact leaves to do in OneDrive is queued: the cancel of the session, the
/// removal of the item an upload left.
fn is_queued(store: &mut TreeStore, fact: Fact) -> bool {
    match fact {
        Fact::UploadSession => abandoned(store).contains(&address(SESSION)),
        Fact::MismatchedItem => list_queued_mismatched_items(store).contains(&OneDriveItemId::new(MISMATCHED)),
        _ => false,
    }
}

/// The items other than `id` whose row of the table holds the fact.
fn list_other_holders(store: &TreeStore, id: LocalItemId, fact: Fact) -> Vec<LocalItemId> {
    let rows = super::super::memory_row::list_memories(&store.conn, "1 = 1", &[]).unwrap();
    rows.into_iter().filter(|(holder, memory)| *holder != id && holds(memory, fact)).map(|(holder, _)| holder).collect()
}

/// Whether the reader that lists the items of a fact, where the fact has one, lists `id`.
fn is_listed(store: &mut TreeStore, id: LocalItemId, fact: Fact, at: Option<&ItemPlace>) -> Option<bool> {
    let memory = store.step_memory();
    Some(match fact {
        Fact::FailedTries => memory.list_timed_waits(100).unwrap().iter().any(|wait| wait.item == id),
        Fact::Refusal | Fact::RemovalRefusal => memory.list_refusal_reasons(None, None, 100).unwrap().iter().any(|refused| refused.item == id),
        Fact::SpaceRefusal => memory.list_space_refusal_reasons(None, 100).unwrap().iter().any(|refused| refused.item == id),
        Fact::RemovalConfirmation => memory.list_items_with_confirmed_removal(None, 100).unwrap().contains(&id),
        Fact::TakenName => memory.list_taken_names(at?.parent_local_item_id).unwrap().iter().any(|taken| taken.item == id),
        _ => return None,
    })
}

/// The first write of a new try for the item: where what its row does not say any more is
/// taken away. For a file the folder shows, the opening of an upload session at its place;
/// for another row, the forgetting of a fact that is not the cell's.
fn begin_next_try(store: &mut TreeStore, id: LocalItemId, kind: ItemKind, fact: Fact) {
    let Some(row) = store.item_store().find_item(id).unwrap() else { return };
    match row.local.and_then(|local| local.place).filter(|_| kind == ItemKind::File) {
        Some(at) => {
            store.step_memory().remember_upload_session_opening(id, &at, 500).unwrap();
        }
        None => {
            let other_fact = if fact == Fact::TakenName { StepFact::FailedTries } else { StepFact::TakenName };
            store.step_memory().forget_fact(id, other_fact).unwrap();
        }
    }
}

/// What became of the fact, and anything that is wrong whatever the fate.
fn find_fate(scene: &mut Scene, fact: Fact) -> (Fate, Vec<String>) {
    let (id, directory, kind) = (scene.item, scene.directory, scene.kind);
    let at = scene.learnt.as_ref().and_then(|learnt| learnt.local.as_ref()).and_then(|local| local.place.clone());
    let store = &mut scene.store;
    let mut wrong = Vec::new();
    // Whatever the fact: no item is queued for removal while the folder has it.
    for queued in list_queued_mismatched_items(store) {
        let holder = store.item_store().find_bound_item(&OneDriveItemReference { drive_id: reference("X").drive_id, item_id: queued }).unwrap();
        if holder.is_some_and(|holder| holder.local.is_some() || holder.synced_place.is_some()) {
            wrong.push("an item the folder has is queued to be removed in OneDrive".to_owned());
        }
    }
    if fact == Fact::Opening {
        let at = at.expect("an opening is learnt of a shown file");
        let at_another_directory: i64 = store.conn.query_row("SELECT count(*) FROM upload_session_openings WHERE directory_local_item_id != ?1", [directory.as_i64()], |row| row.get(0)).unwrap();
        let fate = match (windows(store, &at).is_empty(), at_another_directory) {
            (false, _) => Fate::Answered,
            (true, 0) => Fate::Gone,
            (true, _) => Fate::Moved,
        };
        return (fate, wrong);
    }
    let is_answered = holds(&said(store, id), fact);
    if is_listed(store, id, fact, at.as_ref()).is_some_and(|is_listed| is_listed != is_answered) {
        wrong.push(format!("the item's memory answers it: {is_answered}, and the reader that lists such items the other way"));
    }
    if fact == Fact::UploadSession && !is_answered {
        if let Some(at) = &at {
            let sessions_at_place = store.step_memory().list_empty_file_sessions(at).unwrap();
            if sessions_at_place.contains(&EmptyFileSession::Continued { item: id, address: address(SESSION) }) {
                wrong.push("the reader of the sessions at a place answers it as the item's, which the item's memory does not".to_owned());
            }
        }
    }
    begin_next_try(store, id, kind, fact);
    let (is_still_answered, is_kept) = (holds(&said(store, id), fact), holds(&kept(store, id), fact));
    if fact == Fact::UploadSession && is_kept && is_queued(store, fact) {
        wrong.push("the address is the item's and to be cancelled at once".to_owned());
    }
    if is_answered != is_still_answered {
        wrong.push(format!("answered before the next write: {is_answered}, after it: {is_still_answered}"));
    }
    let fate = if is_answered {
        Fate::Answered
    } else if is_kept {
        Fate::KeptUnanswered
    } else if !list_other_holders(store, id, fact).is_empty() {
        Fate::Moved
    } else if is_queued(store, fact) {
        Fate::Queued
    } else {
        Fate::Gone
    };
    (fate, wrong)
}

/// What the rows say when the cell is over.
fn read_rows_after(scene: &mut Scene) -> RowsAfter {
    let learnt_binding = scene.learnt.as_ref().and_then(|learnt| learnt.onedrive_item.clone());
    let row = scene.row();
    let store = &mut scene.store;
    let holder_of_learnt_binding = learnt_binding.and_then(|binding| store.item_store().find_bound_item(&binding).unwrap()).map(|holder| holder.local_item_id);
    let mismatched = store.item_store().find_bound_item(&reference(MISMATCHED)).unwrap();
    RowsAfter { row, holder_of_learnt_binding, is_mismatched_item_of_the_folder: mismatched.is_some_and(|holder| holder.local.is_some() || holder.synced_place.is_some()) }
}

/// One cell: the fate the rule gives and the fate found, with what is wrong of it; `None`
/// where the fact is not one of such an item.
fn run_cell(operation: Operation, kind: ItemKind, shape: Shape, fact: Fact) -> Option<(Fate, Fate, Vec<String>)> {
    let mut scene = scene(kind, shape);
    prepare(&mut scene, operation);
    scene.learn(fact)?;
    act(&mut scene, operation);
    let after = read_rows_after(&mut scene);
    let by_rule = decide_fate_by_rule(fact, kind, scene.learnt.as_ref().expect("learnt"), &after, &scene.happening);
    let (found, wrong) = find_fate(&mut scene, fact);
    Some((by_rule, found, wrong))
}

/// The enumeration: every fact against every operation, for a file and for a directory in
/// every shape the operation is done for, each cell as the rule says. Run with
/// `--nocapture` it prints the cells as found.
#[test]
fn every_fact_meets_every_operation_and_becomes_what_the_rule_says() {
    let (mut failures, mut cells) = (Vec::new(), 0);
    for operation in OPERATIONS {
        let mut is_done_for_any = false;
        for kind in [ItemKind::File, ItemKind::Directory] {
            for shape in shapes_for(operation, kind).iter().filter(|shape| kind == ItemKind::File || **shape != Edited) {
                is_done_for_any = true;
                let mut line = format!("{operation:?} / {kind:?} {shape:?}:");
                for fact in FACTS {
                    let Some((by_rule, found, wrong)) = run_cell(operation, kind, *shape, fact) else { continue };
                    cells += 1;
                    line.push_str(&format!(" {fact:?}={found:?}"));
                    if found != by_rule {
                        failures.push(format!("{operation:?} / {kind:?} {shape:?} x {fact:?}: {found:?}, by the rule {by_rule:?}"));
                    }
                    failures.extend(wrong.into_iter().map(|what| format!("{operation:?} / {kind:?} {shape:?} x {fact:?}: {what}")));
                }
                println!("{line}");
            }
        }
        assert!(is_done_for_any, "{operation:?} is done for no shape");
    }
    println!("{cells} cells");
    assert!(failures.is_empty(), "{} of {cells} cells are not as the rule says:\n{}", failures.len(), failures.join("\n"));
}
