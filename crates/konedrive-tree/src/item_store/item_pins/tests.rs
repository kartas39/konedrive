use super::*;
use crate::item_store::tests::listed_drive::{listed_file, listed_gone, listed_root, reference, store_with_listed_root, write_listing_of_changes};
use crate::item_store::tests::shown_items::{assert_rules_hold, row, show_listed_directory, show_listed_file};
use crate::item_store::tests::{make_file, place};
use crate::model::{ContentVersion, LocalVersionChange};
use crate::sync_plan::ItemVersions;
use crate::{ListingLink, StepCommitOutcome, TreeStore};

const ROOT: LocalItemId = LocalItemId::ROOT;

fn pin(store: &mut TreeStore, item: LocalItemId) {
    assert_eq!(store.item_store().set_item_pin(item, true).unwrap(), PinChange::Written);
}

fn pins(store: &TreeStore) -> Vec<String> {
    let mut pins = store.pins().unwrap();
    pins.sort();
    pins
}

fn count_pins(store: &mut TreeStore) -> u64 {
    store.item_store().reader().count_pins().unwrap()
}

#[test]
fn a_pin_is_set_on_the_binding_of_the_item() {
    let mut store = store_with_listed_root();
    let directory = show_listed_directory(&mut store, "D", "R", "d");
    let file = show_listed_file(&mut store, "F", "D", "f", "h1");
    pin(&mut store, directory);
    assert_eq!(store.item_store().set_item_pin(directory, true).unwrap(), PinChange::AlreadySo);
    assert_eq!(pins(&store), ["D"], "by the item id of OneDrive; set twice is set once");
    assert!(store.item_store().is_pinned(file).unwrap(), "a pin on a directory holds for what is in it");
    assert_eq!(count_pins(&mut store), 1);

    assert_eq!(store.item_store().set_item_pin(directory, false).unwrap(), PinChange::Written);
    assert_eq!(pins(&store), Vec::<String>::new());
    assert!(!store.item_store().is_pinned(file).unwrap());
    assert_eq!(store.item_store().set_item_pin(directory, false).unwrap(), PinChange::AlreadySo, "taken off twice");
}

#[test]
fn an_item_with_no_binding_cannot_be_pinned() {
    let mut store = store_with_listed_root();
    let made_file = make_file(&mut store, ROOT, "made");
    assert_eq!(store.item_store().set_item_pin(made_file, true).unwrap(), PinChange::NoBinding);
    assert_eq!(store.item_store().set_item_pin(made_file, false).unwrap(), PinChange::NoBinding);
    assert_eq!(store.item_store().set_item_pin(LocalItemId::new(999), true).unwrap(), PinChange::NoItem);
    assert_eq!(pins(&store), Vec::<String>::new());
}

#[test]
fn a_pin_goes_with_its_binding_when_the_binding_moves_to_another_item() {
    let mut store = store_with_listed_root();
    let file = show_listed_file(&mut store, "F", "R", "f", "h1");
    pin(&mut store, file);
    // A program saves the file by a rename over it: the replacing file takes the binding.
    let replacing = make_file(&mut store, ROOT, "f.tmp");
    store.item_store().apply(&LocalVersionChange::SavedOver { replacing, replaced: file }).unwrap();

    assert_eq!(row(&mut store, replacing).onedrive_item, Some(reference("F")));
    assert!(store.item_store().is_pinned(replacing).unwrap());
    assert_eq!((pins(&store), count_pins(&mut store)), (vec!["F".to_owned()], 1));
}

#[test]
fn a_confirmed_removal_takes_the_pin_of_the_item() {
    let mut store = store_with_listed_root();
    let directory = show_listed_directory(&mut store, "D", "R", "d");
    let inside = show_listed_file(&mut store, "F", "D", "f", "h1");
    let other = show_listed_file(&mut store, "G", "R", "g", "h2");
    // Each with a mark of its own, also the file a pin above holds for already.
    for item_id in ["D", "F", "G"] {
        store.set_pin(item_id, true).unwrap();
    }
    store.item_store().apply(&LocalVersionChange::Removed { item: other }).unwrap();
    assert_eq!(pins(&store), ["D", "F", "G"], "removed here, and OneDrive still has it");
    let removed = ItemVersions::of(&row(&mut store, other));
    assert_eq!(store.item_store().commit_send_file_removal(&removed).unwrap(), StepCommitOutcome::Written);
    assert_eq!(pins(&store), ["D", "F"]);

    // A directory's removal takes the pins of all OneDrive had in it.
    store.item_store().apply(&LocalVersionChange::Removed { item: inside }).unwrap();
    store.item_store().apply(&LocalVersionChange::Removed { item: directory }).unwrap();
    let (removed, below) = (ItemVersions::of(&row(&mut store, directory)), ItemVersions::of(&row(&mut store, inside)));
    assert_eq!(store.item_store().commit_send_directory_removal(&removed, &[below]).unwrap(), StepCommitOutcome::Written);
    assert_eq!(pins(&store), Vec::<String>::new());
    assert_rules_hold(&mut store);
}

