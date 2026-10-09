use std::sync::Arc;

use konedrive_tree::{Store, TreeStore};

use super::Pins;
use crate::fuse::directory_contents::tests::directory_contents;

/// The pins of the fixture's drive (`docs/c.txt`, `a.txt`, `b.txt`), with `pinned_ids`
/// pinned, over a store of their own.
fn pins(pinned_ids: &[&str]) -> (tempfile::TempDir, Store, Pins) {
    let (dir, contents) = directory_contents(Vec::new());
    let store = Store::new(TreeStore::in_memory().unwrap());
    let pinned_ids = pinned_ids.iter().map(|id| (*id).to_owned()).collect();
    let pins = Pins::new(pinned_ids, store.clone(), Arc::new(contents));
    (dir, store, pins)
}

/// The id of what keeps the OneDrive id `id` on this device.
fn pinned_by(pins: &Pins, id: &str) -> Option<String> {
    let row = pins.directory_contents.find_row(id).expect("a row of the fixture");
    pins.find_pinning_row(&row).map(|by| by.id)
}

/// The OneDrive ids the store has a pin on.
fn pinned_in_store(store: &Store) -> Vec<String> {
    let mut pinned = store.call_blocking(|s| s.pins()).unwrap();
    pinned.sort();
    pinned
}

#[test]
fn a_file_is_kept_by_its_own_pin() {
    let (_dir, _store, pins) = pins(&["A"]);
    assert_eq!(pinned_by(&pins, "A").as_deref(), Some("A"));
    assert_eq!(pinned_by(&pins, "B"), None);
    assert_eq!(pinned_by(&pins, "C"), None);
}

#[test]
fn a_pinned_directory_keeps_what_is_below_it() {
    let (_dir, _store, pins) = pins(&["D"]);
    assert_eq!(pinned_by(&pins, "C").as_deref(), Some("D"));
    assert_eq!(pinned_by(&pins, "D").as_deref(), Some("D"));
    // Not what lies beside it.
    assert_eq!(pinned_by(&pins, "A"), None);
    // The pin is the directory's own, not the file's.
    assert!(pins.is_pinned("D") && !pins.is_pinned("C"));
}

#[test]
fn the_nearest_pin_is_what_keeps_a_file() {
    let (_dir, _store, pins) = pins(&["R", "D", "C"]);
    assert_eq!(pinned_by(&pins, "C").as_deref(), Some("C"));
    assert_eq!(pinned_by(&pins, "D").as_deref(), Some("D"));
    assert_eq!(pinned_by(&pins, "A").as_deref(), Some("R"));
}

/// Each change is in the store before the set in memory has it: the store is asked itself,
/// not the pins.
#[tokio::test]
async fn a_pin_is_put_on_and_taken_off_in_the_store_and_in_memory() {
    let (_dir, store, pins) = pins(&[]);
    assert!(pins.set_pin("A", true).await.unwrap());
    assert!(pins.is_pinned("A"));
    assert_eq!(pins.count(), 1);
    assert_eq!(store.call(|s| s.pins()).await.unwrap(), vec!["A".to_owned()]);
    // Put on again, it changes nothing, and the store still has it once.
    assert!(!pins.set_pin("A", true).await.unwrap());
    assert_eq!(store.call(|s| s.pins()).await.unwrap(), vec!["A".to_owned()]);
    assert!(pins.set_pin("A", false).await.unwrap());
    assert!(!pins.is_pinned("A"));
    assert_eq!(pins.count(), 0);
    assert!(store.call(|s| s.pins()).await.unwrap().is_empty());
    assert!(!pins.set_pin("A", false).await.unwrap());
}

/// The pins of what is gone come off in memory and in the store; the others stay in both.
#[test]
fn what_a_cycle_removed_keeps_no_pin_in_memory_or_in_the_store() {
    let (_dir, store, pins) = pins(&["A", "D"]);
    store.call_blocking(|s| ["A", "D"].iter().try_for_each(|id| s.set_pin(id, true))).unwrap();
    pins.unpin_removed(vec!["A".to_owned(), "B".to_owned()]);
    assert!(!pins.is_pinned("A"));
    assert_eq!(pins.list_pinned_ids(), vec!["D".to_owned()]);
    assert_eq!(pinned_in_store(&store), vec!["D".to_owned()]);
}
