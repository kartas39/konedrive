use crate::item_store::tests::{is_refused, item, make_directory, make_file, place, synced};
use crate::model::{ContentPresence, Item, ItemVersion, LocalItemId, LocalVersionChange};
use crate::TreeStore;

const ROOT: LocalItemId = LocalItemId::ROOT;

/// What OneDrive has of `id`, as a listing would have written it: here the local version.
fn listed(store: &mut TreeStore, id: LocalItemId) -> Item {
    let mut row = item(store, id);
    row.onedrive = row.local.clone();
    row.onedrive_etag = Some(format!("e-{id}"));
    crate::item_store::item_row::write_item(&store.conn, &row).unwrap();
    item(store, id)
}

/// The local version and what was agreed on go; the binding and what is OneDrive's stay. The
/// item was moved and opened for writing, both unsent: its stamp of being unsent goes too,
/// since nothing of it differs from what was agreed on any more.
#[test]
fn an_item_of_onedrive_loses_its_local_version_and_what_was_agreed_on_and_keeps_what_is_onedrives() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "file");
    synced(&mut store, file, "F");
    let before = listed(&mut store, file);
    store.item_store().apply(&LocalVersionChange::Moved { item: file, place: place(ROOT, "renamed"), is_name_ignored: false }).unwrap();
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: file }).unwrap();
    let unsent = item(&mut store, file);
    assert!(unsent.unsent_since.is_some() && unsent.content_before_write.is_some());

    store.item_store().clear_local_and_synced_version(file).unwrap();
    let expected = Item { local: None, synced_place: None, synced_content: None, content_presence: ContentPresence::NotOnDisk, content_before_write: None, unsent_since: None, ..before.clone() };
    assert_eq!(item(&mut store, file), expected);
    assert_eq!((expected.onedrive_item, expected.onedrive, expected.onedrive_etag), (before.onedrive_item, before.onedrive, before.onedrive_etag));
    assert_eq!(store.item_store().find_item_at(ROOT, "renamed".as_ref()).unwrap(), None, "its place is free");
    assert_eq!(store.item_store().list_unsent(None, 10).unwrap(), [], "nothing of it is unsent");
}

/// A directory goes after what the folder shows in it: each of those first, deepest first.
/// What was agreed to be in it still names its row.
#[test]
fn a_directory_loses_its_local_version_after_what_is_in_it() {
    let mut store = TreeStore::in_memory().unwrap();
    let dir = make_directory(&mut store, ROOT, "dir");
    let sub = make_directory(&mut store, dir, "sub");
    let file = make_file(&mut store, sub, "file");
    for (id, onedrive_id) in [(dir, "D"), (sub, "S"), (file, "F")] {
        synced(&mut store, id, onedrive_id);
        listed(&mut store, id);
    }
    assert!(is_refused(store.item_store().clear_local_and_synced_version(dir)), "a directory that holds something");
    assert!(is_refused(store.item_store().clear_local_and_synced_version(sub)));
    for id in [file, sub, dir] {
        store.item_store().clear_local_and_synced_version(id).unwrap();
    }
    for id in [file, sub, dir] {
        let row = item(&mut store, id);
        assert_eq!((row.local, row.synced_place, row.synced_content, row.unsent_since), (None::<ItemVersion>, None, None, None), "{id}");
        assert!(row.onedrive_item.is_some() && row.onedrive.is_some(), "{id}");
    }
    assert_eq!(store.item_store().list_children(ROOT).unwrap(), []);
}

/// The root, an id with no row, an item with no binding and an item the folder does not show
/// are refused, and nothing is written.
#[test]
fn what_has_no_binding_or_is_not_shown_does_not_lose_a_local_version_so() {
    let mut store = TreeStore::in_memory().unwrap();
    let own = make_file(&mut store, ROOT, "own");
    let removed = make_file(&mut store, ROOT, "removed");
    synced(&mut store, removed, "R");
    store.item_store().apply(&LocalVersionChange::Removed { item: removed }).unwrap();
    let before = store.item_store().list_items().unwrap();
    assert!(is_refused(store.item_store().clear_local_and_synced_version(ROOT)));
    assert!(is_refused(store.item_store().clear_local_and_synced_version(LocalItemId::new(99))));
    assert!(is_refused(store.item_store().clear_local_and_synced_version(own)), "its row would say nothing");
    assert!(is_refused(store.item_store().clear_local_and_synced_version(removed)), "removed here: its removal is unsent, and stays so");
    assert_eq!(store.item_store().list_items().unwrap(), before);
}
