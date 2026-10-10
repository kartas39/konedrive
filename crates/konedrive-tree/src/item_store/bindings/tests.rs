use crate::item_store::tests::{hashed, is_refused, is_refused_by_schema, item, make_directory, make_file, place, reference, synced};
use crate::item_store::stored::write_item;
use crate::model::{ContentPresence, FileSyncState, Item, ItemKind, LocalItemId, LocalVersionChange};
use super::take_over_binding;
use crate::{TreeError, TreeStore};

const ROOT: LocalItemId = LocalItemId::ROOT;

fn bound_to(store: &mut TreeStore, onedrive_id: &str) -> Option<LocalItemId> {
    store.item_store().find_bound_item(&reference(onedrive_id)).unwrap().map(|item| item.local_item_id)
}

/// `take_over_binding` in a transaction of its own, as `ItemStore` has the other two.
fn take_over(store: &mut TreeStore, replacing: LocalItemId, replaced: LocalItemId) -> Result<(), TreeError> {
    store.item_store().write(|tx| take_over_binding(tx, replacing, replaced))
}

fn remove(store: &mut TreeStore, id: LocalItemId) {
    store.item_store().apply(&LocalVersionChange::Removed { item: id }).unwrap();
}

/// `take_over_binding`: the replacing file takes the binding and the synced place and
/// content of the replaced one, which the folder does not show any more, and keeps its id
/// and its local version; the replaced row goes; the binding is one item's.
#[test]
fn take_over_binding_moves_the_binding_and_the_replaced_row_goes() {
    let mut store = TreeStore::in_memory().unwrap();
    let replaced = make_file(&mut store, ROOT, "report");
    let replaced_row = synced(&mut store, replaced, "A");
    remove(&mut store, replaced);
    let replacing = make_file(&mut store, ROOT, "copy");
    let before = item(&mut store, replacing);

    take_over(&mut store, replacing, replaced).unwrap();
    assert_eq!(
        item(&mut store, replacing),
        Item { onedrive_item: Some(reference("A")), synced_place: Some(place(ROOT, "report")), synced_content: Some(hashed("A")), ..before },
        "its id, its local version and its presence stay"
    );
    assert_eq!(replaced_row.synced_content, Some(hashed("A")));
    assert_eq!(store.item_store().find_item(replaced).unwrap(), None);
    assert_eq!(bound_to(&mut store, "A"), Some(replacing));
    assert_eq!(item(&mut store, replacing).file_sync_state(), Some(FileSyncState::ModifiedHere));
}

/// What `take_over_binding` refuses: a replaced item the folder still shows (its file
/// stands at its name, and its row would go from under it), an item that has a binding
/// already (it would lose one), a replacing item the folder does not show, a replaced item
/// with no binding, an item over itself, the root, and an id with no row.
#[test]
fn take_over_binding_refuses_what_would_lose_a_binding_or_a_shown_item() {
    let mut store = TreeStore::in_memory().unwrap();
    let (a, b, local, shown) = (make_file(&mut store, ROOT, "a"), make_file(&mut store, ROOT, "b"), make_file(&mut store, ROOT, "local"), make_file(&mut store, ROOT, "shown"));
    for (id, onedrive_id) in [(a, "A"), (b, "B"), (shown, "S"), (ROOT, "R")] {
        synced(&mut store, id, onedrive_id);
    }
    remove(&mut store, a);
    remove(&mut store, b);
    let other_local = make_file(&mut store, ROOT, "other-local");
    for (replacing, replaced) in [(local, shown), (shown, a), (b, a), (local, other_local), (local, local), (local, ROOT), (ROOT, a), (local, LocalItemId::new(99)), (LocalItemId::new(99), a)] {
        assert!(is_refused(take_over(&mut store, replacing, replaced)), "{replacing} over {replaced}");
    }
    assert_eq!([bound_to(&mut store, "A"), bound_to(&mut store, "B"), bound_to(&mut store, "S")], [Some(a), Some(b), Some(shown)]);
}

