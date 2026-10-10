use std::ffi::OsStr;

use konedrive_tree::{ContentPresence, ContentVersion, DriveId, Item, ItemKind, ItemPlace, LocalItemId, LocalVersionChange, OneDriveItemId, OneDriveItemReference, TreeStore};

use super::{tell_made_content, WrittenRows};
use crate::folder::disk::{daemon_owned, SET_ASIDE_PREFIX};
use crate::fuse::backing_directory_check::backing_directory_contents::BackingDirectoryEntry;
use crate::fuse::backing_directory_check::disk_finding::{CopyWholeness, DiskContent, DiskFinding, UnresolvedReason};
use crate::fuse::backing_directory_check::item_rows::ItemRows;
use crate::fuse::upload_rules::tests::rules;

const ROOT: LocalItemId = LocalItemId::ROOT;

fn drive(name: &str) -> DriveId {
    DriveId::new(name).unwrap()
}

fn entry(name: &str, kind: ItemKind, mark: Option<&str>) -> BackingDirectoryEntry {
    let file = DiskContent { size: 3, mtime: 100, mtime_nsec: 5, ctag: mark.map(|id| format!("c-{id}")) };
    BackingDirectoryEntry { name: name.into(), kind, onedrive_id: mark.map(str::to_owned), wholeness: CopyWholeness::Whole, file }
}

/// A store with `names` made in the root, a directory where the name ends in `/`.
fn store_with(names: &[&str]) -> (TreeStore, Vec<LocalItemId>) {
    let mut store = TreeStore::in_memory().unwrap();
    let made = names
        .iter()
        .map(|name| {
            let (kind, name) = match name.strip_suffix('/') {
                Some(name) => (ItemKind::Directory, name),
                None => (ItemKind::File, *name),
            };
            let content = (kind == ItemKind::File).then_some(ContentVersion::ChangedHere { size: 3, mtime: 100, mtime_nsec: 0, change_count: 1 });
            let place = ItemPlace { parent_local_item_id: ROOT, name: name.into() };
            store.item_store().apply(&LocalVersionChange::Made { kind, place, content, is_name_ignored: false, replaced: None }).unwrap()
        })
        .collect();
    (store, made)
}

fn row(store: &mut TreeStore, item: LocalItemId) -> Item {
    store.item_store().find_item(item).unwrap().unwrap()
}

fn name_of(store: &mut TreeStore, item: LocalItemId) -> (LocalItemId, String) {
    let place = row(store, item).local.unwrap().place.unwrap();
    (place.parent_local_item_id, place.name.to_string_lossy().into_owned())
}

/// Every write through it leaves the places it holds in memory as the store has them.
fn assert_places_are_the_stores(written: &WrittenRows<'_, '_>, items: &[LocalItemId]) {
    for &item in items {
        let in_store = written.item_store.find_item(item).unwrap().and_then(|row| row.local).and_then(|local| local.place);
        let in_memory = written.places().find_place(item).map(|(directory, name)| ItemPlace { parent_local_item_id: directory, name: name.to_owned() });
        assert_eq!(in_memory, in_store, "{item}");
    }
}

#[test]
fn a_file_no_row_knows_is_made_with_the_content_its_mark_tells_only_when_it_is_bound_to_that_mark() {
    let marked = entry("a.txt", ItemKind::File, Some("A"));
    assert_eq!(tell_made_content(&marked, DiskFinding::MarkedWithoutRow), Some(ContentVersion::Hashed { size: 3, mtime: 100, mtime_nsec: 5, hash: None, ctag: Some("c-A".to_owned()) }));
    let changed_here = Some(ContentVersion::ChangedHere { size: 3, mtime: 100, mtime_nsec: 5, change_count: 1 });
    assert_eq!(tell_made_content(&marked, DiskFinding::MadeWithStaleMark), changed_here);
    assert_eq!(tell_made_content(&entry("a.txt", ItemKind::File, None), DiskFinding::Made), changed_here);
    assert_eq!(tell_made_content(&entry("docs", ItemKind::Directory, Some("D")), DiskFinding::MarkedWithoutRow), None);
}

#[test]
fn an_item_is_made_at_its_entrys_name_with_what_the_rules_say_of_it() {
    let (mut store, made) = store_with(&["docs/"]);
    let rows = ItemRows::new(store.item_store().list_items().unwrap());
    let (rules, folder_drive) = (rules(), drive("d1"));
    let mut item_store = store.item_store();
    let mut written = WrittenRows::new(&mut item_store, &rules, &folder_drive, &rows);
    let file = entry("new.tmp", ItemKind::File, None);
    let new = written.make(made[0], &file, tell_made_content(&file, DiskFinding::Made), None).unwrap();
    assert_eq!(written.places().find_at(made[0], "new.tmp".as_ref()), Some(new));
    assert!(matches!(written.make(made[0], &file, tell_made_content(&file, DiskFinding::Made), None), Err(UnresolvedReason::Refused(_))), "the place is taken");
    assert_places_are_the_stores(&written, &[made[0], new]);
    let new = row(&mut store, new);
    assert_eq!((new.is_name_ignored, new.content_presence, new.onedrive_item), (true, ContentPresence::OnDisk, None));
}

