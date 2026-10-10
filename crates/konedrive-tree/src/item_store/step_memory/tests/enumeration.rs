//! Every fact a step remembers, against every operation of the store that can change what
//! the fact was learnt for: one cell each, run in a store of its own. A cell says what
//! becomes of the fact ([`Fate`]), and for a fact that is still answered why that is right.
//! The table is [`fates_of_a_shown_file`] and [`fates_of_a_removed_file`]; an operation that
//! is added to the store gets a line here before it is used.
//!
//! The item of every cell is a file `report` in the directory `docs`, which is in the
//! directory `above`, both directories of OneDrive the folder shows. The file is new here,
//! or an item of OneDrive the two sides agree on, or such an item removed here
//! ([`ItemShape`]). The fact is written by its own operation, then the operation of the
//! cell is done, and then the first write of a new try for the item, which is where what
//! the row does not say any more is taken away.

use std::ffi::OsStr;

use konedrive_reason::Reason;

use super::*;
use crate::item_store::tests::listed_drive::{listed_directory, listed_file, listed_gone, reference, store_with_listed_root, write_listing_of_changes};
use crate::item_store::{ConfirmedItem, StepCommitOutcome};
use crate::model::{ItemKind, OneDriveItemId};
use crate::sync_plan::ItemVersions;

/// What the item of a cell is when the fact is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemShape {
    /// A file made here that OneDrive does not have.
    NewFile,
    /// A file of OneDrive the folder shows, the two sides agreeing on it.
    BoundFile,
    /// A file of OneDrive removed here, its removal not sent.
    RemovedFile,
}

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

/// The facts of a file the folder shows, in the order of [`fates_of_a_shown_file`].
const FACTS_OF_A_SHOWN_FILE: [Fact; 8] =
    [Fact::UploadSession, Fact::Opening, Fact::MismatchedItem, Fact::TemporaryName, Fact::FailedTries, Fact::SpaceRefusal, Fact::Refusal, Fact::TakenName];

/// The facts of a file removed here, in the order of [`fates_of_a_removed_file`].
const FACTS_OF_A_REMOVED_FILE: [Fact; 4] = [Fact::RemovalConfirmation, Fact::RemovalRefusal, Fact::FailedTries, Fact::MismatchedItem];

/// What becomes of a fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    /// Still answered for the item, and after the next write for it.
    Answered,
    /// Not answered, and nothing of it is kept after the next write for the item, or with
    /// its row.
    Gone,
    /// Kept for another item, or for another directory.
    Moved,
    /// Not answered, and what is left to do in OneDrive is queued: the cancel of an upload
    /// session, the removal of the item an upload left.
    Queued,
    /// Not answered, and still in the item's row after the next write for it. Never right.
    KeptUnanswered,
}

/// What a cell is to be: the fate, and for a fact still answered why that is right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cell {
    Answered(&'static str),
    Gone,
    Moved,
    Queued,
}

impl Cell {
    fn fate(self) -> Fate {
        match self {
            Cell::Answered(_) => Fate::Answered,
            Cell::Gone => Fate::Gone,
            Cell::Moved => Fate::Moved,
            Cell::Queued => Fate::Queued,
        }
    }
}

use Cell::{Gone, Moved, Queued};

