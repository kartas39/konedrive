use konedrive_tree::{ContentPresence, ContentVersion, DriveId, Item, ItemKind, ItemPlace, ItemVersion, LocalItemId, OneDriveItemId, OneDriveItemReference, Placement};

use super::ItemRows;

fn id(number: i64) -> LocalItemId {
    LocalItemId::new(number)
}

/// The row of an item OneDrive does not have, at `name` in `parent`.
fn own(number: i64, parent: i64, name: &str, kind: ItemKind) -> Item {
    let content = (kind == ItemKind::File).then_some(ContentVersion::ChangedHere { size: 3, mtime: 100, mtime_nsec: 0, change_count: 1 });
    Item {
        local_item_id: id(number),
        onedrive_item: None,
        kind,
        local: Some(ItemVersion { place: Some(ItemPlace { parent_local_item_id: id(parent), name: name.into() }), content }),
        onedrive: None,
        onedrive_etag: None,
        onedrive_mime: None,
        synced_place: None,
        synced_content: None,
        content_presence: if kind == ItemKind::File { ContentPresence::OnDisk } else { ContentPresence::NotOnDisk },
        content_before_write: None,
        placement: Placement::Placed,
        is_name_ignored: false,
        confirmed_at_count: 0,
        unsent_since: None,
        is_executable: false,
    }
}

fn bound_in(drive: &str, onedrive_id: &str, mut item: Item) -> Item {
    item.onedrive_item = Some(OneDriveItemReference { drive_id: DriveId::new(drive).unwrap(), item_id: OneDriveItemId::new(onedrive_id) });
    item
}

#[test]
fn a_row_is_found_by_its_id_and_by_its_place() {
    let rows = ItemRows::new(vec![own(2, 1, "docs", ItemKind::Directory), own(3, 2, "b.txt", ItemKind::File), own(4, 2, "a.txt", ItemKind::File), own(5, 1, "a.txt", ItemKind::File)]);
    assert_eq!(rows.find(id(3)).unwrap().local_item_id, id(3));
    assert!(rows.find(id(9)).is_none());
    assert_eq!(rows.find_at(id(2), "a.txt".as_ref()).unwrap().local_item_id, id(4));
    assert_eq!(rows.find_at(id(1), "a.txt".as_ref()).unwrap().local_item_id, id(5));
    assert!(rows.find_at(id(1), "b.txt".as_ref()).is_none());
    assert_eq!(rows.list_inside(id(2)).iter().map(|item| item.local_item_id).collect::<Vec<_>>(), [id(4), id(3)], "by name");
}

#[test]
fn an_item_the_folder_does_not_show_has_no_place_and_its_id_of_onedrive_is_had_by_a_row() {
    let mut unshown = bound_in("d1", "X", own(2, 1, "x.bin", ItemKind::File));
    unshown.local = None;
    let rows = ItemRows::new(vec![unshown, bound_in("d2", "Y", own(3, 1, "y.bin", ItemKind::File)), own(4, 1, "own.txt", ItemKind::File)]);
    assert!(rows.find_at(id(1), "x.bin".as_ref()).is_none());
    assert!(rows.has_onedrive_id("X"), "a row the folder does not show");
    assert!(rows.has_onedrive_id("Y"), "a row of another drive");
    assert!(!rows.has_onedrive_id("Z"));
}