/// `give_up_binding`: the item keeps its id, its place and its content and is an item
/// OneDrive does not have; a new row takes the binding, and nothing else: the folder does
/// not show it, and nothing was agreed on for it.
#[test]
fn give_up_binding_leaves_the_item_local_and_a_new_row_takes_the_binding() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    synced(&mut store, file, "A");
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: file }).unwrap();
    let before = item(&mut store, file);

    let taker = store.item_store().give_up_binding(file).unwrap();
    assert_ne!(taker, file);
    assert_eq!(
        item(&mut store, file),
        Item { onedrive_item: None, synced_place: None, synced_content: None, ..before },
        "a program writing it goes on writing the same item"
    );
    assert_eq!(item(&mut store, file).file_sync_state(), Some(FileSyncState::LocalOnly));
    assert_eq!(
        item(&mut store, taker),
        Item {
            local_item_id: taker,
            onedrive_item: Some(reference("A")),
            kind: ItemKind::File,
            local: None,
            synced_place: None,
            synced_content: None,
            content_presence: ContentPresence::NotOnDisk,
            content_before_write: None,
            is_ignored: false,
        }
    );
    assert_eq!(bound_to(&mut store, "A"), Some(taker));
}

/// A directory that gives up its binding keeps what the folder shows in it; what the two
/// sides agreed is in that directory of OneDrive is in the row that took the binding.
#[test]
fn give_up_binding_of_a_directory_keeps_its_content_and_hands_on_what_was_agreed() {
    let mut store = TreeStore::in_memory().unwrap();
    let dir = make_directory(&mut store, ROOT, "dir");
    let (sent, own) = (make_file(&mut store, dir, "sent"), make_file(&mut store, dir, "own"));
    synced(&mut store, dir, "D");
    synced(&mut store, sent, "S");

    let taker = store.item_store().give_up_binding(dir).unwrap();
    for (id, name) in [(sent, "sent"), (own, "own")] {
        assert_eq!(item(&mut store, id).local.unwrap().place, Some(place(dir, name)), "the folder shows it where it was");
    }
    assert_eq!(item(&mut store, sent).synced_place, Some(place(taker, "sent")), "OneDrive has it in the directory of that binding");
    assert_eq!(item(&mut store, own).synced_place, None);
}

/// What `give_up_binding` refuses: an item with no binding, the root, and an id with no row.
#[test]
fn give_up_binding_refuses_an_item_with_none_and_the_root() {
    let mut store = TreeStore::in_memory().unwrap();
    let local = make_file(&mut store, ROOT, "local");
    synced(&mut store, ROOT, "R");
    let mut items = store.item_store();
    for id in [local, ROOT, LocalItemId::new(99)] {
        assert!(is_refused(items.give_up_binding(id)), "{id}");
    }
    assert_eq!(bound_to(&mut store, "R"), Some(ROOT));
}

/// `merge_items`: a new item here and a new item in OneDrive are one. The local row takes
/// the binding and what was agreed for the other, and keeps its id and its local version;
/// the other row goes, and what it had below it is below the local row.
#[test]
fn merge_items_gives_the_local_row_the_binding_and_the_other_row_goes() {
    let mut store = TreeStore::in_memory().unwrap();
    let local = make_directory(&mut store, ROOT, "Photos");
    let before = item(&mut store, local);
    // The same directory as a listing brought it, with a file in it, neither shown yet.
    let listed = make_directory(&mut store, ROOT, "listed");
    let listed_file = make_file(&mut store, listed, "a.jpg");
    synced(&mut store, listed_file, "F");
    remove(&mut store, listed_file);
    let mut listed_row = synced(&mut store, listed, "P");
    listed_row.local = None;
    listed_row.synced_place = Some(place(ROOT, "Photos"));
    write_item(&store.conn, &listed_row).unwrap();

    store.item_store().merge_items(local, listed).unwrap();
    assert_eq!(item(&mut store, local), Item { onedrive_item: Some(reference("P")), synced_place: Some(place(ROOT, "Photos")), ..before });
    assert_eq!(store.item_store().find_item(listed).unwrap(), None);
    assert_eq!(bound_to(&mut store, "P"), Some(local));
    assert_eq!(item(&mut store, listed_file).synced_place, Some(place(local, "a.jpg")));
}

