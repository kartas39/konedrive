use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::FileType;
use konedrive_fs::handle::FileHandle;
use konedrive_tree::{Kind, Row};

use crate::fuse::directory_reference::DirectoryReference;
use crate::fuse::entry_place::EntryPlace;
use crate::fuse::local_only_entry::LocalOnlyEntry;
use super::attributes;
use crate::fuse::access_mode::AccessMode;
use crate::fuse::backing_directory::{DiskEntry, DiskMetadata, EntryKind};
use crate::fuse::directory_contents::tests::row;
use crate::fuse::directory_contents::{DirectoryEntry, DirectorySyncState, EntryState, FileSyncState};

fn at(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

/// A file of 10 bytes in 8 blocks on disk, last modified at 500.
fn on_disk(kind: EntryKind) -> DiskMetadata {
    DiskMetadata { kind, size: 10, blocks: 8, modified: at(500) }
}

/// The store's row of a file of 42 bytes, or of a directory, last modified at 100.
fn stored(kind: Kind) -> Row {
    Row { size: 42, mtime: 100, ..row("A", Some("R"), "name", kind) }
}

fn file(state: FileSyncState) -> DirectoryEntry {
    DirectoryEntry { name: "name".into(), state: EntryState::File(state) }
}

fn directory(state: DirectorySyncState) -> DirectoryEntry {
    DirectoryEntry { name: "name".into(), state: EntryState::Directory(state) }
}

fn local_only(kind: EntryKind) -> LocalOnlyEntry {
    let on_disk = DiskEntry { handle: FileHandle { kind: 1, bytes: vec![1, 0, 0, 0, 1, 0, 0, 0] }, metadata: on_disk(kind) };
    LocalOnlyEntry { on_disk, place: EntryPlace { directory: DirectoryReference { onedrive_id: Some("R".into()), path: "".into() }, name: "name".into() } }
}

#[test]
fn a_onedrive_directory_has_the_stores_time_and_no_size() {
    let shown = attributes(7, &AccessMode::read_write(), &directory(DirectorySyncState::InOneDrive(stored(Kind::Folder))));
    assert_eq!((shown.ino.0, shown.kind, shown.size, shown.blocks, shown.nlink), (7, FileType::Directory, 0, 0, 2));
    assert_eq!((shown.atime, shown.mtime, shown.ctime), (at(100), at(100), at(100)));
}

#[test]
fn a_remote_only_file_has_the_stores_size_and_time_and_no_blocks() {
    let shown = attributes(7, &AccessMode::read_write(), &file(FileSyncState::RemoteOnly(stored(Kind::File))));
    assert_eq!((shown.kind, shown.size, shown.blocks, shown.nlink, shown.mtime), (FileType::RegularFile, 42, 0, 1, at(100)));
}

/// A download does not change when the file was last modified: that is OneDrive's to say.
#[test]
fn a_synced_file_has_its_copys_size_and_blocks_and_the_stores_time() {
    let shown = attributes(7, &AccessMode::read_write(), &file(FileSyncState::Synced(stored(Kind::File), on_disk(EntryKind::File))));
    assert_eq!((shown.kind, shown.size, shown.blocks, shown.mtime), (FileType::RegularFile, 10, 8, at(100)));
}

#[test]
fn a_file_modified_here_has_its_copys_size_and_time() {
    let shown = attributes(7, &AccessMode::read_write(), &file(FileSyncState::ModifiedHere(stored(Kind::File), on_disk(EntryKind::File))));
    assert_eq!((shown.size, shown.blocks, shown.atime, shown.mtime, shown.ctime), (10, 8, at(500), at(500), at(500)));
}

#[test]
fn a_local_only_file_has_what_the_filesystem_says_of_it() {
    let shown = attributes(7, &AccessMode::read_write(), &file(FileSyncState::LocalOnly(local_only(EntryKind::File))));
    assert_eq!((shown.kind, shown.size, shown.blocks, shown.mtime), (FileType::RegularFile, 10, 8, at(500)));
}

#[test]
fn a_local_only_directory_has_the_filesystems_blocks_and_time_and_no_size() {
    let shown = attributes(7, &AccessMode::read_write(), &directory(DirectorySyncState::LocalOnly(local_only(EntryKind::Directory))));
    assert_eq!((shown.kind, shown.size, shown.blocks, shown.nlink, shown.mtime), (FileType::Directory, 0, 8, 2, at(500)));
}

/// A read-only folder shows no write bit anywhere, whatever the state of the entry.
#[test]
fn the_modes_are_the_access_modes_in_every_state() {
    let files = [
        file(FileSyncState::RemoteOnly(stored(Kind::File))),
        file(FileSyncState::Synced(stored(Kind::File), on_disk(EntryKind::File))),
        file(FileSyncState::ModifiedHere(stored(Kind::File), on_disk(EntryKind::File))),
        file(FileSyncState::LocalOnly(local_only(EntryKind::File))),
    ];
    for entry in &files {
        assert_eq!((attributes(2, &AccessMode::read_write(), entry).perm, attributes(2, &AccessMode::read_only(), entry).perm), (0o644, 0o444), "{entry:?}");
    }
    let directories = [directory(DirectorySyncState::InOneDrive(stored(Kind::Folder))), directory(DirectorySyncState::LocalOnly(local_only(EntryKind::Directory)))];
    for entry in &directories {
        assert_eq!((attributes(2, &AccessMode::read_write(), entry).perm, attributes(2, &AccessMode::read_only(), entry).perm), (0o755, 0o555), "{entry:?}");
    }
}
