use crate::item_store::tests::{hashed, is_refused, is_refused_by_schema, item, make_directory, make_file, place, reference, synced};
use crate::item_store::item_row::write_item;
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

    let taker = store.item_store().give_up_binding(file, false).unwrap();
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
            onedrive: None,
            onedrive_etag: None,
            onedrive_mime: None,
            synced_place: None,
            synced_content: None,
            content_presence: ContentPresence::NotOnDisk,
            content_before_write: None,
            placement: crate::model::Placement::Placed,
            is_name_ignored: false,
            confirmed_at_count: 0,
            unsent_since: None,
            is_executable: false,
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

    let taker = store.item_store().give_up_binding(dir, false).unwrap();
    for (id, name) in [(sent, "sent"), (own, "own")] {
        assert_eq!(item(&mut store, id).local.unwrap().place, Some(place(dir, name)), "the folder shows it where it was");
    }
    assert_eq!(item(&mut store, sent).synced_place, Some(place(taker, "sent")), "OneDrive has it in the directory of that binding");
    assert_eq!(item(&mut store, own).synced_place, None);
}

/// An item that gives its binding up is an item with no binding that has a name: what the
/// upload rules say of that name is written then, as the caller says it, whatever the row
/// said while OneDrive had the item. The row that takes the binding has no name.
#[test]
fn give_up_binding_writes_what_the_upload_rules_say_of_the_givers_name() {
    let mut store = TreeStore::in_memory().unwrap();
    let (cache, plain) = (make_directory(&mut store, ROOT, ".cache"), make_directory(&mut store, ROOT, "plain"));
    let inside = make_file(&mut store, cache, "inside");
    synced(&mut store, cache, "C");
    synced(&mut store, plain, "P");
    let mut plain_row = item(&mut store, plain);
    plain_row.is_name_ignored = true;
    write_item(&store.conn, &plain_row).unwrap();

    let cache_taker = store.item_store().give_up_binding(cache, true).unwrap();
    let plain_taker = store.item_store().give_up_binding(plain, false).unwrap();
    assert_eq!([cache, plain, cache_taker, plain_taker].map(|id| item(&mut store, id).is_name_ignored), [true, false, false, false]);
    assert!(store.item_store().is_ignored(inside).unwrap(), "what is in a directory OneDrive does not have any more, whose name is ignored");
}

/// What `give_up_binding` refuses: an item with no binding, the root, an id with no row,
/// and an item the folder does not show: it would be left with no binding, no local
/// version and nothing agreed on, a row that no operation ever takes away.
#[test]
fn give_up_binding_refuses_an_item_with_none_the_root_and_one_not_shown() {
    let mut store = TreeStore::in_memory().unwrap();
    let local = make_file(&mut store, ROOT, "local");
    let removed = make_file(&mut store, ROOT, "removed");
    let removed_row = synced(&mut store, removed, "G");
    remove(&mut store, removed);
    synced(&mut store, ROOT, "R");
    let mut items = store.item_store();
    for id in [local, removed, ROOT, LocalItemId::new(99)] {
        assert!(is_refused(items.give_up_binding(id, false)), "{id}");
    }
    assert_eq!(bound_to(&mut store, "R"), Some(ROOT));
    assert_eq!(item(&mut store, removed).synced_place, removed_row.synced_place, "the removed row is as it was");
    assert_eq!(bound_to(&mut store, "G"), Some(removed));
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
    assert!(before.unsent_since.is_some());
    assert_eq!(item(&mut store, local), Item { onedrive_item: Some(reference("P")), synced_place: Some(place(ROOT, "Photos")), unsent_since: None, ..before }, "a directory where the two sides agree it is: nothing of it is unsent");
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
    let taker = store.item_store().give_up_binding(new_directory, false).unwrap();
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

/// `bind_item`: an item of the folder that OneDrive did not have gets its binding when
/// OneDrive confirms it as a new item. It keeps its id and everything else.
#[test]
fn bind_item_gives_an_item_of_the_folder_its_first_binding() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "new");
    let before = item(&mut store, file);
    store.item_store().bind_item(file, &reference("N")).unwrap();
    assert_eq!(item(&mut store, file), Item { onedrive_item: Some(reference("N")), ..before });
    assert_eq!(bound_to(&mut store, "N"), Some(file));
}