/// What `merge_items` refuses: a local item that has a binding, another that has none, one
/// the folder shows too (they are two items then), an item with itself, the root, and an id
/// with no row.
#[test]
fn merge_items_refuses_two_items_that_are_not_one() {
    let mut store = TreeStore::in_memory().unwrap();
    let (local, other_local) = (make_file(&mut store, ROOT, "local"), make_file(&mut store, ROOT, "other-local"));
    let (shown, bound, gone) = (make_file(&mut store, ROOT, "shown"), make_file(&mut store, ROOT, "bound"), make_file(&mut store, ROOT, "gone"));
    for (id, onedrive_id) in [(shown, "S"), (bound, "B"), (gone, "G")] {
        synced(&mut store, id, onedrive_id);
    }
    synced(&mut store, ROOT, "R");
    remove(&mut store, gone);
    let mut items = store.item_store();
    for (first, second) in [(bound, gone), (local, other_local), (local, shown), (local, local), (local, ROOT), (ROOT, gone), (local, LocalItemId::new(99)), (LocalItemId::new(99), gone)] {
        assert!(is_refused(items.merge_items(first, second)), "{first} with {second}");
    }
    assert_eq!([bound_to(&mut store, "S"), bound_to(&mut store, "B"), bound_to(&mut store, "G")], [Some(shown), Some(bound), Some(gone)]);
}

/// A binding stays with an item of its kind: a directory does not take over a file's, a
/// file's is not merged into a directory, and the new row of a binding given up is of the
/// giver's kind. An id of OneDrive never changes its kind.
#[test]
fn a_binding_does_not_move_between_a_file_and_a_directory() {
    let mut store = TreeStore::in_memory().unwrap();
    let (file, directory) = (make_file(&mut store, ROOT, "file"), make_directory(&mut store, ROOT, "directory"));
    synced(&mut store, file, "F");
    synced(&mut store, directory, "D");
    remove(&mut store, file);
    remove(&mut store, directory);
    let (new_file, new_directory) = (make_file(&mut store, ROOT, "new-file"), make_directory(&mut store, ROOT, "new-directory"));
    assert!(is_refused(take_over(&mut store, new_directory, file)), "a directory over a file");
    assert!(is_refused(take_over(&mut store, new_file, directory)), "a file over a directory");
    assert!(is_refused(take_over(&mut store, new_directory, directory)), "a directory over a directory: only a file's binding is taken over");
    let mut items = store.item_store();
    assert!(is_refused(items.merge_items(new_file, directory)), "a file with a directory");
    assert!(is_refused(items.merge_items(new_directory, file)), "a directory with a file");
    assert_eq!([bound_to(&mut store, "F"), bound_to(&mut store, "D")], [Some(file), Some(directory)]);

    store.item_store().merge_items(new_directory, directory).unwrap();
    let taker = store.item_store().give_up_binding(new_directory).unwrap();
    assert_eq!(item(&mut store, taker).kind, ItemKind::Directory, "the row that took the binding is what the giver is");
}

/// A binding is one item's, whoever writes the rows: the schema's unique index refuses a
/// second row with the same drive and item id.
#[test]
fn the_schema_refuses_a_second_row_with_one_binding() {
    let mut store = TreeStore::in_memory().unwrap();
    let (a, b) = (make_file(&mut store, ROOT, "a"), make_file(&mut store, ROOT, "b"));
    synced(&mut store, a, "A");
    let mut second = item(&mut store, b);
    second.onedrive_item = Some(reference("A"));
    assert!(is_refused_by_schema(write_item(&store.conn, &second)));
    assert_eq!(bound_to(&mut store, "A"), Some(a));
}