#[test]
fn a_listed_removal_and_a_full_listings_sweep_take_the_pin() {
    let mut store = store_with_listed_root();
    show_listed_directory(&mut store, "D", "R", "d");
    show_listed_file(&mut store, "F", "D", "f", "h1");
    show_listed_file(&mut store, "G", "R", "g", "h2");
    let unseen = show_listed_file(&mut store, "U", "R", "u", "h3");
    show_listed_file(&mut store, "K", "R", "k", "h4");
    write_listing_of_changes(&mut store, &[listed_file("N", "R", "never-shown", (3, "h5"), "e-N")]);
    let never_shown = store.item_store().find_bound_item(&reference("N")).unwrap().unwrap().local_item_id;
    // Each with a mark of its own, also the file a pin above holds for already.
    for item_id in ["D", "F", "G", "U", "K", "N"] {
        store.set_pin(item_id, true).unwrap();
    }

    // A listing says the file is gone, and a directory with what the rows say is in it.
    write_listing_of_changes(&mut store, &[listed_gone("G"), listed_gone("D")]);
    assert_eq!(pins(&store), ["K", "N", "U"], "the pin of what OneDrive does not have any more is gone, though the folder still shows the item");

    // A full listing that does not name `U` nor `N` ends: what it did not see is gone.
    let listing = store.item_store().begin_full_listing().unwrap();
    let outcome = store.item_store().write_listed_changes(listing.id, &[listed_root(), listed_file("K", "R", "k", (3, "h4"), "e-K")], listing.began_at_count, &ListingLink::DeltaLink("link-2".into())).unwrap();
    assert!(outcome.is_link_written());
    assert_eq!(store.item_store().find_item(never_shown).unwrap(), None, "the row of an item the folder never showed went with the sweep");
    assert!(row(&mut store, unseen).onedrive.is_none());
    assert_eq!(pins(&store), ["K"]);
}

#[test]
fn the_count_of_pins_is_of_bindings_that_rows_still_have() {
    let mut store = store_with_listed_root();
    let file = show_listed_file(&mut store, "F", "R", "f", "h1");
    pin(&mut store, file);
    // A pin of an item id no row has, set by the id alone.
    store.set_pin("no-row-has-this", true).unwrap();
    assert_eq!(count_pins(&mut store), 1);
    assert_eq!(count_pins(&mut TreeStore::in_memory().unwrap()), 0);

    // A row that is deleted with its binding takes its pin: a pin set on a row that is a
    // binding alone goes when a listing's end takes the row.
    store.item_store().apply(&LocalVersionChange::Removed { item: file }).unwrap();
    let removed = ItemVersions::of(&row(&mut store, file));
    store.item_store().commit_send_file_removal(&removed).unwrap();
    pin(&mut store, file);
    assert_eq!(count_pins(&mut store), 1, "the row stays until a listing that began after the removal has ended");
    write_listing_of_changes(&mut store, &[]);
    assert_eq!(store.item_store().find_item(file).unwrap(), None);
    assert_eq!(pins(&store), ["no-row-has-this"]);
    assert_eq!(count_pins(&mut store), 0);
}

/// What the pins hold, by the sizes the folder shows: every file below a pinned directory
/// and a file made here in it, not those below an exception, and a file pinned beside.
#[test]
fn the_pinned_bytes_are_the_sizes_of_the_files_the_pins_hold() {
    let (mut store, [directory, _, sub, _, beside]) = store_with_shown_directory();
    let bytes = |store: &mut TreeStore| store.item_store().reader().sum_pinned_file_sizes().unwrap();
    assert_eq!(bytes(&mut store), 0);
    pin(&mut store, directory);
    assert_eq!(bytes(&mut store), 6, "`f` and `g`, 3 bytes each");
    let content = ContentVersion::ChangedHere { size: 5, mtime: 200, mtime_nsec: 0, change_count: 1 };
    store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::File, place: place(directory, "made"), content: Some(content), is_name_ignored: false, replaced: None }).unwrap();
    assert_eq!(bytes(&mut store), 11, "a file made here is held too");

    store.item_store().set_item_pin(sub, false).unwrap();
    assert_eq!(bytes(&mut store), 8, "`g` is excepted");
    pin(&mut store, beside);
    assert_eq!(bytes(&mut store), 11);
    store.item_store().set_item_pin(ROOT, false).unwrap();
    assert_eq!(bytes(&mut store), 0);
}

#[test]
fn the_own_mark_says_what_the_marks_above_do_not() {
    assert_eq!(decide_own_pin_mark(true, false), Some(PinMark::Pin));
    assert_eq!(decide_own_pin_mark(false, true), Some(PinMark::Exception));
    assert_eq!(decide_own_pin_mark(true, true), None, "the marks above say so already");
    assert_eq!(decide_own_pin_mark(false, false), None);
}