/// A daemon that died between marking an uploaded file and its commit finds, when it runs
/// again, the item OneDrive has under that reference as a row of its own that the folder
/// does not show. Binding the item of the folder to the reference merges that row into it:
/// the item keeps its id and takes what was agreed on, and the other row goes.
#[test]
fn bind_item_merges_the_row_the_folder_does_not_show_that_has_the_reference() {
    let mut store = TreeStore::in_memory().unwrap();
    let listed = make_file(&mut store, ROOT, "listed");
    let listed_row = synced(&mut store, listed, "N");
    remove(&mut store, listed);
    let uploaded = make_file(&mut store, ROOT, "uploaded");
    let before = item(&mut store, uploaded);

    store.item_store().bind_item(uploaded, &reference("N")).unwrap();
    assert_eq!(item(&mut store, uploaded), Item { onedrive_item: Some(reference("N")), synced_place: listed_row.synced_place, synced_content: listed_row.synced_content, ..before });
    assert_eq!(store.item_store().find_item(listed).unwrap(), None);
    assert_eq!(bound_to(&mut store, "N"), Some(uploaded));
}

/// An item is bound, gives its binding up and is bound to the same reference again: the
/// row that took the binding, which the folder does not show, is merged back into the item
/// and goes, with what was agreed on for it since.
#[test]
fn an_item_bound_again_after_giving_its_binding_up_takes_the_row_of_the_binding_back() {
    let mut store = TreeStore::in_memory().unwrap();
    let directory = make_directory(&mut store, ROOT, "directory");
    let child = make_file(&mut store, directory, "child");
    store.item_store().bind_item(directory, &reference("D")).unwrap();
    synced(&mut store, child, "C");
    let bound = item(&mut store, directory);

    let taker = store.item_store().give_up_binding(directory, false).unwrap();
    assert_eq!((bound_to(&mut store, "D"), item(&mut store, child).synced_place), (Some(taker), Some(place(taker, "child"))));
    store.item_store().bind_item(directory, &reference("D")).unwrap();
    assert_eq!(item(&mut store, directory), bound);
    assert_eq!(store.item_store().find_item(taker).unwrap(), None);
    assert_eq!((bound_to(&mut store, "D"), item(&mut store, child).synced_place), (Some(directory), Some(place(directory, "child"))));
}

/// What `bind_item` refuses, and never by the index of bindings: a reference that is an
/// item's the folder shows, a reference of an item of another kind, an item that has a
/// binding, one the folder does not show, the root, and an id with no row.
#[test]
fn bind_item_refuses_a_reference_of_a_shown_item_and_an_item_that_has_a_binding() {
    let mut store = TreeStore::in_memory().unwrap();
    let (local, shown, gone) = (make_file(&mut store, ROOT, "local"), make_file(&mut store, ROOT, "shown"), make_file(&mut store, ROOT, "gone"));
    let directory = make_directory(&mut store, ROOT, "directory");
    for (id, onedrive_id) in [(shown, "S"), (gone, "G"), (directory, "D")] {
        synced(&mut store, id, onedrive_id);
    }
    remove(&mut store, gone);
    remove(&mut store, directory);
    let mut items = store.item_store();
    for (id, onedrive_id) in [(local, "S"), (local, "D"), (shown, "X"), (gone, "X"), (ROOT, "X"), (LocalItemId::new(99), "X")] {
        assert!(is_refused(items.bind_item(id, &reference(onedrive_id))), "{id} to {onedrive_id}");
    }
    assert_eq!([bound_to(&mut store, "S"), bound_to(&mut store, "G"), bound_to(&mut store, "D"), bound_to(&mut store, "X")], [Some(shown), Some(gone), Some(directory), None]);
}

/// `make_onedrive_item`: a row for an item of OneDrive the client did not know, as at a
/// parent's first mention in a listing: the binding and the kind, and nothing else. A
/// reference that is an item's already is refused.
#[test]
fn make_onedrive_item_makes_a_row_with_the_binding_and_the_kind_alone() {
    let mut store = TreeStore::in_memory().unwrap();
    let parent = store.item_store().make_onedrive_item(&reference("P"), ItemKind::Directory).unwrap();
    assert_eq!(
        item(&mut store, parent),
        Item {
            local_item_id: parent,
            onedrive_item: Some(reference("P")),
            kind: ItemKind::Directory,
            local: None,
            onedrive: None,
            onedrive_etag: None,
            onedrive_mime: None,
            synced_place: None,
            synced_content: None,
            content_presence: ContentPresence::NotOnDisk,
            content_before_write: None,
            placement: crate::model::Placement::Placed,
            is_name_ignored: false,
            confirmed_at_count: 0,
            unsent_since: None,
            is_executable: false,
        }
    );
    let file = store.item_store().make_onedrive_item(&reference("F"), ItemKind::File).unwrap();
    assert_eq!((item(&mut store, file).kind, item(&mut store, file).file_sync_state()), (ItemKind::File, None));
    assert!(is_refused(store.item_store().make_onedrive_item(&reference("P"), ItemKind::Directory)));
    assert_eq!(bound_to(&mut store, "P"), Some(parent));
}

