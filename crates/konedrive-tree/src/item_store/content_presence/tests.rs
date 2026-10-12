use crate::item_store::tests::{is_refused, item, make_directory, make_file, synced};
use crate::model::{ContentPresence, Item, LocalItemId, LocalVersionChange};
use crate::TreeStore;

const ROOT: LocalItemId = LocalItemId::ROOT;

/// The presence is written alone: the place, the content, the binding and what was agreed on
/// stay. A directory, an item the folder does not show and an id with no row have no content
/// to be on disk.
#[test]
fn the_content_presence_of_a_file_the_folder_shows_is_written_and_nothing_else_of_its_row() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "file");
    let before = synced(&mut store, file, "A");
    assert_eq!(before.content_presence, ContentPresence::OnDisk);

    store.item_store().set_content_presence(file, ContentPresence::NotOnDisk).unwrap();
    assert_eq!(item(&mut store, file), Item { content_presence: ContentPresence::NotOnDisk, ..before.clone() });
    store.item_store().set_content_presence(file, ContentPresence::OnDisk).unwrap();
    assert_eq!(item(&mut store, file), before);

    let directory = make_directory(&mut store, ROOT, "directory");
    assert!(is_refused(store.item_store().set_content_presence(directory, ContentPresence::OnDisk)));
    assert!(is_refused(store.item_store().set_content_presence(LocalItemId::new(99), ContentPresence::OnDisk)));
    store.item_store().apply(&LocalVersionChange::Removed { item: file }).unwrap();
    assert!(is_refused(store.item_store().set_content_presence(file, ContentPresence::OnDisk)));
    assert_eq!(item(&mut store, file).content_presence, ContentPresence::NotOnDisk);
}
