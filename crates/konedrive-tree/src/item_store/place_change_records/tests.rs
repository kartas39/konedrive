use super::*;
use crate::item_store::tests::{hashed, made, make_file, place};
use crate::model::ContentPresence;
use crate::TreeStore;

const ROOT: LocalItemId = LocalItemId::ROOT;

/// One record of each case.
fn a_record_of_each_case() -> Vec<PlaceChangeRecord> {
    let (item, other) = (LocalItemId::new(7), LocalItemId::new(8));
    vec![
        PlaceChangeRecord::MakeEntry { kind: ItemKind::File, place: place(ROOT, "new.txt"), owner: MadeEntryOwner::NewItem { replaced: None } },
        PlaceChangeRecord::MakeEntry { kind: ItemKind::File, place: place(ROOT, "again.txt"), owner: MadeEntryOwner::NewItem { replaced: Some(other) } },
        PlaceChangeRecord::MakeEntry { kind: ItemKind::Directory, place: place(item, "dir"), owner: MadeEntryOwner::NewItem { replaced: None } },
        PlaceChangeRecord::MakeEntry { kind: ItemKind::File, place: place(ROOT, "empty.bin"), owner: MadeEntryOwner::Item(item) },
        PlaceChangeRecord::MoveEntry { item, from: place(ROOT, "a"), to: place(other, "b") },
        PlaceChangeRecord::MoveEntryOver { item, from: place(ROOT, "a.tmp"), to: place(ROOT, "a"), replaced: other, aside_name: ".konedrive-aside-8".into() },
        PlaceChangeRecord::SetAside { item, place: place(other, "gone"), aside_name: ".konedrive-aside-7".into() },
        PlaceChangeRecord::LinkFile { item, place: place(ROOT, "landed.bin"), content: hashed("h") },
    ]
}

#[test]
fn a_record_of_each_case_is_read_as_it_was_written_in_the_order_written() {
    let mut store = TreeStore::in_memory().unwrap();
    assert_eq!(store.item_store().list_place_change_records().unwrap(), vec![]);
    let records = a_record_of_each_case();
    let ids: Vec<PlaceChangeRecordId> = records.iter().map(|record| store.item_store().record_place_change(record).unwrap()).collect();
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]), "an id is given once, in order");
    assert_eq!(store.item_store().list_place_change_records().unwrap(), ids.into_iter().zip(records).collect::<Vec<_>>());
}

#[test]
fn a_name_that_is_not_utf8_is_kept_as_it_was_given() {
    use std::os::unix::ffi::OsStringExt;
    let mut store = TreeStore::in_memory().unwrap();
    let name = OsString::from_vec(vec![b'a', 0xff, b'b']);
    let record = PlaceChangeRecord::SetAside { item: LocalItemId::new(3), place: ItemPlace { parent_local_item_id: ROOT, name: name.clone() }, aside_name: name };
    let id = store.item_store().record_place_change(&record).unwrap();
    assert_eq!(store.item_store().list_place_change_records().unwrap(), vec![(id, record)]);
}

#[test]
fn a_record_with_a_name_that_is_no_name_of_a_directory_entry_is_refused() {
    let mut store = TreeStore::in_memory().unwrap();
    let item = LocalItemId::new(3);
    for record in [
        PlaceChangeRecord::MoveEntry { item, from: place(ROOT, "a"), to: place(ROOT, "b/c") },
        PlaceChangeRecord::MoveEntry { item, from: place(ROOT, ".."), to: place(ROOT, "b") },
        PlaceChangeRecord::SetAside { item, place: place(ROOT, "a"), aside_name: "".into() },
    ] {
        assert!(matches!(store.item_store().record_place_change(&record), Err(TreeError::Refused(_))), "{record:?}");
    }
    assert_eq!(store.item_store().list_place_change_records().unwrap(), vec![]);
}