/// The bytes the fact was learnt of are the file's.
const BYTES: Cell = Cell::Answered("the bytes it was learnt of are the file's");
/// The item is at the place, and for a refusal with the bytes, the fact was learnt for.
const VERSION: Cell = Cell::Answered("the local version it was learnt for is the item's");
/// The fact holds no version of the item.
const NO_VERSION: Cell = Cell::Answered("it holds no version of the item: kept until its step forgets it, the binding moves or the row goes");
/// The fact is about the item an upload left, whatever becomes of the file.
const LEFT_IN_ONEDRIVE: Cell = Cell::Answered("the item an upload left is in OneDrive whatever becomes of the file: kept until it is removed there");
/// An opening is of its place.
const OF_THE_PLACE: Cell = Cell::Answered("it is of the place in OneDrive, not of the file: the empty file may still hold the name");
/// A temporary name says where OneDrive has the item.
const IN_ONEDRIVE: Cell = Cell::Answered("it says under which name OneDrive has the item, which nothing done here changes: its step forgets it");
/// The removal is the one the fact was learnt for.
const SAME_REMOVAL: Cell = Cell::Answered("the row is the same unsent removal, with the stamp it had: the directory of OneDrive it is removed from is the same, under the row that has its binding now");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    // The local version changes.
    Renamed,
    MovedToAnotherDirectory,
    SavedOverABoundFile,
    SavedOverANewFile,
    ANewFileSavedOverTheNewFile,
    ANewFileSavedOverTheBoundFile,
    ABoundFileSavedOverTheBoundFile,
    RemovedNeverInOneDrive,
    Removed,
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
    DirectoryOfNewFileGaveUpBinding,
    DirectoryOfBoundFileGaveUpBinding,
    DirectoryAboveGaveUpBinding,
    // The commits of the steps.
    CommitSendNewFile,
    CommitSendContent,
    CommitSendMove,
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
    // Of a file removed here.
    ShownAgain,
    ShownAgainAndRemovedAgain,
    MadeAgain,
    MadeAgainAndRemovedAgain,
    ShownAgainAndGaveUpBinding,
    ShownAgainAndCeasedToBeShown,
    CommitSendFileRemoval,
    CommitSendDirectoryRemoval,
    CommitForgetSyncedVersion,
    CommitDropRow,
    DirectoryOfRemovedFileGaveUpBinding,
    RowOfRemovedFileDeleted,
}

use Operation::*;

const OPERATIONS_ON_A_SHOWN_FILE: [Operation; 39] = [
    Renamed,
    MovedToAnotherDirectory,
    SavedOverABoundFile,
    SavedOverANewFile,
    ANewFileSavedOverTheNewFile,
    ANewFileSavedOverTheBoundFile,
    ABoundFileSavedOverTheBoundFile,
    RemovedNeverInOneDrive,
    Removed,
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
    DirectoryOfNewFileGaveUpBinding,
    DirectoryOfBoundFileGaveUpBinding,
    DirectoryAboveGaveUpBinding,
    CommitSendNewFile,
    CommitSendContent,
    CommitSendMove,
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
];

const OPERATIONS_ON_A_REMOVED_FILE: [Operation; 12] = [
    ShownAgain,
    ShownAgainAndRemovedAgain,
    MadeAgain,
    MadeAgainAndRemovedAgain,
    ShownAgainAndGaveUpBinding,
    ShownAgainAndCeasedToBeShown,
    CommitSendFileRemoval,
    CommitSendDirectoryRemoval,
    CommitForgetSyncedVersion,
    CommitDropRow,
    DirectoryOfRemovedFileGaveUpBinding,
    RowOfRemovedFileDeleted,
];

/// What the file is when the fact is written, for the operation to be one the store does.
fn shape_for(operation: Operation) -> ItemShape {
    match operation {
        Renamed | MovedToAnotherDirectory | SavedOverABoundFile | SavedOverANewFile | ANewFileSavedOverTheNewFile | RemovedNeverInOneDrive | OpenedForWriting | ClosedChanged
        | ClosedUnchanged | Truncated | ExchangedPlaces | Merged | BoundFirst | BoundAndMerged | DirectoryOfNewFileGaveUpBinding | DirectoryAboveGaveUpBinding | CommitSendNewFile
        | CommitRenameToConflictCopy | CommitMergeItems | CommitHashLocalContent | RowDeleted => ItemShape::NewFile,
        ANewFileSavedOverTheBoundFile | ABoundFileSavedOverTheBoundFile | Removed | RemovedAndMadeAgain | GaveUpBinding | DirectoryOfBoundFileGaveUpBinding | CommitSendContent
        | CommitSendMove | CommitMoveItem | CommitTakeMoveBack | CommitTakeOneDriveContent | CommitAgreeOnContent | CommitRemoveItem | CommitDownloadContent
        | CommitMoveToTemporaryName | CommitGiveUpBinding | CommitAgreeOnPlace | CeasedToBeShown => ItemShape::BoundFile,
        ShownAgain | ShownAgainAndRemovedAgain | MadeAgain | MadeAgainAndRemovedAgain | ShownAgainAndGaveUpBinding | ShownAgainAndCeasedToBeShown | CommitSendFileRemoval
        | CommitSendDirectoryRemoval | CommitForgetSyncedVersion | CommitDropRow | DirectoryOfRemovedFileGaveUpBinding | RowOfRemovedFileDeleted => ItemShape::RemovedFile,
    }
}

