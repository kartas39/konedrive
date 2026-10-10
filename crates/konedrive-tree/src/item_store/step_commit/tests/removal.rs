//! The commit of a removal that was sent: which row a file's is written to, and what the
//! removal of a directory asks of the rows below it.

use super::*;

/// The second review's S4: a file is made at the place a file was removed from, while that
/// removal is being sent, and takes its binding. What OneDrive confirmed — the item is
/// gone — is written to the row that has the binding now: the made file gives the binding
/// up to a row that is left with it alone, stamped, and is the user's own, sent as a new
/// item by the next plan. Nothing says OneDrive has what it confirmed it removed.
#[test]
fn the_removal_of_a_file_is_confirmed_of_the_row_that_has_the_binding_now() {
    let mut store = store_with_listed_root();
    let file = show_file(&mut store, "G", "g");
    store.item_store().apply(&LocalVersionChange::Removed { item: file }).unwrap();
    let planned_from = versions(&mut store, file);
    let content = Some(ContentVersion::ChangedHere { size: 9, mtime: 400, mtime_nsec: 1, change_count: 1 });
    let made = store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::File, place: place(ROOT, "g"), content: content.clone(), is_name_ignored: false, replaced: Some(file) }).unwrap();
    assert_eq!((store.item_store().find_item(file).unwrap(), bound(&mut store, "G").local_item_id), (None, made), "the made file has the binding");

    assert_eq!(store.item_store().commit_send_file_removal(&planned_from).unwrap(), StepCommitOutcome::Written);
    let removed = bound(&mut store, "G");
    assert!(removed.local_item_id != made && is_binding_alone(&removed) && removed.confirmed_at_count > 0, "{removed:?}");
    let made = row(&mut store, made);
    assert_eq!((made.onedrive_item.clone(), made.onedrive.clone(), made.synced_place.clone(), made.synced_content.clone()), (None, None, None, None), "the made file is an item OneDrive does not have");
    assert_eq!(made.local, Some(ItemVersion { place: Some(place(ROOT, "g")), content }));
    assert_eq!(find_broken_rule(&store.item_store().list_items().unwrap(), false), None);
    assert_eq!(store.item_store().find_sent_removal_window().unwrap().removal_count, 1);

    // The same commit again finds OneDrive's side changed, whichever row has the binding.
    assert!(is_left_unwritten(&mut store, |items| items.commit_send_file_removal(&planned_from)));
}
