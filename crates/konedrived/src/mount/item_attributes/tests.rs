use std::time::{Duration, UNIX_EPOCH};

use fuser::FileType;
use konedrive_tree::{ContentPresence, ContentVersion, ItemPlace, ItemVersion, Placement};

use super::*;

const MOUNT_STARTED_AT: i64 = 1_700_000_000;

fn content(size: u64, mtime: i64, mtime_nsec: u32) -> ContentVersion {
    ContentVersion::Hashed { size, mtime, mtime_nsec, hash: None, ctag: Some("c1".into()) }
}

/// A row the folder shows as `kind`, with `content`.
pub(in crate::mount) fn shown_item(number: i64, kind: ItemKind, content: Option<ContentVersion>) -> Item {
    Item {
        local_item_id: LocalItemId::new(number),
        onedrive_item: None,
        kind,
        local: Some(ItemVersion { place: Some(ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: format!("item-{number}").into() }), content }),
        onedrive: None,
        onedrive_etag: None,
        onedrive_mime: None,
        synced_place: None,
        synced_content: None,
        content_presence: ContentPresence::OnDisk,
        content_before_write: None,
        placement: Placement::Placed,
        is_name_ignored: false,
        confirmed_at_count: 0,
        unsent_since: None,
    }
}

fn facts(size_and_times: SizeAndTimes, blocks: u64, directory_mtime: Option<(i64, u32)>) -> BackingFileFacts {
    BackingFileFacts { size_and_times, blocks, directory_mtime }
}

fn at(seconds: u64, nanoseconds: u32) -> std::time::SystemTime {
    UNIX_EPOCH + Duration::new(seconds, nanoseconds)
}

#[test]
fn a_files_size_and_times_are_its_local_contents() {
    let file = shown_item(7, ItemKind::File, Some(content(1234, 1_600_000_000, 500)));
    let attributes = to_item_attributes(&file, AccessMode::ReadWrite, &facts(SizeAndTimes::OfLocalContent, 0, None), MOUNT_STARTED_AT).unwrap();
    assert_eq!((attributes.ino, attributes.size, attributes.kind, attributes.nlink), (INodeNo(7), 1234, FileType::RegularFile, 1));
    assert_eq!((attributes.mtime, attributes.atime, attributes.ctime), (at(1_600_000_000, 500), at(1_600_000_000, 500), at(1_600_000_000, 500)));
    assert_eq!((attributes.perm, attributes.blksize, attributes.crtime), (0o644, 4096, UNIX_EPOCH));
    // A directory's time found on disk is nothing to a file.
    let with_directory_time = to_item_attributes(&file, AccessMode::ReadWrite, &facts(SizeAndTimes::OfLocalContent, 0, Some((5, 5))), MOUNT_STARTED_AT).unwrap();
    assert_eq!(with_directory_time, attributes);
}

#[test]
fn a_file_open_for_writing_has_the_size_and_times_of_its_backing_file() {
    // The row keeps the size of before the open until the last close.
    let file = shown_item(7, ItemKind::File, Some(content(5, 1_600_000_000, 0)));
    let of_writers_file = SizeAndTimes::OfWritersFile { size: 11, mtime: 1_650_000_000, mtime_nsec: 42 };
    let attributes = to_item_attributes(&file, AccessMode::ReadWrite, &facts(of_writers_file, 8, None), MOUNT_STARTED_AT).unwrap();
    assert_eq!((attributes.size, attributes.mtime, attributes.blocks), (11, at(1_650_000_000, 42), 8), "an outdated size loses appended data");
}

#[test]
fn a_files_blocks_are_its_backing_files_and_none_when_it_is_not_on_disk() {
    let file = shown_item(7, ItemKind::File, Some(content(100_000, 1_600_000_000, 0)));
    let on_disk = to_item_attributes(&file, AccessMode::ReadWrite, &facts(SizeAndTimes::OfLocalContent, 200, None), MOUNT_STARTED_AT).unwrap();
    assert_eq!((on_disk.size, on_disk.blocks), (100_000, 200));
    let not_on_disk = to_item_attributes(&file, AccessMode::ReadWrite, &facts(SizeAndTimes::OfLocalContent, 0, None), MOUNT_STARTED_AT).unwrap();
    assert_eq!((not_on_disk.size, not_on_disk.blocks), (100_000, 0), "the size is the content's, whatever is on disk");
    // A file with no content is shown empty.
    let without_content = to_item_attributes(&shown_item(8, ItemKind::File, None), AccessMode::ReadWrite, &facts(SizeAndTimes::OfLocalContent, 0, None), MOUNT_STARTED_AT).unwrap();
    assert_eq!((without_content.size, without_content.mtime), (0, at(MOUNT_STARTED_AT as u64, 0)));
}

