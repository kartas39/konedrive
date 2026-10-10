use super::*;
use crate::item_store::tests::make_file;
use crate::model::LocalItemId;
use crate::TreeStore;

const ROOT: LocalItemId = LocalItemId::ROOT;

fn state(store: &mut TreeStore) -> SendingState {
    store.item_store().reader().find_sending_state().unwrap()
}

#[test]
fn a_store_that_was_never_switched_is_read_write() {
    let mut store = TreeStore::in_memory().unwrap();
    assert_eq!(state(&mut store), SendingState::ReadWrite);
    make_file(&mut store, ROOT, "a");
    assert_eq!(state(&mut store), SendingState::ReadWrite);
}

#[test]
fn the_switch_to_read_only_keeps_the_last_stamp_given_before_it() {
    let mut store = TreeStore::in_memory().unwrap();
    assert_eq!(store.item_store().switch_sending_to_read_only().unwrap(), SendingState::ReadOnly { last_unsent_since: 0 }, "nothing was ever unsent");
    store.item_store().switch_sending_to_read_write().unwrap();

    make_file(&mut store, ROOT, "a");
    make_file(&mut store, ROOT, "b");
    let last = store.item_store().last_unsent_since().unwrap();
    assert_eq!(last, 2);
    assert_eq!(store.item_store().switch_sending_to_read_only().unwrap(), SendingState::ReadOnly { last_unsent_since: last });
    assert_eq!(state(&mut store), SendingState::ReadOnly { last_unsent_since: last });
}

#[test]
fn a_second_switch_to_read_only_keeps_the_first_stamp() {
    let mut store = TreeStore::in_memory().unwrap();
    make_file(&mut store, ROOT, "a");
    let first = store.item_store().switch_sending_to_read_only().unwrap();
    // The last close of a file that was open, written after the switch, gives a stamp.
    make_file(&mut store, ROOT, "b");
    assert_eq!(store.item_store().switch_sending_to_read_only().unwrap(), first);
    assert_eq!(state(&mut store), SendingState::ReadOnly { last_unsent_since: 1 });
}

#[test]
fn the_switch_back_to_read_write_forgets_the_stamp() {
    let mut store = TreeStore::in_memory().unwrap();
    make_file(&mut store, ROOT, "a");
    store.item_store().switch_sending_to_read_only().unwrap();
    assert_eq!(store.item_store().switch_sending_to_read_write().unwrap(), SendingState::ReadWrite);
    assert_eq!(state(&mut store), SendingState::ReadWrite);
    assert_eq!(store.item_store().switch_sending_to_read_write().unwrap(), SendingState::ReadWrite, "switched back twice");
    make_file(&mut store, ROOT, "b");
    assert_eq!(store.item_store().switch_sending_to_read_only().unwrap(), SendingState::ReadOnly { last_unsent_since: 2 }, "a later switch has its own stamp");
}

#[test]
fn the_stamp_of_the_switch_is_read_again_after_the_store_is_opened_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.sqlite");
    let mut store = TreeStore::open(&path).unwrap();
    make_file(&mut store, ROOT, "a");
    store.item_store().switch_sending_to_read_only().unwrap();
    make_file(&mut store, ROOT, "b");
    drop(store);
    assert_eq!(state(&mut TreeStore::open(&path).unwrap()), SendingState::ReadOnly { last_unsent_since: 1 });
    assert_eq!(TreeStore::open_read_only(&path).unwrap().read_items(|reader| reader.find_sending_state()).unwrap(), SendingState::ReadOnly { last_unsent_since: 1 });
}