/// The mark holds no drive: an item is bound in the drive of the nearest directory above
/// it that has a binding, and in the folder's own where there is none.
#[test]
fn an_item_is_bound_in_the_drive_of_the_directory_it_is_in() {
    let (mut store, made) = store_with(&["shared/", "top.txt"]);
    store.item_store().bind_item(made[0], &OneDriveItemReference { drive_id: drive("d2"), item_id: OneDriveItemId::new("S") }).unwrap();
    let rows = ItemRows::new(store.item_store().list_items().unwrap());
    let (rules, folder_drive) = (rules(), drive("d1"));
    let mut item_store = store.item_store();
    let mut written = WrittenRows::new(&mut item_store, &rules, &folder_drive, &rows);
    let inner = written.make(made[0], &entry("inner", ItemKind::Directory, None), None, None).unwrap();
    let file = entry("f.txt", ItemKind::File, Some("F"));
    let deep = written.make(inner, &file, tell_made_content(&file, DiskFinding::MarkedWithoutRow), None).unwrap();
    assert_eq!((written.find_drive_inside(inner), written.find_drive_inside(made[0]), written.find_drive_inside(ROOT)), (&drive("d2"), &drive("d2"), &drive("d1")));
    written.bind(deep, &OneDriveItemReference { drive_id: written.find_drive_inside(inner).clone(), item_id: OneDriveItemId::new("F") }).unwrap();
    written.bind(made[1], &OneDriveItemReference { drive_id: written.find_drive_inside(ROOT).clone(), item_id: OneDriveItemId::new("T") }).unwrap();
    let bound = |store: &mut TreeStore, item| row(store, item).onedrive_item.map(|reference| (reference.drive_id, reference.item_id.as_str().to_owned()));
    assert_eq!(bound(&mut store, deep), Some((drive("d2"), "F".to_owned())));
    assert_eq!(bound(&mut store, made[1]), Some((drive("d1"), "T".to_owned())));
}

#[test]
fn a_content_found_changed_is_another_content_with_its_count_raised_and_is_on_disk() {
    let (mut store, made) = store_with(&["a.txt", "open.txt"]);
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: made[1] }).unwrap();
    let rows = ItemRows::new(store.item_store().list_items().unwrap());
    let (rules, folder_drive) = (rules(), drive("d1"));
    let mut item_store = store.item_store();
    let mut written = WrittenRows::new(&mut item_store, &rules, &folder_drive, &rows);
    let file = DiskContent { size: 9, mtime: 200, mtime_nsec: 1, ctag: None };
    written.write_content(made[0], DiskFinding::ContentChanged, &file, false, true).unwrap();
    written.write_content(made[1], DiskFinding::ContentChanged, &file, true, true).unwrap();
    let changed = Some(ContentVersion::ChangedHere { size: 9, mtime: 200, mtime_nsec: 1, change_count: 2 });
    for item in made.clone() {
        let row = row(&mut store, item);
        assert_eq!((row.local.unwrap().content, row.content_before_write), (changed.clone(), None), "{item}");
    }
    let mut item_store = store.item_store();
    let mut written = WrittenRows::new(&mut item_store, &rules, &folder_drive, &rows);
    written.write_content(made[0], DiskFinding::ContentNoLongerOnDisk, &file, false, true).unwrap();
    assert_eq!(row(&mut store, made[0]).content_presence, ContentPresence::NotOnDisk);
    let mut item_store = store.item_store();
    let mut written = WrittenRows::new(&mut item_store, &rules, &folder_drive, &rows);
    written.write_content(made[0], DiskFinding::CopyLandedBeforeRow, &file, false, false).unwrap();
    written.write_content(made[1], DiskFinding::NothingChanged, &file, false, true).unwrap();
    assert_eq!((row(&mut store, made[0]).content_presence, row(&mut store, made[1]).local.unwrap().content), (ContentPresence::OnDisk, changed));
}

