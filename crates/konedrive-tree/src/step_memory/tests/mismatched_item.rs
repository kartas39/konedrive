//! The item an upload left in OneDrive with other content than was sent: found by its own
//! reference, kept with its drive, and void once the folder has that item.

use super::*;
use crate::item_store::tests::listed_drive::{listed_file, reference, store_with_listed_root, write_listing_of_changes};
use crate::item_store::StepCommitOutcome;
use crate::model::{DriveId, OneDriveItemId, OneDriveItemReference};
use crate::sync_plan::ItemVersions;

fn left(item: OneDriveItemReference) -> MismatchedItem {
    MismatchedItem::of_answer(item, Some("c-h-BAD"), None)
}

/// A plan asks of an item a listing brought whether an upload of this folder left it: it is
/// found by its reference with the file whose upload left it, with no file once that file's
/// row went, and not at all for an item of another drive, an item no upload left, or one
/// that was forgotten, wherever it was kept.
#[test]
fn the_item_an_upload_left_is_found_by_its_reference_with_its_file() {
    let mut store = store_with_listed_root();
    let (file, other) = (make_file(&mut store, ROOT, "report"), make_file(&mut store, ROOT, "other"));
    let in_another_drive = OneDriveItemReference { drive_id: DriveId::new("d2").unwrap(), item_id: OneDriveItemId::new("BAD") };
    assert_eq!(store.step_memory().remember_mismatched_item(file, &left(reference("BAD"))).unwrap(), LeftToRemove);
    assert_eq!(store.step_memory().remember_mismatched_item(other, &left(in_another_drive.clone())).unwrap(), LeftToRemove);
    let found = |store: &mut TreeStore, item: &OneDriveItemReference| store.step_memory().find_mismatched_item(item).unwrap();
    assert_eq!(found(&mut store, &reference("BAD")), Some(MismatchedItemUpload { mismatched_item: left(reference("BAD")), file: Some(file) }));
    assert_eq!(found(&mut store, &in_another_drive).map(|upload| upload.file), Some(Some(other)), "the same id in another drive is another item");
    assert_eq!(found(&mut store, &reference("X")), None);

    // A listing brings the item: a row has its binding, and the folder does not have it.
    write_listing_of_changes(&mut store, &[listed_file("BAD", "R", "report", (0, "h-BAD"), "e-BAD")]);
    assert_eq!(found(&mut store, &reference("BAD")).map(|upload| upload.file), Some(Some(file)), "what the plan learns before it shows the item");

    remove(&mut store, other);
    remove(&mut store, file);
    assert_eq!(found(&mut store, &in_another_drive), Some(MismatchedItemUpload { mismatched_item: left(in_another_drive.clone()), file: None }));
    assert_eq!(store.step_memory().list_mismatched_item_removals(10).unwrap(), [left(in_another_drive.clone()), left(reference("BAD"))], "each with its drive, the one left first first");
    store.step_memory().forget_mismatched_item(&in_another_drive).unwrap();
    assert_eq!((found(&mut store, &in_another_drive), store.step_memory().list_mismatched_item_removals(10).unwrap()), (None, vec![left(reference("BAD"))]));
}

/// An item the folder has is no item an upload left, whoever wrote the row: shown by the
/// plan, it is taken out of the file's memory and out of the removals, is not found by its
/// reference, is not given again, and does not come back when the folder ceases to show it.
#[test]
fn an_item_the_folder_has_is_never_left_to_be_removed() {
    let mut store = store_with_listed_root();
    let (file, gone) = (make_file(&mut store, ROOT, "report"), make_file(&mut store, ROOT, "gone"));
    assert_eq!(store.step_memory().remember_mismatched_item(file, &left(reference("BAD"))).unwrap(), LeftToRemove);
    assert_eq!(store.step_memory().remember_mismatched_item(gone, &left(reference("WORSE"))).unwrap(), LeftToRemove);
    remove(&mut store, gone);
    assert_eq!(store.step_memory().list_mismatched_item_removals(10).unwrap(), [left(reference("WORSE"))]);

    write_listing_of_changes(&mut store, &[listed_file("BAD", "R", "bad", (0, "h-BAD"), "e-BAD"), listed_file("WORSE", "R", "worse", (0, "h-W"), "e-W")]);
    for item_id in ["BAD", "WORSE"] {
        let id = store.item_store().find_bound_item(&reference(item_id)).unwrap().unwrap().local_item_id;
        let planned_from = ItemVersions::of(&item(&mut store, id));
        assert_eq!(store.item_store().commit_show_item(&planned_from, false).unwrap(), StepCommitOutcome::Written);
    }
    assert_eq!((kept(&store, file), store.step_memory().list_mismatched_item_removals(10).unwrap()), (ItemMemory::default(), vec![]));
    assert_eq!((count(&store, "mismatched_item_removals"), store.step_memory().find_mismatched_item(&reference("BAD")).unwrap()), (0, None));

    assert_eq!(store.step_memory().remember_mismatched_item(file, &left(reference("BAD"))).unwrap(), OfFolder, "the step is told, and does not ask its removal");
    assert_eq!(kept(&store, file), ItemMemory::default(), "nothing is kept of an item the folder has");
    let shown = store.item_store().find_bound_item(&reference("WORSE")).unwrap().unwrap().local_item_id;
    store.item_store().clear_local_and_synced_version(shown).unwrap();
    assert_eq!(store.step_memory().list_mismatched_item_removals(10).unwrap(), [], "void for good");
}

