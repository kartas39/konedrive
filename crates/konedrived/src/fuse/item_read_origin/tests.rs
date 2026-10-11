use konedrive_tree::{DriveId, OneDriveItemId};

use super::*;
use crate::fuse::item_attributes::tests::shown_item;

fn content(ctag: &str, size: u64) -> ContentVersion {
    ContentVersion::Hashed { size, mtime: 1_600_000_000, mtime_nsec: 0, hash: None, ctag: Some(ctag.into()) }
}

fn binding() -> OneDriveItemReference {
    OneDriveItemReference { drive_id: DriveId::new("d1").unwrap(), item_id: OneDriveItemId::new("A1") }
}

/// A file of OneDrive the folder shows with `content`, on disk or not.
fn file(content: ContentVersion, content_presence: ContentPresence) -> Item {
    Item { onedrive_item: Some(binding()), content_presence, ..shown_item(7, ItemKind::File, Some(content)) }
}

#[test]
fn a_file_on_disk_is_opened_from_its_backing_file() {
    assert_eq!(decide_open_to_read(&file(content("c1", 10), ContentPresence::OnDisk)), Ok(OpenToRead::BackingFile));
    // A file made here has no binding, and is on disk.
    assert_eq!(decide_open_to_read(&shown_item(8, ItemKind::File, Some(content("c1", 10)))), Ok(OpenToRead::BackingFile));
}

#[test]
fn a_file_not_on_disk_is_opened_by_its_item_and_its_content_and_nothing_on_disk() {
    let opened = decide_open_to_read(&file(content("c1", 10), ContentPresence::NotOnDisk));
    assert_eq!(opened, Ok(OpenToRead::NotOnDisk { item: LocalItemId::new(7), content: content("c1", 10) }));
}

#[test]
fn a_file_with_no_binding_and_nothing_on_disk_cannot_be_read() {
    let nowhere = Item { onedrive_item: None, ..file(content("c1", 10), ContentPresence::NotOnDisk) };
    assert_eq!(decide_open_to_read(&nowhere), Err(Errno::EIO));
    assert_eq!(decide_not_on_disk_read_origin(&NodeRow::Shown(nowhere), &content("c1", 10)), NotOnDiskReadOrigin::Refused(Errno::EIO));
    // Nor a file of OneDrive whose row has no content.
    let without_content = Item { content_presence: ContentPresence::NotOnDisk, onedrive_item: Some(binding()), ..shown_item(7, ItemKind::File, None) };
    assert_eq!(decide_open_to_read(&without_content), Err(Errno::EIO));
}

#[test]
fn a_directory_and_an_item_the_folder_does_not_show_are_not_opened_to_be_read() {
    assert_eq!(decide_open_to_read(&shown_item(9, ItemKind::Directory, None)), Err(Errno::EISDIR));
    let not_shown = Item { local: None, ..file(content("c1", 10), ContentPresence::NotOnDisk) };
    assert_eq!(decide_open_to_read(&not_shown), Err(Errno::ESTALE));
}

#[test]
fn a_reader_whose_content_landed_since_reads_the_backing_file() {
    let landed = NodeRow::Shown(file(content("c1", 10), ContentPresence::OnDisk));
    assert_eq!(decide_not_on_disk_read_origin(&landed, &content("c1", 10)), NotOnDiskReadOrigin::LandedSince);
}

#[test]
fn a_reader_whose_row_is_gone_is_stale() {
    let not_shown = NodeRow::NotShown;
    for row in [NodeRow::Gone, not_shown] {
        assert_eq!(decide_not_on_disk_read_origin(&row, &content("c1", 10)), NotOnDiskReadOrigin::Refused(Errno::ESTALE), "this handle has no file to be answered from");
    }
}

#[test]
fn a_reader_whose_item_shows_another_content_by_now_is_stale() {
    let stale = NotOnDiskReadOrigin::Refused(Errno::ESTALE);
    // A step took OneDrive's new content: not on disk still, and another content.
    let taken = NodeRow::Shown(file(content("c2", 12), ContentPresence::NotOnDisk));
    assert_eq!(decide_not_on_disk_read_origin(&taken, &content("c1", 10)), stale);
    // Another content of the same size is another content.
    let same_size = NodeRow::Shown(file(content("c2", 10), ContentPresence::NotOnDisk));
    assert_eq!(decide_not_on_disk_read_origin(&same_size, &content("c1", 10)), stale);
    // And one that is on disk by now is not the one that was opened either: no byte of
    // one content at the size of another.
    let landed_other = NodeRow::Shown(file(content("c2", 12), ContentPresence::OnDisk));
    assert_eq!(decide_not_on_disk_read_origin(&landed_other, &content("c1", 10)), stale);
    let changed_here = NodeRow::Shown(file(ContentVersion::ChangedHere { size: 10, mtime: 1_600_000_000, mtime_nsec: 0, change_count: 1 }, ContentPresence::OnDisk));
    assert_eq!(decide_not_on_disk_read_origin(&changed_here, &content("c1", 10)), stale);
    // Between a listing and its step the row's local content is still the one opened,
    // whatever OneDrive has by then: the old content is read by its version.
    let listed = Item { onedrive: Some(konedrive_tree::ItemVersion { place: None, content: Some(content("c2", 12)) }), ..file(content("c1", 10), ContentPresence::NotOnDisk) };
    assert!(matches!(decide_not_on_disk_read_origin(&NodeRow::Shown(listed), &content("c1", 10)), NotOnDiskReadOrigin::OneDriveRange(_)));
}

#[test]
fn a_range_is_asked_of_the_drive_and_the_item_the_binding_names() {
    let row = NodeRow::Shown(file(content("c1", 10), ContentPresence::NotOnDisk));
    let origin = decide_not_on_disk_read_origin(&row, &content("c1", 10));
    assert_eq!(origin, NotOnDiskReadOrigin::OneDriveRange(RangeSource { onedrive_item: binding(), content: content("c1", 10) }), "the content is the one the handle was opened as: its size is the end of the file");
}