#[test]
fn an_item_is_moved_to_a_free_place_and_two_and_three_take_each_others_places() {
    let (mut store, made) = store_with(&["a", "b", "c", "docs/"]);
    let rows = ItemRows::new(store.item_store().list_items().unwrap());
    let (rules, folder_drive) = (rules(), drive("d1"));
    let mut item_store = store.item_store();
    let mut written = WrittenRows::new(&mut item_store, &rules, &folder_drive, &rows);
    assert!(matches!(written.move_to_free_place(made[0], ROOT, "b".as_ref()), Err(UnresolvedReason::Refused(_))), "the place is taken");
    written.move_to_free_place(made[0], made[3], "a.tmp".as_ref()).unwrap();
    written.exchange(made[1], made[2]).unwrap();
    assert_places_are_the_stores(&written, &made);
    // a to b's place, b to c's, c to a's.
    let ring: Vec<(LocalItemId, (LocalItemId, &OsStr))> = vec![(made[0], (ROOT, "c".as_ref())), (made[1], (ROOT, "b".as_ref())), (made[2], (made[3], "a.tmp".as_ref()))];
    written.turn_round(&ring).unwrap();
    assert_places_are_the_stores(&written, &made);
    assert_eq!((name_of(&mut store, made[0]), name_of(&mut store, made[1]), name_of(&mut store, made[2])), ((ROOT, "c".to_owned()), (ROOT, "b".to_owned()), (made[3], "a.tmp".to_owned())));
    assert!(row(&mut store, made[2]).is_name_ignored && !row(&mut store, made[0]).is_name_ignored, "each has what the rules say of its name");
    assert!(store.item_store().list_items().unwrap().iter().all(|row| row.local.as_ref().and_then(|local| local.place.as_ref()).is_none_or(|place| !daemon_owned(&place.name))), "the name the first stepped aside to is gone");
}

/// What leaves the folder leaves as it was found: removed here, with the removal of an item
/// of OneDrive to be sent; or with its place lost, and nothing to be sent.
#[test]
fn an_item_leaves_the_folder_removed_or_with_its_place_lost() {
    let (mut store, made) = store_with(&["own", "removed", "lost", "docs/", "kept"]);
    for (item, onedrive_id) in [(made[1], "R"), (made[2], "L")] {
        store.item_store().bind_item(item, &OneDriveItemReference { drive_id: drive("d1"), item_id: OneDriveItemId::new(onedrive_id) }).unwrap();
    }
    let place = ItemPlace { parent_local_item_id: made[3], name: "kept".into() };
    store.item_store().apply(&LocalVersionChange::Moved { item: made[4], place, is_name_ignored: false }).unwrap();
    let rows = ItemRows::new(store.item_store().list_items().unwrap());
    let (rules, folder_drive) = (rules(), drive("d1"));
    let mut item_store = store.item_store();
    let mut written = WrittenRows::new(&mut item_store, &rules, &folder_drive, &rows);
    written.take_from_folder(made[0], DiskFinding::Removed).unwrap();
    written.take_from_folder(made[1], DiskFinding::Removed).unwrap();
    written.take_from_folder(made[2], DiskFinding::PlaceLost).unwrap();
    assert!(matches!(written.take_from_folder(made[3], DiskFinding::Removed), Err(UnresolvedReason::Refused(_))), "what is in it leaves first");
    assert!(matches!(written.take_from_folder(made[4], DiskFinding::Moved), Err(UnresolvedReason::Refused(_))));
    assert_eq!(written.places().list_inside(ROOT), [made[3]]);
    assert_places_are_the_stores(&written, &made[1..]);
    assert_eq!(store.item_store().find_item(made[0]).unwrap(), None, "an item OneDrive never had loses its row");
    let (removed, lost) = (row(&mut store, made[1]), row(&mut store, made[2]));
    assert!(removed.local.is_none() && removed.onedrive_item.is_some() && lost.local.is_none() && lost.onedrive_item.is_some());
    assert_eq!(lost.unsent_since, None, "nothing of it is unsent");
}

#[test]
fn an_item_is_set_aside_to_a_name_of_the_daemons_own_that_no_row_has() {
    let (mut store, made) = store_with(&["a", "b"]);
    let taken = ItemPlace { parent_local_item_id: ROOT, name: format!("{SET_ASIDE_PREFIX}{}", made[0]).into() };
    store.item_store().apply(&LocalVersionChange::Moved { item: made[1], place: taken.clone(), is_name_ignored: false }).unwrap();
    let rows = ItemRows::new(store.item_store().list_items().unwrap());
    let (rules, folder_drive) = (rules(), drive("d1"));
    let mut item_store = store.item_store();
    let mut written = WrittenRows::new(&mut item_store, &rules, &folder_drive, &rows);
    let name = written.set_aside(made[0]).unwrap();
    assert!(daemon_owned(&name) && name != taken.name, "{name:?}");
    assert!(written.places().is_at(made[0], ROOT, &name));
    assert_places_are_the_stores(&written, &made);
}