/// What becomes of each fact of a file the folder shows, in the order of
/// [`FACTS_OF_A_SHOWN_FILE`]: the upload session, the opening, the item an upload left, the
/// temporary name, the failed tries, the refusal for lack of space, the refusal for good,
/// the taken name.
fn fates_of_a_shown_file(operation: Operation) -> [Cell; 8] {
    // The place changed and nothing else.
    let at_another_place = [BYTES, OF_THE_PLACE, LEFT_IN_ONEDRIVE, IN_ONEDRIVE, NO_VERSION, BYTES, Gone, Gone];
    // The content changed and nothing else.
    let with_another_content = [Queued, OF_THE_PLACE, LEFT_IN_ONEDRIVE, IN_ONEDRIVE, NO_VERSION, Gone, Gone, VERSION];
    // Nothing the facts were learnt for changed.
    let as_it_was = [BYTES, OF_THE_PLACE, LEFT_IN_ONEDRIVE, IN_ONEDRIVE, NO_VERSION, BYTES, VERSION, VERSION];
    // The row went: the address is left to cancel and the item an upload left to remove.
    let with_no_row = [Queued, OF_THE_PLACE, Queued, Gone, Gone, Gone, Gone, Gone];
    // The folder does not show the file any more, and its row stays.
    let not_shown = [Queued, OF_THE_PLACE, LEFT_IN_ONEDRIVE, IN_ONEDRIVE, NO_VERSION, Gone, Gone, Gone];
    // The item's binding is another than the one its requests were made for: what OneDrive
    // answered those requests is dropped, the session to be cancelled. What is of its
    // content and of the place it is at stays.
    let with_another_binding = [Queued, OF_THE_PLACE, LEFT_IN_ONEDRIVE, Gone, Gone, BYTES, Gone, VERSION];
    match operation {
        Renamed | MovedToAnotherDirectory | SavedOverANewFile | ExchangedPlaces | CommitMoveItem | CommitTakeMoveBack | CommitRenameToConflictCopy | CommitMoveToTemporaryName => at_another_place,
        // The binding of the file it was saved over is its own now, and its place is that
        // file's.
        SavedOverABoundFile => [Queued, OF_THE_PLACE, LEFT_IN_ONEDRIVE, Gone, Gone, BYTES, Gone, Gone],
        ANewFileSavedOverTheNewFile | ANewFileSavedOverTheBoundFile | RemovedNeverInOneDrive | RemovedAndMadeAgain | RowDeleted => with_no_row,
        ABoundFileSavedOverTheBoundFile | Removed | CommitRemoveItem | CeasedToBeShown => not_shown,
        // A writer has the file open: what it was before is kept, and nothing is planned of
        // the file until the close says whether it changed.
        OpenedForWriting | ClosedUnchanged => as_it_was,
        ClosedChanged | Truncated | CommitTakeOneDriveContent | CommitAgreeOnContent => with_another_content,
        Merged | BoundAndMerged | GaveUpBinding | CommitGiveUpBinding | CommitMergeItems => with_another_binding,
        // The item is given a binding first, and no binding moves: by itself it changes
        // nothing a fact was learnt for. The commit of the step that made the item is what
        // ends the facts of that request (`CommitSendNewFile`).
        BoundFirst => as_it_was,
        // The directory the folder shows is one OneDrive does not have yet: what OneDrive
        // answered about sending the file into the directory it had is not of this one.
        // The session of a new file would end in the directory of OneDrive that is another
        // row's now; its opening is with that row.
        DirectoryOfNewFileGaveUpBinding => [Queued, Moved, LEFT_IN_ONEDRIVE, IN_ONEDRIVE, NO_VERSION, BYTES, Gone, Gone],
        // The session of an item OneDrive has sends into that item, wherever it is.
        DirectoryOfBoundFileGaveUpBinding => [BYTES, Moved, LEFT_IN_ONEDRIVE, IN_ONEDRIVE, NO_VERSION, BYTES, Gone, Gone],
        // The directory the file was sent into has the binding it had.
        DirectoryAboveGaveUpBinding => as_it_was,
        // OneDrive confirmed the upload: the session that sent the content ended, and is
        // forgotten with no cancel; the tries of the request that went through are
        // forgotten; and the local content is the one OneDrive confirmed, so that what was
        // refused of the former is not said any more.
        CommitSendNewFile | CommitSendContent => [Gone, OF_THE_PLACE, LEFT_IN_ONEDRIVE, IN_ONEDRIVE, Gone, Gone, Gone, VERSION],
        // OneDrive confirmed the move to the place the temporary name led to: the exchange
        // is done, and the tries of the request that went through are forgotten. Nothing
        // the folder shows changed.
        CommitSendMove => [BYTES, OF_THE_PLACE, LEFT_IN_ONEDRIVE, Gone, Gone, BYTES, VERSION, VERSION],
        // What was agreed on, or whether the bytes are on disk, changed; nothing the folder
        // shows did.
        CommitDownloadContent | CommitAgreeOnPlace => as_it_was,
        // The same bytes, told by their hash from now on: the facts of the content go on.
        CommitHashLocalContent => as_it_was,
        _ => panic!("{operation:?} is no operation on a file the folder shows"),
    }
}