#[test]
fn a_record_that_is_forgotten_is_gone_and_one_that_did_not_stand_is_said() {
    let mut store = TreeStore::in_memory().unwrap();
    let records = a_record_of_each_case();
    let first = store.item_store().record_place_change(&records[0]).unwrap();
    let second = store.item_store().record_place_change(&records[4]).unwrap();
    assert!(store.item_store().forget_place_change_record(first).unwrap());
    assert!(!store.item_store().forget_place_change_record(first).unwrap(), "it stood once");
    assert_eq!(store.item_store().list_place_change_records().unwrap(), vec![(second, records[4].clone())]);
}

#[test]
fn a_record_goes_in_the_transaction_of_the_row_and_stays_when_that_is_dropped() {
    let mut store = TreeStore::in_memory().unwrap();
    let record = PlaceChangeRecord::MakeEntry { kind: ItemKind::File, place: place(ROOT, "new.txt"), owner: MadeEntryOwner::NewItem { replaced: None } };
    let id = store.item_store().record_place_change(&record).unwrap();

    // A writer of the row whose transaction is dropped: the row and the record's going
    // are both undone.
    {
        let tx = store.conn.transaction().unwrap();
        apply_change(&tx, &made(place(ROOT, "new.txt"), ItemKind::File)).unwrap();
        assert!(forget_place_change_record(&tx, id).unwrap());
    }
    assert_eq!(store.item_store().find_item_at(ROOT, "new.txt".as_ref()).unwrap(), None);
    assert_eq!(store.item_store().list_place_change_records().unwrap(), vec![(id, record)]);

    // Committed: the row is there and the record is not.
    let tx = store.conn.transaction().unwrap();
    apply_change(&tx, &made(place(ROOT, "new.txt"), ItemKind::File)).unwrap();
    assert!(forget_place_change_record(&tx, id).unwrap());
    tx.commit().unwrap();
    assert!(store.item_store().find_item_at(ROOT, "new.txt".as_ref()).unwrap().is_some());
    assert_eq!(store.item_store().list_place_change_records().unwrap(), vec![]);
}

#[test]
fn a_record_settled_with_a_change_of_a_local_version_writes_the_row_and_goes() {
    let mut store = TreeStore::in_memory().unwrap();
    let record = PlaceChangeRecord::MakeEntry { kind: ItemKind::Directory, place: place(ROOT, "dir"), owner: MadeEntryOwner::NewItem { replaced: None } };
    let id = store.item_store().record_place_change(&record).unwrap();
    let made_item = store.item_store().settle_place_change_record(id, &PlaceChangeRowWrite::LocalVersion(made(place(ROOT, "dir"), ItemKind::Directory))).unwrap();
    assert_eq!(store.item_store().find_item_at(ROOT, "dir".as_ref()).unwrap().unwrap().local_item_id, made_item);
    assert_eq!(store.item_store().list_place_change_records().unwrap(), vec![]);
}

#[test]
fn a_settlement_the_rows_refuse_leaves_the_rows_and_the_record() {
    let mut store = TreeStore::in_memory().unwrap();
    make_file(&mut store, ROOT, "taken");
    let record = PlaceChangeRecord::MakeEntry { kind: ItemKind::File, place: place(ROOT, "taken"), owner: MadeEntryOwner::NewItem { replaced: None } };
    let id = store.item_store().record_place_change(&record).unwrap();
    let rows = store.item_store().list_items().unwrap();
    assert!(matches!(store.item_store().settle_place_change_record(id, &PlaceChangeRowWrite::LocalVersion(made(place(ROOT, "taken"), ItemKind::File))), Err(TreeError::Refused(_))));
    assert_eq!(store.item_store().list_items().unwrap(), rows);
    assert_eq!(store.item_store().list_place_change_records().unwrap(), vec![(id, record)]);
}