/// A second upload of the same file leaves another item with other content: the first is
/// in OneDrive still, and is left to remove by itself; it is not lost with the memory of
/// the file, which holds the later one.
#[test]
fn an_item_an_earlier_upload_of_the_file_left_is_left_to_remove_when_another_is_kept() {
    let mut store = store_with_listed_root();
    let file = make_file(&mut store, ROOT, "report");
    assert_eq!(store.step_memory().remember_mismatched_item(file, &left(reference("BAD"))).unwrap(), LeftToRemove);
    assert_eq!(store.step_memory().remember_mismatched_item(file, &left(reference("BAD"))).unwrap(), LeftToRemove);
    assert_eq!(store.step_memory().list_mismatched_item_removals(10).unwrap(), [], "the same item given again");
    assert_eq!(store.step_memory().remember_mismatched_item(file, &left(reference("BAD2"))).unwrap(), LeftToRemove);
    assert_eq!((kept(&store, file).mismatched_item, store.step_memory().list_mismatched_item_removals(10).unwrap()), (Some(left(reference("BAD2"))), vec![left(reference("BAD"))]));
    assert_eq!(store.step_memory().find_mismatched_item(&reference("BAD")).unwrap(), Some(MismatchedItemUpload { mismatched_item: left(reference("BAD")), file: None }));
}

/// The removal of the item an upload left is confirmed — OneDrive removed it, or does not
/// have it —: it is forgotten, and the row a listing made of it loses its OneDrive
/// version in the same transaction, stamped as a removal a step sent. A forgetting for
/// another cause leaves that row as the listing wrote it.
#[test]
fn a_leftover_whose_removal_is_confirmed_takes_the_onedrive_version_of_its_listed_row() {
    let mut store = store_with_listed_root();
    let (file, other) = (make_file(&mut store, ROOT, "report"), make_file(&mut store, ROOT, "other"));
    assert_eq!(store.step_memory().remember_mismatched_item(file, &left(reference("BAD"))).unwrap(), LeftToRemove);
    assert_eq!(store.step_memory().remember_mismatched_item(other, &left(reference("WORSE"))).unwrap(), LeftToRemove);
    write_listing_of_changes(&mut store, &[listed_file("BAD", "R", "report", (0, "h-BAD"), "e-BAD"), listed_file("WORSE", "R", "other", (0, "h-W"), "e-W")]);
    let listed = |store: &mut TreeStore, id: &str| store.item_store().find_bound_item(&reference(id)).unwrap().unwrap();
    assert!(listed(&mut store, "BAD").onedrive.is_some() && listed(&mut store, "WORSE").onedrive.is_some());
    let stamp_before = listed(&mut store, "BAD").confirmed_at_count;

    store.step_memory().forget_removed_mismatched_item(&reference("BAD")).unwrap();
    let row = listed(&mut store, "BAD");
    assert_eq!((row.onedrive, row.onedrive_etag, row.local, row.synced_place), (None, None, None, None), "the binding alone is left");
    assert!(row.confirmed_at_count > stamp_before, "stamped: a page fetched before the removal does not bring it back");
    assert_eq!((kept(&store, file), store.step_memory().find_mismatched_item(&reference("BAD")).unwrap()), (ItemMemory::default(), None));

    // Found changed by another: forgotten, and OneDrive has it still.
    store.step_memory().forget_mismatched_item(&reference("WORSE")).unwrap();
    assert!(listed(&mut store, "WORSE").onedrive.is_some());
    assert_eq!(kept(&store, other), ItemMemory::default());

    // Of an item no row is bound to, the forgetting is all there is.
    assert_eq!(store.step_memory().remember_mismatched_item(file, &left(reference("UNLISTED"))).unwrap(), LeftToRemove);
    store.step_memory().forget_removed_mismatched_item(&reference("UNLISTED")).unwrap();
    assert_eq!(kept(&store, file), ItemMemory::default());
}

