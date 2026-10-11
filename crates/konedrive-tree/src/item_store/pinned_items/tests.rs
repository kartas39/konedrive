use crate::item_store::tests::listed_drive::{listed_directory, listed_file, store_with_listed_root, write_listing_of_changes};
use crate::model::{ContentPresence, ContentVersion, ItemKind, ItemPlace, LocalItemId, LocalVersionChange};
use crate::sync_plan::ItemVersions;
use crate::{StepCommitOutcome, TreeStore};

const ROOT: LocalItemId = LocalItemId::ROOT;

/// The item `item_id` of OneDrive, listed already, is shown by the folder: its local item id.
fn show(store: &mut TreeStore, item_id: &str) -> LocalItemId {
    let row = store.item_store().find_bound_item(&crate::item_store::tests::listed_drive::reference(item_id)).unwrap().unwrap();
    assert_eq!(store.item_store().commit_show_item(&ItemVersions::of(&row), false).unwrap(), StepCommitOutcome::Written);
    row.local_item_id
}

/// The directory `D` with the file `A` and the directory `E`, which has the file `B`; the
/// file `C` in the root; all shown and agreed on, no content on disk. Their local item ids,
/// in that order.
fn store_with_shown_tree() -> (TreeStore, [LocalItemId; 5]) {
    let mut store = store_with_listed_root();
    write_listing_of_changes(
        &mut store,
        &[listed_directory("D", "R", "d", "e-D"), listed_file("A", "D", "a.txt", (3, "h-A"), "e-A"), listed_directory("E", "D", "e", "e-E"), listed_file("B", "E", "b.txt", (3, "h-B"), "e-B"), listed_file("C", "R", "c.txt", (3, "h-C"), "e-C")],
    );
    let ids = ["D", "A", "E", "B", "C"].map(|item_id| show(&mut store, item_id));
    (store, ids)
}

#[test]
fn a_pin_holds_for_the_item_it_is_on_and_for_all_the_folder_shows_below_it() {
    let (mut store, [directory, in_directory, inner, in_inner, in_root]) = store_with_shown_tree();
    assert!(!store.item_store().is_pinned(in_inner).unwrap());
    store.set_pin("D", true).unwrap();
    for held in [directory, in_directory, inner, in_inner] {
        assert!(store.item_store().is_pinned(held).unwrap(), "{held}");
    }
    assert!(!store.item_store().is_pinned(in_root).unwrap());
    assert!(!store.item_store().is_pinned(ROOT).unwrap());
    assert!(!store.item_store().is_pinned(LocalItemId::new(99)).unwrap(), "no row");

    // A file the user made in the pinned directory, which OneDrive does not have yet.
    let content = Some(ContentVersion::ChangedHere { size: 1, mtime: 1, mtime_nsec: 0, change_count: 1 });
    let made = store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::File, place: ItemPlace { parent_local_item_id: inner, name: "new".into() }, content, is_name_ignored: false, replaced: None }).unwrap();
    assert!(store.item_store().is_pinned(made).unwrap());

    // Moved out of it here, the pin holds for it no more; the pin on a file is its own.
    store.item_store().apply(&LocalVersionChange::Moved { item: in_inner, place: ItemPlace { parent_local_item_id: ROOT, name: "b.txt".into() }, is_name_ignored: false }).unwrap();
    assert!(!store.item_store().is_pinned(in_inner).unwrap());
    store.set_pin("C", true).unwrap();
    assert!(store.item_store().is_pinned(in_root).unwrap());
}

#[test]
fn the_pinned_files_to_download_are_those_not_on_disk_that_agree_on_everything() {
    let (mut store, [_, in_directory, _, in_inner, in_root]) = store_with_shown_tree();
    assert_eq!(store.item_store().list_pinned_files_to_download(None, 10).unwrap(), vec![]);
    store.set_pin("D", true).unwrap();
    let ids = |store: &mut TreeStore, after: Option<LocalItemId>, limit: usize| -> Vec<LocalItemId> { store.item_store().list_pinned_files_to_download(after, limit).unwrap().iter().map(|item| item.local_item_id).collect() };
    assert_eq!(ids(&mut store, None, 10), vec![in_directory, in_inner], "the files below the pin, no directory");
    assert_eq!(ids(&mut store, None, 1), vec![in_directory]);
    assert_eq!(ids(&mut store, Some(in_directory), 10), vec![in_inner]);

    // On disk already; changed in OneDrive; moved here: none of them is only to download.
    store.item_store().set_content_presence(in_directory, ContentPresence::OnDisk).unwrap();
    assert_eq!(ids(&mut store, None, 10), vec![in_inner]);
    write_listing_of_changes(&mut store, &[listed_file("B", "E", "b.txt", (4, "h-B2"), "e-B2")]);
    assert_eq!(ids(&mut store, None, 10), vec![]);
    store.set_pin("C", true).unwrap();
    assert_eq!(ids(&mut store, None, 10), vec![in_root]);
    store.item_store().apply(&LocalVersionChange::Moved { item: in_root, place: ItemPlace { parent_local_item_id: ROOT, name: "renamed".into() }, is_name_ignored: false }).unwrap();
    assert_eq!(ids(&mut store, None, 10), vec![]);
}

#[test]
fn whether_an_item_is_pinned_is_read_from_the_rows_above_it_and_not_from_every_row() {
    let (store, [.., in_inner, _]) = store_with_shown_tree();
    let plan: Vec<String> = store
        .conn
        .prepare(&format!("EXPLAIN QUERY PLAN {}", crate::item_store::item_row::is_pinned_sql()))
        .unwrap()
        .query_map([in_inner.as_i64()], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!plan.iter().any(|step| step.starts_with("SCAN items")), "{plan:#?}");
}