/// What becomes of each fact of a file removed here, in the order of
/// [`FACTS_OF_A_REMOVED_FILE`]: the confirmation of the removal, the refusal of the removal,
/// the failed tries, the item an upload left.
fn fates_of_a_removed_file(operation: Operation) -> [Cell; 4] {
    match operation {
        // Another removal, if the file is removed again: the user confirmed the first, and
        // OneDrive refused the first.
        ShownAgain | ShownAgainAndRemovedAgain | ShownAgainAndCeasedToBeShown => [Gone, Gone, NO_VERSION, LEFT_IN_ONEDRIVE],
        // The row went, and the file made again is another item's row.
        MadeAgain | MadeAgainAndRemovedAgain | RowOfRemovedFileDeleted => [Gone, Gone, Gone, Queued],
        ShownAgainAndGaveUpBinding => [Gone, Gone, Gone, LEFT_IN_ONEDRIVE],
        // OneDrive confirmed the removal: nothing is unsent of the row, and the tries of the
        // request that went through are forgotten.
        CommitSendFileRemoval | CommitSendDirectoryRemoval => [Gone, Gone, Gone, LEFT_IN_ONEDRIVE],
        // The row is no removal any more: nothing is unsent of it.
        CommitForgetSyncedVersion | CommitDropRow => [Gone, Gone, NO_VERSION, LEFT_IN_ONEDRIVE],
        DirectoryOfRemovedFileGaveUpBinding => [SAME_REMOVAL, SAME_REMOVAL, NO_VERSION, LEFT_IN_ONEDRIVE],
        _ => panic!("{operation:?} is no operation on a file removed here"),
    }
}

/// The store of a cell and the items in it.
struct Scene {
    store: TreeStore,
    above: LocalItemId,
    directory: LocalItemId,
    /// The file the fact is of.
    file: LocalItemId,
    /// The place the file has when the fact is written.
    place: ItemPlace,
    /// Another file the operation needs, when it needs one.
    other: Option<LocalItemId>,
}

fn versions(store: &mut TreeStore, id: LocalItemId) -> ItemVersions {
    ItemVersions::of(&item(store, id))
}

fn is_written(outcome: Result<StepCommitOutcome, crate::TreeError>) {
    assert_eq!(outcome.unwrap(), StepCommitOutcome::Written);
}

/// The folder shows the item of OneDrive `item_id`, which a listing brought.
fn show(store: &mut TreeStore, item_id: &str) -> LocalItemId {
    let id = store.item_store().find_bound_item(&reference(item_id)).unwrap().expect("listed").local_item_id;
    let planned_from = versions(store, id);
    is_written(store.item_store().commit_show_item(&planned_from, false));
    id
}

fn listed_content(size: u64, hash: &str) -> ContentVersion {
    ContentVersion::Hashed { size, mtime: 1000, mtime_nsec: 0, hash: Some(hash.to_owned()), ctag: Some(format!("c-{hash}")) }
}

fn confirmed(item_id: &str, name: &str, content: ContentVersion) -> ConfirmedItem {
    ConfirmedItem { item: reference(item_id), parent: reference("D"), name: name.into(), content: Some(content), etag: format!("e-{item_id}-confirmed") }
}

fn apply(store: &mut TreeStore, change: LocalVersionChange) -> LocalItemId {
    store.item_store().apply(&change).unwrap()
}