/// A file whose first writer's open was written with "on disk", as the mount writes it
/// before it makes the backing file of a file that was not on disk.
fn file_opened_for_writing_with_nothing_on_disk(store: &mut TreeStore) -> (LocalItemId, ContentVersion) {
    let file = make_file(store, ROOT, "f");
    store.item_store().set_content_presence(file, ContentPresence::NotOnDisk).unwrap();
    let before = store.item_store().find_item(file).unwrap().unwrap().local.unwrap().content.unwrap();
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: file }).unwrap();
    store.item_store().set_content_presence(file, ContentPresence::OnDisk).unwrap();
    (file, before)
}

#[test]
fn a_record_settled_with_the_content_before_the_write_puts_it_back_not_on_disk() {
    let mut store = TreeStore::in_memory().unwrap();
    let (file, before) = file_opened_for_writing_with_nothing_on_disk(&mut store);
    let id = store.item_store().record_place_change(&PlaceChangeRecord::MakeEntry { kind: ItemKind::File, place: place(ROOT, "f"), owner: MadeEntryOwner::Item(file) }).unwrap();
    assert_eq!(store.item_store().settle_place_change_record(id, &PlaceChangeRowWrite::ContentBeforeWrite { item: file }).unwrap(), file);
    let row = store.item_store().find_item(file).unwrap().unwrap();
    assert_eq!((row.local.unwrap().content, row.content_before_write, row.content_presence), (Some(before), None, ContentPresence::NotOnDisk));
    assert_eq!(store.item_store().list_place_change_records().unwrap(), vec![]);
}

#[test]
fn the_content_before_the_write_is_refused_for_a_file_that_keeps_none() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "f");
    let id = store.item_store().record_place_change(&PlaceChangeRecord::MakeEntry { kind: ItemKind::File, place: place(ROOT, "f"), owner: MadeEntryOwner::Item(file) }).unwrap();
    let row = store.item_store().find_item(file).unwrap();
    assert!(matches!(store.item_store().settle_place_change_record(id, &PlaceChangeRowWrite::ContentBeforeWrite { item: file }), Err(TreeError::Refused(_))));
    assert_eq!(store.item_store().find_item(file).unwrap(), row);
    assert_eq!(store.item_store().list_place_change_records().unwrap().len(), 1);
}

#[test]
fn a_record_settled_with_the_landed_content_says_on_disk_only_while_the_row_has_that_content() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "f");
    store.item_store().set_content_presence(file, ContentPresence::NotOnDisk).unwrap();
    let content = store.item_store().find_item(file).unwrap().unwrap().local.unwrap().content.unwrap();
    let record = PlaceChangeRecord::LinkFile { item: file, place: place(ROOT, "f"), content: content.clone() };

    let id = store.item_store().record_place_change(&record).unwrap();
    assert!(matches!(store.item_store().settle_place_change_record(id, &PlaceChangeRowWrite::ContentOnDisk { item: file, content: hashed("another") }), Err(TreeError::Refused(_))));
    assert_eq!(store.item_store().find_item(file).unwrap().unwrap().content_presence, ContentPresence::NotOnDisk);
    assert_eq!(store.item_store().list_place_change_records().unwrap().len(), 1);

    assert_eq!(store.item_store().settle_place_change_record(id, &PlaceChangeRowWrite::ContentOnDisk { item: file, content }).unwrap(), file);
    assert_eq!(store.item_store().find_item(file).unwrap().unwrap().content_presence, ContentPresence::OnDisk);
    assert_eq!(store.item_store().list_place_change_records().unwrap(), vec![]);
}

#[test]
fn a_record_outlives_the_row_of_its_item() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "f");
    let record = PlaceChangeRecord::SetAside { item: file, place: place(ROOT, "f"), aside_name: ".konedrive-aside-2".into() };
    let id = store.item_store().record_place_change(&record).unwrap();
    store.item_store().apply(&LocalVersionChange::Removed { item: file }).unwrap();
    assert_eq!(store.item_store().list_place_change_records().unwrap(), vec![(id, record)]);
}