#[test]
fn a_directory_has_the_times_the_decision_gives_it() {
    let on_disk = facts(SizeAndTimes::OfLocalContent, 0, Some((1_650_000_000, 7)));
    let not_on_disk = facts(SizeAndTimes::OfLocalContent, 0, None);
    assert_eq!(decide_directory_mtime(&on_disk, MOUNT_STARTED_AT), (1_650_000_000, 7), "the backing directory's when it is on disk");
    assert_eq!(decide_directory_mtime(&not_on_disk, MOUNT_STARTED_AT), (MOUNT_STARTED_AT, 0), "else the time the mount started");
    let directory = shown_item(9, ItemKind::Directory, None);
    let attributes = to_item_attributes(&directory, AccessMode::ReadWrite, &on_disk, MOUNT_STARTED_AT).unwrap();
    assert_eq!((attributes.ino, attributes.kind, attributes.size, attributes.blocks, attributes.nlink, attributes.perm), (INodeNo(9), FileType::Directory, 0, 0, 2, 0o755));
    assert_eq!(attributes.mtime, at(1_650_000_000, 7));
    assert_eq!(to_item_attributes(&directory, AccessMode::ReadWrite, &not_on_disk, MOUNT_STARTED_AT).unwrap().mtime, at(MOUNT_STARTED_AT as u64, 0));
    // Blocks and a writer's size are nothing to a directory.
    let of_a_file = facts(SizeAndTimes::OfWritersFile { size: 11, mtime: 1, mtime_nsec: 1 }, 8, None);
    let attributes = to_item_attributes(&directory, AccessMode::ReadWrite, &of_a_file, MOUNT_STARTED_AT).unwrap();
    assert_eq!((attributes.size, attributes.blocks, attributes.mtime), (0, 0, at(MOUNT_STARTED_AT as u64, 0)));
}

#[test]
fn a_read_only_folder_shows_no_write_bit() {
    let no_facts = facts(SizeAndTimes::OfLocalContent, 0, None);
    let perm = |kind, access_mode| to_item_attributes(&shown_item(7, kind, None), access_mode, &no_facts, MOUNT_STARTED_AT).unwrap().perm;
    assert_eq!((perm(ItemKind::File, AccessMode::ReadOnly), perm(ItemKind::Directory, AccessMode::ReadOnly)), (0o444, 0o555));
    assert_eq!((perm(ItemKind::File, AccessMode::ReadWrite), perm(ItemKind::Directory, AccessMode::ReadWrite)), (0o644, 0o755));
}

#[test]
fn an_item_the_folder_does_not_show_has_no_attributes() {
    let no_facts = facts(SizeAndTimes::OfLocalContent, 0, None);
    for kind in [ItemKind::File, ItemKind::Directory] {
        let not_shown = Item { local: None, ..shown_item(7, kind, None) };
        assert_eq!(to_item_attributes(&not_shown, AccessMode::ReadWrite, &no_facts, MOUNT_STARTED_AT), None);
    }
}

#[test]
fn the_node_id_of_an_item_is_its_local_item_id_and_back() {
    assert_eq!(to_node_id(LocalItemId::ROOT), INodeNo::ROOT, "the root is 1 on both sides");
    assert_eq!(to_local_item_id(INodeNo::ROOT), LocalItemId::ROOT);
    for number in [2, 77, i64::MAX] {
        assert_eq!(to_node_id(LocalItemId::new(number)), INodeNo(number as u64));
        assert_eq!(to_local_item_id(to_node_id(LocalItemId::new(number))), LocalItemId::new(number));
    }
    assert!(to_local_item_id(INodeNo(u64::MAX)).as_i64() < 0, "a number beyond every row's names no item");
}

#[test]
fn a_file_whose_handle_outlived_its_row_has_the_attributes_of_the_handles_file() {
    let facts = OpenFileFacts { size: 3, mtime: 1_650_000_000, mtime_nsec: 9, blocks: 8 };
    let attributes = to_open_file_attributes(LocalItemId::new(12), AccessMode::ReadOnly, &facts);
    assert_eq!((attributes.ino, attributes.kind, attributes.size, attributes.blocks, attributes.mtime, attributes.perm), (INodeNo(12), FileType::RegularFile, 3, 8, at(1_650_000_000, 9), 0o444));
}

#[test]
fn a_name_that_is_vanishing_has_attributes_of_its_kind_and_nothing_else() {
    let file = to_vanishing_attributes(LocalItemId::new(12), ItemKind::File, AccessMode::ReadWrite, MOUNT_STARTED_AT);
    assert_eq!((file.ino, file.kind, file.size, file.blocks), (INodeNo(12), FileType::RegularFile, 0, 0));
    let directory = to_vanishing_attributes(LocalItemId::new(13), ItemKind::Directory, AccessMode::ReadWrite, MOUNT_STARTED_AT);
    assert_eq!((directory.ino, directory.kind), (INodeNo(13), FileType::Directory));
}

#[test]
fn a_time_before_1970_is_shown_as_it_is() {
    let file = shown_item(7, ItemKind::File, Some(content(1, -10, 250_000_000)));
    let attributes = to_item_attributes(&file, AccessMode::ReadWrite, &facts(SizeAndTimes::OfLocalContent, 0, None), MOUNT_STARTED_AT).unwrap();
    assert_eq!(attributes.mtime, UNIX_EPOCH - Duration::from_millis(9750));
}