fn move_to(store: &mut TreeStore, id: LocalItemId, to: ItemPlace) {
    apply(store, LocalVersionChange::Moved { item: id, place: to, is_name_ignored: false });
}

fn scene(shape: ItemShape) -> Scene {
    let mut store = store_with_listed_root();
    write_listing_of_changes(&mut store, &[listed_directory("G", "R", "above", "e-G"), listed_directory("D", "G", "docs", "e-D")]);
    let (above, directory) = (show(&mut store, "G"), show(&mut store, "D"));
    let file = match shape {
        ItemShape::NewFile => make_file(&mut store, directory, "report"),
        ItemShape::BoundFile | ItemShape::RemovedFile => {
            write_listing_of_changes(&mut store, &[listed_file("X", "D", "report", (3, "h-X"), "e-X")]);
            show(&mut store, "X")
        }
    };
    if shape == ItemShape::RemovedFile {
        remove(&mut store, file);
    }
    Scene { store, above, directory, file, place: place(directory, "report"), other: None }
}

/// What the operation needs of the rows before the fact is written.
fn prepare(scene: &mut Scene, operation: Operation) {
    let (file, directory) = (scene.file, scene.directory);
    let store = &mut scene.store;
    match operation {
        SavedOverABoundFile | ABoundFileSavedOverTheBoundFile => {
            write_listing_of_changes(store, &[listed_file("O", "D", "other", (3, "h-O"), "e-O")]);
            scene.other = Some(show(store, "O"));
        }
        SavedOverANewFile | ANewFileSavedOverTheNewFile | ANewFileSavedOverTheBoundFile | ExchangedPlaces => scene.other = Some(make_file(store, directory, "other")),
        Merged | BoundAndMerged | CommitMergeItems => {
            write_listing_of_changes(store, &[listed_file("N", "D", "report", (5, "h-N"), "e-N")]);
            scene.other = Some(store.item_store().find_bound_item(&reference("N")).unwrap().expect("listed").local_item_id);
        }
        CommitSendContent => change_content(store, file),
        CommitSendMove | CommitTakeMoveBack => move_to(store, file, place(directory, "moved")),
        CommitMoveItem => write_listing_of_changes(store, &[listed_file("X", "D", "elsewhere", (3, "h-X"), "e-X2")]),
        CommitTakeOneDriveContent | CommitAgreeOnContent | ShownAgain | ShownAgainAndRemovedAgain | ShownAgainAndGaveUpBinding | ShownAgainAndCeasedToBeShown => {
            write_listing_of_changes(store, &[listed_file("X", "D", "report", (9, "h-X2"), "e-X2")]);
        }
        CommitRemoveItem | CommitDropRow => write_listing_of_changes(store, &[listed_gone("X")]),
        CommitAgreeOnPlace => {
            move_to(store, file, place(directory, "same"));
            write_listing_of_changes(store, &[listed_file("X", "D", "same", (3, "h-X"), "e-X2")]);
        }
        CommitSendDirectoryRemoval => remove(store, directory),
        _ => {}
    }
}