/// `d` {`f`, `sub` {`g`}} and `beside`, all shown.
fn store_with_shown_directory() -> (TreeStore, [LocalItemId; 5]) {
    let mut store = store_with_listed_root();
    let directory = show_listed_directory(&mut store, "D", "R", "d");
    let file = show_listed_file(&mut store, "F", "D", "f", "h1");
    let sub = show_listed_directory(&mut store, "S", "D", "sub");
    let in_sub = show_listed_file(&mut store, "G", "S", "g", "h2");
    let beside = show_listed_file(&mut store, "B", "R", "beside", "h3");
    (store, [directory, file, sub, in_sub, beside])
}

fn pinned(store: &mut TreeStore, items: &[LocalItemId]) -> Vec<bool> {
    items.iter().map(|item| store.item_store().is_pinned(*item).unwrap()).collect()
}

fn to_download(store: &mut TreeStore) -> Vec<LocalItemId> {
    store.item_store().list_pinned_files_to_download(None, 10).unwrap().iter().map(|item| item.local_item_id).collect()
}

#[test]
fn an_item_below_a_pin_is_excepted_and_a_pin_below_the_exception_holds() {
    let (mut store, [directory, file, sub, in_sub, beside]) = store_with_shown_directory();
    let all = [directory, file, sub, in_sub, beside];
    pin(&mut store, directory);
    assert_eq!(pinned(&mut store, &all), [true, true, true, true, false]);

    // A directory in the pinned one is let go: nothing in it is held, and the rest is.
    assert_eq!(store.item_store().set_item_pin(sub, false).unwrap(), PinChange::Written);
    assert_eq!(pinned(&mut store, &all), [true, true, false, false, false]);
    assert_eq!(to_download(&mut store), [file], "the pin does not download what is excepted");
    assert_eq!((pins(&store), count_pins(&mut store)), (vec!["D".to_owned()], 1), "an exception is no pin");
    assert_eq!(store.item_store().set_item_pin(sub, false).unwrap(), PinChange::AlreadySo);

    // A file the user makes in the excepted directory is not held either.
    let made = make_file(&mut store, sub, "made");
    assert!(!store.item_store().is_pinned(made).unwrap());

    // A pin below the exception wins over it.
    pin(&mut store, in_sub);
    assert_eq!(pinned(&mut store, &all), [true, true, false, true, false]);
    assert_eq!(to_download(&mut store), [file, in_sub]);
    assert_eq!(count_pins(&mut store), 2);
}

#[test]
fn pinning_an_excepted_item_takes_the_exception_off() {
    let (mut store, [directory, file, ..]) = store_with_shown_directory();
    pin(&mut store, directory);
    assert_eq!(store.item_store().set_item_pin(file, false).unwrap(), PinChange::Written);
    assert!(!store.item_store().is_pinned(file).unwrap());

    pin(&mut store, file);
    assert!(store.item_store().is_pinned(file).unwrap());
    assert_eq!(pins(&store), ["D"], "held by the directory's pin again, with no mark of its own");
    assert_eq!(store.item_store().set_item_pin(file, true).unwrap(), PinChange::AlreadySo);

    // With no pin above, an item that is not held gets no mark.
    assert_eq!(store.item_store().set_item_pin(directory, false).unwrap(), PinChange::Written);
    assert_eq!(store.item_store().set_item_pin(file, false).unwrap(), PinChange::AlreadySo);
    let marks: i64 = store.conn.query_row("SELECT COUNT(*) FROM pins", [], |row| row.get(0)).unwrap();
    assert_eq!(marks, 0);
}

#[test]
fn what_is_asked_of_a_directory_takes_the_marks_below_it_off() {
    let (mut store, [directory, file, sub, in_sub, beside]) = store_with_shown_directory();
    let all = [directory, file, sub, in_sub, beside];
    let marks = |store: &TreeStore| -> i64 { store.conn.query_row("SELECT COUNT(*) FROM pins", [], |row| row.get(0)).unwrap() };

    // Pinned: the exception and the pin below it go, and the whole directory is held.
    pin(&mut store, directory);
    store.item_store().set_item_pin(sub, false).unwrap();
    pin(&mut store, in_sub);
    pin(&mut store, beside);
    assert_eq!(store.item_store().set_item_pin(directory, true).unwrap(), PinChange::Written, "a pin held for it already, and marks below it went");
    assert_eq!(pinned(&mut store, &all), [true, true, true, true, true]);
    assert_eq!(marks(&store), 2, "the directory's and the one beside it, which is not below");

    // Let go: a file pinned by itself in it is let go with it.
    assert_eq!(store.item_store().set_item_pin(directory, false).unwrap(), PinChange::Written);
    pin(&mut store, in_sub);
    assert_eq!(store.item_store().set_item_pin(directory, false).unwrap(), PinChange::Written, "no pin held for it, and a pin below it went");
    assert_eq!(pinned(&mut store, &all), [false, false, false, false, true]);
    assert_eq!(marks(&store), 1);

    // The folder itself: every mark in it goes.
    assert_eq!(store.item_store().set_item_pin(ROOT, false).unwrap(), PinChange::Written);
    assert_eq!(marks(&store), 0);
}
