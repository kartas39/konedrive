use super::*;
use crate::item_store::tests::listed_drive::{listed_file, listed_gone, listed_root, reference, store_with_listed_root, write_listing_of_changes};
use crate::item_store::tests::shown_items::{assert_rules_hold, row, show_listed_directory, show_listed_file};
use crate::item_store::tests::make_file;
use crate::model::LocalVersionChange;
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
    pin(&mut store, directory);
    assert_eq!(pins(&store), ["D"], "by the item id of OneDrive; set twice is set once");
    assert!(store.item_store().is_pinned(file).unwrap(), "a pin on a directory holds for what is in it");
    assert_eq!(count_pins(&mut store), 1);

    assert_eq!(store.item_store().set_item_pin(directory, false).unwrap(), PinChange::Written);
    assert_eq!(pins(&store), Vec::<String>::new());
    assert!(!store.item_store().is_pinned(file).unwrap());
    assert_eq!(store.item_store().set_item_pin(directory, false).unwrap(), PinChange::Written, "taken off twice");
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
    for item in [directory, inside, other] {
        pin(&mut store, item);
    }
    store.item_store().apply(&LocalVersionChange::Removed { item: other }).unwrap();
    assert_eq!(pins(&store), ["D", "F", "G"], "removed here, and OneDrive still has it");
    let removed = ItemVersions::of(&row(&mut store, other));
    assert_eq!(store.item_store().commit_send_file_removal(&removed, 0).unwrap(), StepCommitOutcome::Written);
    assert_eq!(pins(&store), ["D", "F"]);

    // A directory's removal takes the pins of all OneDrive had in it.
    store.item_store().apply(&LocalVersionChange::Removed { item: inside }).unwrap();
    store.item_store().apply(&LocalVersionChange::Removed { item: directory }).unwrap();
    let (removed, below) = (ItemVersions::of(&row(&mut store, directory)), ItemVersions::of(&row(&mut store, inside)));
    assert_eq!(store.item_store().commit_send_directory_removal(&removed, &[below], 0).unwrap(), StepCommitOutcome::Written);
    assert_eq!(pins(&store), Vec::<String>::new());
    assert_rules_hold(&mut store);
}

#[test]
fn a_listed_removal_and_a_full_listings_sweep_take_the_pin() {
    let mut store = store_with_listed_root();
    let directory = show_listed_directory(&mut store, "D", "R", "d");
    let inside = show_listed_file(&mut store, "F", "D", "f", "h1");
    let gone = show_listed_file(&mut store, "G", "R", "g", "h2");
    let unseen = show_listed_file(&mut store, "U", "R", "u", "h3");
    let kept = show_listed_file(&mut store, "K", "R", "k", "h4");
    write_listing_of_changes(&mut store, &[listed_file("N", "R", "never-shown", (3, "h5"), "e-N")]);
    let never_shown = store.item_store().find_bound_item(&reference("N")).unwrap().unwrap().local_item_id;
    for item in [directory, inside, gone, unseen, kept, never_shown] {
        pin(&mut store, item);
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
    // A pin of an item id no row has: one today's mount set, by the id alone.
    store.set_pin("no-row-has-this", true).unwrap();
    assert_eq!(count_pins(&mut store), 1);
    assert_eq!(count_pins(&mut TreeStore::in_memory().unwrap()), 0);

    // A row that is deleted with its binding takes its pin: a pin set on a row that is a
    // binding alone goes when a listing's end takes the row.
    store.item_store().apply(&LocalVersionChange::Removed { item: file }).unwrap();
    let removed = ItemVersions::of(&row(&mut store, file));
    store.item_store().commit_send_file_removal(&removed, 0).unwrap();
    pin(&mut store, file);
    assert_eq!(count_pins(&mut store), 1, "the row stays until a listing that began after the removal has ended");
    write_listing_of_changes(&mut store, &[]);
    assert_eq!(store.item_store().find_item(file).unwrap(), None);
    assert_eq!(pins(&store), ["no-row-has-this"]);
    assert_eq!(count_pins(&mut store), 0);
}