/// The operation of the cell.
fn act(scene: &mut Scene, operation: Operation) {
    let (file, directory, above, at, other) = (scene.file, scene.directory, scene.above, scene.place.clone(), scene.other);
    let other = move || other.expect("prepared");
    let store = &mut scene.store;
    let show_again = |store: &mut TreeStore| {
        let planned_from = versions(store, file);
        is_written(store.item_store().commit_show_item(&planned_from, false));
    };
    let make_again = |store: &mut TreeStore| {
        let content = Some(ContentVersion::ChangedHere { size: 4, mtime: 400, mtime_nsec: 0, change_count: 1 });
        apply(store, LocalVersionChange::Made { kind: ItemKind::File, place: at.clone(), content, is_name_ignored: false, replaced: Some(file) })
    };
    let planned_from = store.item_store().find_item(file).unwrap().map(|row| ItemVersions::of(&row));
    let planned_from = move || planned_from.clone().expect("the file has a row");
    match operation {
        Renamed => move_to(store, file, place(directory, "renamed")),
        MovedToAnotherDirectory => move_to(store, file, place(above, "report")),
        SavedOverABoundFile | SavedOverANewFile => {
            apply(store, LocalVersionChange::SavedOver { replacing: file, replaced: other() });
        }
        ANewFileSavedOverTheNewFile | ANewFileSavedOverTheBoundFile | ABoundFileSavedOverTheBoundFile => {
            apply(store, LocalVersionChange::SavedOver { replacing: other(), replaced: file });
        }
        RemovedNeverInOneDrive | Removed => remove(store, file),
        RemovedAndMadeAgain => {
            remove(store, file);
            make_again(store);
        }
        OpenedForWriting => {
            apply(store, LocalVersionChange::OpenedForWriting { item: file });
        }
        ClosedChanged => change_content(store, file),
        ClosedUnchanged => {
            apply(store, LocalVersionChange::OpenedForWriting { item: file });
            apply(store, LocalVersionChange::ClosedUnchanged { item: file });
        }
        Truncated => {
            apply(store, LocalVersionChange::Truncated { item: file, size: 1, mtime: 300, mtime_nsec: 1 });
        }
        ExchangedPlaces => store.item_store().exchange_local_places(file, other()).unwrap(),
        Merged => store.item_store().merge_items(file, other()).unwrap(),
        BoundFirst => store.item_store().bind_item(file, &reference("NEW")).unwrap(),
        BoundAndMerged => store.item_store().bind_item(file, &reference("N")).unwrap(),
        GaveUpBinding => {
            store.item_store().give_up_binding(file, false).unwrap();
        }
        DirectoryOfNewFileGaveUpBinding | DirectoryOfBoundFileGaveUpBinding | DirectoryOfRemovedFileGaveUpBinding => {
            store.item_store().give_up_binding(directory, false).unwrap();
        }
        DirectoryAboveGaveUpBinding => {
            store.item_store().give_up_binding(above, false).unwrap();
        }
        CommitSendNewFile => {
            let made = confirmed("NEW", "report", ContentVersion::Hashed { size: 0, mtime: 200, mtime_nsec: 0, hash: Some("h-new".to_owned()), ctag: Some("c-new".to_owned()) });
            is_written(store.item_store().commit_send_new_file(&planned_from(), &reference("D"), OsStr::new("report"), &made));
        }
        CommitSendContent => is_written(store.item_store().commit_send_content(&planned_from(), &confirmed("X", "report", listed_content(77, "h-sent")))),
        CommitSendMove => is_written(store.item_store().commit_send_move(&planned_from(), &reference("D"), OsStr::new("moved"), &confirmed("X", "moved", listed_content(3, "h-X")))),
        CommitMoveItem => is_written(store.item_store().commit_move_item(&planned_from(), false)),
        CommitTakeMoveBack => is_written(store.item_store().commit_take_move_back(&planned_from(), false)),
        CommitTakeOneDriveContent => is_written(store.item_store().commit_take_onedrive_content(&planned_from())),
        CommitAgreeOnContent => is_written(store.item_store().commit_agree_on_content(&planned_from())),
        CommitRemoveItem => is_written(store.item_store().commit_remove_item(&planned_from())),
        CommitDownloadContent => is_written(store.item_store().commit_download_content(&planned_from())),
        CommitRenameToConflictCopy => is_written(store.item_store().commit_rename_to_conflict_copy(&planned_from(), OsStr::new("report (conflict)"), false)),
        CommitMoveToTemporaryName => is_written(store.item_store().commit_move_to_temporary_name(&planned_from(), &place(directory, ".report.swap"), false)),
        CommitGiveUpBinding => is_written(store.item_store().commit_give_up_binding(&planned_from(), false)),
        CommitMergeItems => {
            let onedrive = versions(store, other());
            is_written(store.item_store().commit_merge_items(&planned_from(), &onedrive));
        }
        CommitHashLocalContent => is_written(store.item_store().commit_hash_local_content(&planned_from(), "h-local")),
        CommitAgreeOnPlace => is_written(store.item_store().commit_agree_on_place(&planned_from())),
        CeasedToBeShown => store.item_store().clear_local_and_synced_version(file).unwrap(),
        RowDeleted | RowOfRemovedFileDeleted => {
            store.conn.execute("DELETE FROM items WHERE local_item_id = ?1", [file.as_i64()]).unwrap();
        }
        ShownAgain => show_again(store),
        ShownAgainAndRemovedAgain => {
            show_again(store);
            remove(store, file);
        }
        MadeAgain => {
            make_again(store);
        }
        MadeAgainAndRemovedAgain => {
            let made = make_again(store);
            remove(store, made);
        }
        ShownAgainAndGaveUpBinding => {
            show_again(store);
            store.item_store().give_up_binding(file, false).unwrap();
        }
        ShownAgainAndCeasedToBeShown => {
            show_again(store);
            store.item_store().clear_local_and_synced_version(file).unwrap();
        }
        CommitSendFileRemoval => is_written(store.item_store().commit_send_file_removal(&planned_from())),
        CommitSendDirectoryRemoval => {
            let removed_directory = versions(store, directory);
            is_written(store.item_store().commit_send_directory_removal(&removed_directory, &[planned_from()]));
        }
        CommitForgetSyncedVersion => is_written(store.item_store().commit_forget_synced_version(&planned_from())),
        CommitDropRow => is_written(store.item_store().commit_drop_row(&planned_from())),
    }
}