/// A row the folder has is no leftover's: the confirmation of a removal writes nothing
/// of it.
#[test]
fn the_confirmed_removal_of_a_leftover_writes_nothing_of_an_item_the_folder_has() {
    let mut store = store_with_listed_root();
    write_listing_of_changes(&mut store, &[listed_file("OURS", "R", "ours", (0, "h"), "e-O")]);
    let id = store.item_store().find_bound_item(&reference("OURS")).unwrap().unwrap().local_item_id;
    let planned_from = ItemVersions::of(&item(&mut store, id));
    assert_eq!(store.item_store().commit_show_item(&planned_from, false).unwrap(), StepCommitOutcome::Written);
    let before = item(&mut store, id);

    store.step_memory().forget_removed_mismatched_item(&reference("OURS")).unwrap();
    assert_eq!(item(&mut store, id), before);
}

/// A file that comes to be left here — its own name is ignored from now on — cannot
/// remove what an upload of it left: no step of it runs. The leftover goes to the
/// daemon's removals at that write, and a file that is only renamed keeps it.
#[test]
fn a_file_that_comes_to_be_left_here_gives_its_leftover_to_the_daemon() {
    let mut store = store_with_listed_root();
    let (file, kept_file) = (make_file(&mut store, ROOT, "report"), make_file(&mut store, ROOT, "kept"));
    assert_eq!(store.step_memory().remember_mismatched_item(file, &left(reference("BAD"))).unwrap(), LeftToRemove);
    assert_eq!(store.step_memory().remember_mismatched_item(kept_file, &left(reference("KEPT"))).unwrap(), LeftToRemove);

    rename(&mut store, kept_file, "kept again");
    change_content(&mut store, kept_file);
    assert_eq!((kept(&store, kept_file).mismatched_item, store.step_memory().list_mismatched_item_removals(10).unwrap()), (Some(left(reference("KEPT"))), vec![]));

    // An editor's safe save: the file is renamed to a name the upload rules ignore.
    store.item_store().apply(&LocalVersionChange::Moved { item: file, place: place(ROOT, "report~"), is_name_ignored: true }).unwrap();
    assert_eq!((kept(&store, file).mismatched_item, store.step_memory().list_mismatched_item_removals(10).unwrap()), (None, vec![left(reference("BAD"))]));
    assert_eq!(store.step_memory().find_mismatched_item(&reference("BAD")).unwrap(), Some(MismatchedItemUpload { mismatched_item: left(reference("BAD")), file: None }));

    // Renamed again under an ignored name, it was left here before: nothing more happens.
    store.item_store().apply(&LocalVersionChange::Moved { item: file, place: place(ROOT, "report~~"), is_name_ignored: true }).unwrap();
    assert_eq!(store.step_memory().list_mismatched_item_removals(10).unwrap(), [left(reference("BAD"))]);
}

/// A file below a directory whose name comes to be ignored is left here with it.
#[test]
fn a_file_below_a_directory_that_comes_to_be_ignored_gives_its_leftover_to_the_daemon() {
    let mut store = store_with_listed_root();
    let directory = make_directory(&mut store, ROOT, "build");
    let file = make_file(&mut store, directory, "out");
    assert_eq!(store.step_memory().remember_mismatched_item(file, &left(reference("BAD"))).unwrap(), LeftToRemove);

    store.item_store().apply(&LocalVersionChange::Moved { item: directory, place: place(ROOT, "cache"), is_name_ignored: true }).unwrap();
    assert_eq!((kept(&store, file).mismatched_item, store.step_memory().list_mismatched_item_removals(10).unwrap()), (None, vec![left(reference("BAD"))]));
}

/// A file that gets a binding is no new file any more: the step that removes a leftover
/// is a new file's, so the leftover of another item goes to the daemon's removals.
#[test]
fn a_file_that_gets_a_binding_gives_its_leftover_to_the_daemon() {
    let mut store = store_with_listed_root();
    let file = make_file(&mut store, ROOT, "report");
    assert_eq!(store.step_memory().remember_mismatched_item(file, &left(reference("BAD"))).unwrap(), LeftToRemove);

    store.item_store().bind_item(file, &reference("GOOD")).unwrap();
    assert_eq!((kept(&store, file).mismatched_item, store.step_memory().list_mismatched_item_removals(10).unwrap()), (None, vec![left(reference("BAD"))]));
}