/// An item id of OneDrive is never the empty text: the code refuses a binding with one
/// wherever a binding is written, before the schema would, and the root is not bound to a
/// drive's root whose id the store has as the empty text.
#[test]
fn a_binding_with_an_empty_item_id_is_refused() {
    let mut store = TreeStore::in_memory().unwrap();
    let local = make_file(&mut store, ROOT, "local");
    assert!(is_refused(store.item_store().bind_item(local, &reference(""))));
    assert!(is_refused(store.item_store().make_onedrive_item(&reference(""), ItemKind::Directory)));
    assert_eq!((item(&mut store, local).onedrive_item, bound_to(&mut store, "")), (None, None));

    store.set_meta(crate::meta::ROOT_ITEM_ID, Some("")).unwrap();
    store.set_drive_id("d1").unwrap();
    assert_eq!(item(&mut store, ROOT).onedrive_item, None);
}

/// `id`, which is bound, as OneDrive has it at `at` with the eTag `etag`, stamped and seen
/// by a full listing: every column of the OneDrive side set to something of its own.
fn with_onedrive_version(store: &mut TreeStore, id: LocalItemId, at: crate::model::ItemPlace, etag: &str) -> Item {
    let mut row = item(store, id);
    let content = (row.kind == ItemKind::File).then(|| hashed(etag));
    row.onedrive = Some(crate::model::ItemVersion { place: Some(at), content });
    row.onedrive_etag = Some(etag.to_owned());
    row.placement = crate::model::Placement::Skipped(crate::model::SkipReason::NameTooLong);
    row.confirmed_at_count = 7;
    write_item(&store.conn, &row).unwrap();
    crate::item_store::item_row::set_seen_in_full_listing(&store.conn, id, 3).unwrap();
    row
}

fn seen_in_full_listing(store: &TreeStore, id: LocalItemId) -> u64 {
    crate::item_store::item_row::find_seen_in_full_listing(&store.conn, id).unwrap()
}

/// The binding is moved with all that is OneDrive's: the item that takes a binding takes
/// the OneDrive version, its eTag, the placement, the stamp of the confirmation and the
/// full listing that saw the item, and what OneDrive has in the directory is in the row
/// that has the directory's binding. A merge of two directories shows all of it.
#[test]
fn an_item_that_takes_a_binding_takes_what_onedrive_has_under_it() {
    let mut store = TreeStore::in_memory().unwrap();
    let listed = store.item_store().make_onedrive_item(&reference("P"), ItemKind::Directory).unwrap();
    let listed_row = with_onedrive_version(&mut store, listed, place(ROOT, "Photos"), "e-P");
    let child = store.item_store().make_onedrive_item(&reference("C"), ItemKind::File).unwrap();
    with_onedrive_version(&mut store, child, place(listed, "child"), "e-C");
    let local = make_directory(&mut store, ROOT, "Photos");
    let before = item(&mut store, local);

    store.item_store().merge_items(local, listed).unwrap();
    assert_eq!(
        item(&mut store, local),
        Item { onedrive_item: Some(reference("P")), onedrive: listed_row.onedrive, onedrive_etag: Some("e-P".to_owned()), placement: listed_row.placement, confirmed_at_count: 7, ..before }
    );
    assert_eq!(seen_in_full_listing(&store, local), 3);
    assert_eq!(item(&mut store, child).onedrive.unwrap().place, Some(place(local, "child")), "what OneDrive has in the directory is in the row that has its binding");
    assert_eq!(store.item_store().find_item(listed).unwrap(), None);
}

/// `give_up_binding`: the new row takes the binding and what OneDrive has under it, and
/// the item that gave it up has nothing of OneDrive's left.
#[test]
fn give_up_binding_hands_what_onedrive_has_to_the_row_that_takes_the_binding() {
    let mut store = TreeStore::in_memory().unwrap();
    let directory = make_directory(&mut store, ROOT, "Photos");
    synced(&mut store, directory, "P");
    let listed_row = with_onedrive_version(&mut store, directory, place(ROOT, "Photos"), "e-P");
    let child = store.item_store().make_onedrive_item(&reference("C"), ItemKind::File).unwrap();
    with_onedrive_version(&mut store, child, place(directory, "child"), "e-C");

    let taker = store.item_store().give_up_binding(directory, false).unwrap();
    let giver = item(&mut store, directory);
    assert_eq!((giver.onedrive, giver.onedrive_etag, giver.placement, giver.confirmed_at_count), (None, None, crate::model::Placement::Placed, 0));
    assert_eq!(seen_in_full_listing(&store, directory), 0);
    let taker_row = item(&mut store, taker);
    assert_eq!((taker_row.onedrive, taker_row.onedrive_etag, taker_row.placement, taker_row.confirmed_at_count), (listed_row.onedrive, Some("e-P".to_owned()), listed_row.placement, 7));
    assert_eq!(seen_in_full_listing(&store, taker), 3);
    assert_eq!(item(&mut store, child).onedrive.unwrap().place, Some(place(taker, "child")));
}