const SESSION: &str = "of-the-cell";
const MISMATCHED: &str = "BAD";

/// The fact is written of the file by its own operation, and is answered.
fn remember(scene: &mut Scene, fact: Fact) {
    let (file, at) = (scene.file, scene.place.clone());
    let store = &mut scene.store;
    let is_bound = item(store, file).onedrive_item.is_some();
    match fact {
        Fact::UploadSession => {
            let session = UploadSession { address: address(SESSION), expires_at: None, next_offset: 0, content: local_content(store, file), empty_file_place: (!is_bound).then_some(at) };
            assert_eq!(store.step_memory().remember_upload_session(file, &session).unwrap(), Continued);
        }
        Fact::Opening => {
            store.step_memory().remember_upload_session_opening(file, &at, 10).unwrap();
        }
        Fact::MismatchedItem => store.step_memory().remember_mismatched_item(file, &MismatchedItem::of_answer(OneDriveItemId::new(MISMATCHED), Some("c-bad"), None)).unwrap(),
        Fact::TemporaryName => {
            let temporary = temporary_toward(store, file);
            store.step_memory().remember_temporary_name(file, &temporary).unwrap();
        }
        Fact::FailedTries => {
            store.step_memory().remember_failed_try(file, &Reason::Network, Some(60)).unwrap();
        }
        Fact::SpaceRefusal => {
            let refused = space_refusal(store, file, 1);
            assert_eq!(store.step_memory().remember_space_refusal(file, &refused).unwrap(), StillSaid);
        }
        Fact::Refusal => {
            let refusal = refusal_of(store, file, Reason::Forbidden);
            assert_eq!(store.step_memory().remember_refusal(file, &refusal).unwrap(), StillSaid);
        }
        Fact::TakenName => {
            let taken = taken_at(store, file, "REPORT");
            assert_eq!(store.step_memory().remember_taken_name(file, &taken).unwrap(), StillSaid);
        }
        Fact::RemovalConfirmation => {
            let removal = unsent_removal(store, file);
            assert_eq!(store.step_memory().remember_removals_confirmed(&[removal]).unwrap(), 1);
        }
        Fact::RemovalRefusal => {
            let version = RefusedVersion::Removal { unsent_since: unsent_removal(store, file).unsent_since };
            assert_eq!(store.step_memory().remember_refusal(file, &Refusal { reason: Reason::Forbidden, version }).unwrap(), StillSaid);
        }
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
        Fact::Refusal => memory.refusal.as_ref().is_some_and(|refusal| matches!(refusal.version, RefusedVersion::Shown(_))),
        Fact::TakenName => memory.taken_name.is_some(),
        Fact::RemovalConfirmation => memory.removal_confirmation.is_some(),
        Fact::RemovalRefusal => memory.refusal.as_ref().is_some_and(|refusal| matches!(refusal.version, RefusedVersion::Removal { .. })),
    }
}

/// Whether what the fact leaves to do in OneDrive is queued: the cancel of the session, the
/// removal of the item an upload left.
fn is_queued(store: &mut TreeStore, fact: Fact) -> bool {
    match fact {
        Fact::UploadSession => abandoned(store).contains(&address(SESSION)),
        Fact::MismatchedItem => store.step_memory().list_mismatched_item_removals(100).unwrap().iter().any(|left| left.item_id == OneDriveItemId::new(MISMATCHED)),
        _ => false,
    }
}

