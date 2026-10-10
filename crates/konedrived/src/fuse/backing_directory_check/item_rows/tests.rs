use konedrive_tree::{ContentPresence, ContentVersion, DriveId, Item, ItemKind, ItemPlace, ItemVersion, LocalItemId, OneDriveItemId, OneDriveItemReference, Placement};

use super::ItemRows;

pub(in crate::fuse::backing_directory_check) fn id(number: i64) -> LocalItemId {
    LocalItemId::new(number)
}

pub(in crate::fuse::backing_directory_check) fn drive(name: &str) -> DriveId {
    DriveId::new(name).unwrap()
}

/// The row of an item OneDrive does not have, at `name` in `parent`: a file of 3 bytes last
/// modified at 100, changed here, its content on disk.
pub(in crate::fuse::backing_directory_check) fn own(number: i64, parent: i64, name: &str, kind: ItemKind) -> Item {
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
    }
}

/// The row of the item `onedrive_id` of the drive `drive_name`, the two sides agreeing: a
/// file is a clean copy on disk, of the cTag `c-<onedrive_id>`.
pub(in crate::fuse::backing_directory_check) fn bound_in(drive_name: &str, onedrive_id: &str, number: i64, parent: i64, name: &str, kind: ItemKind) -> Item {
    let mut item = own(number, parent, name, kind);
    let local = item.local.as_mut().unwrap();
    if kind == ItemKind::File {
        local.content = Some(ContentVersion::Hashed { size: 3, mtime: 100, mtime_nsec: 0, hash: Some(format!("h-{onedrive_id}")), ctag: Some(format!("c-{onedrive_id}")) });
    }
    item.synced_place = local.place.clone();
    item.synced_content = local.content.clone();
    item.onedrive = item.local.clone();
    item.onedrive_item = Some(OneDriveItemReference { drive_id: drive(drive_name), item_id: OneDriveItemId::new(onedrive_id) });
    item
}

/// [`bound_in`] the folder's own drive, `d1`.
pub(in crate::fuse::backing_directory_check) fn bound(onedrive_id: &str, number: i64, parent: i64, name: &str, kind: ItemKind) -> Item {
    bound_in("d1", onedrive_id, number, parent, name, kind)
}

/// The file's content is not on disk.
pub(in crate::fuse::backing_directory_check) fn not_downloaded(item: Item) -> Item {
    Item { content_presence: ContentPresence::NotOnDisk, ..item }
}

/// The file was changed here through the mount, and the change is not sent.
pub(in crate::fuse::backing_directory_check) fn changed_here(mut item: Item) -> Item {
    item.local.as_mut().unwrap().content = Some(ContentVersion::ChangedHere { size: 3, mtime: 100, mtime_nsec: 0, change_count: 1 });
    item
}

/// The item was removed here, and the removal is not sent.
pub(in crate::fuse::backing_directory_check) fn removed_here(item: Item) -> Item {
    Item { local: None, content_presence: ContentPresence::NotOnDisk, ..item }
}

#[test]
fn a_row_the_folder_shows_is_found_by_its_id_by_its_place_and_among_what_its_directory_holds() {
    let rows = ItemRows::new(vec![own(2, 1, "docs", ItemKind::Directory), own(3, 2, "b.txt", ItemKind::File), own(4, 2, "a.txt", ItemKind::File), removed_here(bound("X", 5, 2, "gone.txt", ItemKind::File))]);
    assert_eq!(rows.find(id(3)).map(|row| row.local_item_id), Some(id(3)));
    assert_eq!(rows.find_at(id(2), "a.txt".as_ref()).map(|row| row.local_item_id), Some(id(4)));
    assert_eq!(rows.find_at(id(1), "a.txt".as_ref()).map(|row| row.local_item_id), None);
    assert_eq!(rows.find_at(id(2), "gone.txt".as_ref()).map(|row| row.local_item_id), None, "the folder does not show it");
    assert_eq!(rows.list_inside(id(2)).iter().map(|row| row.local_item_id).collect::<Vec<_>>(), [id(4), id(3)], "by name");
    assert_eq!(rows.list().count(), 4);
}

/// A mark holds an item's id and no drive: of two rows that have one id, the drive tells
/// which is meant. Where no row of the drive has the id, the row that has it is meant: an
/// item is not always of the drive of the directory it is in.
#[test]
fn a_bound_row_is_found_by_its_drive_and_its_item_id_shown_and_not_shown_apart() {
    let rows = ItemRows::new(vec![
        bound_in("d1", "X", 2, 1, "one.txt", ItemKind::File),
        bound_in("d2", "X", 3, 1, "two.txt", ItemKind::File),
        removed_here(bound_in("d2", "Y", 4, 1, "gone.txt", ItemKind::File)),
        removed_here(bound_in("d1", "D", 5, 1, "gone", ItemKind::Directory)),
    ]);
    assert_eq!(rows.find_shown_bound(&drive("d1"), "X").map(|row| row.local_item_id), Some(id(2)));
    assert_eq!(rows.find_shown_bound(&drive("d2"), "X").map(|row| row.local_item_id), Some(id(3)));
    assert_eq!(rows.find_shown_bound(&drive("d3"), "X").map(|row| row.local_item_id), Some(id(2)), "no row of that drive: the one made first");
    assert_eq!(rows.find_shown_bound(&drive("d2"), "Y").map(|row| row.local_item_id), None, "not shown");
    assert_eq!(rows.find_unshown_bound(&drive("d2"), "Y").map(|row| row.local_item_id), Some(id(4)));
    assert_eq!(rows.find_unshown_bound(&drive("d1"), "Y").map(|row| row.local_item_id), Some(id(4)), "no row of that drive has the id");
    assert_eq!(rows.find_unshown_bound(&drive("d1"), "none").map(|row| row.local_item_id), None);
    assert_eq!(rows.find_unshown_bound(&drive("d1"), "X").map(|row| row.local_item_id), None, "shown");
}

/// What a file made by hand replaces: a file of OneDrive removed here from that place.
#[test]
fn a_file_removed_here_is_found_by_the_place_it_was_agreed_to_be_at() {
    let rows = ItemRows::new(vec![removed_here(bound("X", 2, 1, "gone.txt", ItemKind::File)), removed_here(bound("D", 3, 1, "gone", ItemKind::Directory))]);
    assert_eq!(rows.find_file_removed_at(id(1), "gone.txt".as_ref()), Some(id(2)));
    assert_eq!(rows.find_file_removed_at(id(1), "gone".as_ref()), None, "a directory is made as a new item");
    assert_eq!(rows.find_file_removed_at(id(2), "gone.txt".as_ref()), None);
}