/// The items other than `file` whose row of the table holds the fact.
fn list_other_holders(store: &TreeStore, file: LocalItemId, fact: Fact) -> Vec<LocalItemId> {
    let rows = super::super::memory_row::list_memories(&store.conn, "1 = 1", &[]).unwrap();
    rows.into_iter().filter(|(id, memory)| *id != file && holds(memory, fact)).map(|(id, _)| id).collect()
}

/// The first write of a new try for the file: where what its row does not say any more is
/// taken away. For a file the folder shows, the opening of an upload session at its place;
/// for another row, the forgetting of a fact that is not the cell's.
fn begin_next_try(store: &mut TreeStore, file: LocalItemId, fact: Fact) {
    let Some(row) = store.item_store().find_item(file).unwrap() else { return };
    match row.local.and_then(|local| local.place) {
        Some(at) => {
            store.step_memory().remember_upload_session_opening(file, &at, 500).unwrap();
        }
        _ => {
            let other_fact = if fact == Fact::TakenName { StepFact::FailedTries } else { StepFact::TakenName };
            store.step_memory().forget_fact(file, other_fact).unwrap();
        }
    }
}

/// What became of the fact, and anything that is wrong whatever the fate.
fn find_fate(scene: &mut Scene, fact: Fact) -> (Fate, Vec<String>) {
    let (file, at, directory) = (scene.file, scene.place.clone(), scene.directory);
    let store = &mut scene.store;
    let mut wrong = Vec::new();
    if fact == Fact::Opening {
        let at_another_directory: i64 = store.conn.query_row("SELECT count(*) FROM upload_session_openings WHERE directory_local_item_id != ?1", [directory.as_i64()], |row| row.get(0)).unwrap();
        let fate = match (windows(store, &at).is_empty(), at_another_directory) {
            (false, _) => Fate::Answered,
            (true, 0) => Fate::Gone,
            (true, _) => Fate::Moved,
        };
        return (fate, wrong);
    }
    let is_answered = holds(&said(store, file), fact);
    if fact == Fact::UploadSession && !is_answered {
        let sessions_at_place = store.step_memory().list_empty_file_sessions(&at).unwrap();
        if sessions_at_place.contains(&EmptyFileSession::Continued { item: file, address: address(SESSION) }) {
            wrong.push("the reader of the sessions at a place answers it as the item's, which the item's memory does not".to_owned());
        }
    }
    begin_next_try(store, file, fact);
    let (is_still_answered, is_kept) = (holds(&said(store, file), fact), holds(&kept(store, file), fact));
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
    } else if !list_other_holders(store, file, fact).is_empty() {
        Fate::Moved
    } else if is_queued(store, fact) {
        Fate::Queued
    } else {
        Fate::Gone
    };
    (fate, wrong)
}

/// One cell: its fate as found, and what is wrong of it.
fn run_cell(operation: Operation, fact: Fact) -> (Fate, Vec<String>) {
    let mut scene = scene(shape_for(operation));
    prepare(&mut scene, operation);
    remember(&mut scene, fact);
    act(&mut scene, operation);
    find_fate(&mut scene, fact)
}

/// The enumeration: every fact against every operation, each cell as the table says. Run
/// with `--nocapture` it prints the table as found.
#[test]
fn every_fact_meets_every_operation_that_changes_what_it_was_learnt_for() {
    let mut failures = Vec::new();
    let mut check = |operation: Operation, facts: &[Fact], cells: &[Cell]| {
        let mut line = format!("{operation:?}:");
        for (fact, cell) in facts.iter().zip(cells) {
            let (fate, wrong) = run_cell(operation, *fact);
            line.push_str(&format!(" {fact:?}={fate:?}"));
            if fate != cell.fate() {
                failures.push(format!("{operation:?} x {fact:?}: {fate:?}, to be {:?}", cell.fate()));
            }
            failures.extend(wrong.into_iter().map(|what| format!("{operation:?} x {fact:?}: {what}")));
        }
        println!("{line}");
    };
    for operation in OPERATIONS_ON_A_SHOWN_FILE {
        check(operation, &FACTS_OF_A_SHOWN_FILE, &fates_of_a_shown_file(operation));
    }
    for operation in OPERATIONS_ON_A_REMOVED_FILE {
        check(operation, &FACTS_OF_A_REMOVED_FILE, &fates_of_a_removed_file(operation));
    }
    assert!(failures.is_empty(), "{} cells are not as the table says:\n{}", failures.len(), failures.join("\n"));
}
