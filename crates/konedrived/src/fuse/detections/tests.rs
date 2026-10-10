//! What the tests of the events share: the drive of the directory contents' tests in a store
//! in memory, disk entries by made-up file handles, and places in the drive.

use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use konedrive_fs::handle::FileHandle;
use konedrive_tree::outbox::{Detection, OutboxKind, OutboxOp, OutboxRow, OutboxState, Reason};
use konedrive_tree::TreeStore;

use crate::fuse::directory_reference::DirectoryReference;
use crate::fuse::entry_place::EntryPlace;
use crate::fuse::local_only_entry::LocalOnlyEntry;
use super::{is_directory_of, FolderEvent, OutboxBatch};
use crate::fuse::backing_directory::{DiskEntry, DiskMetadata, EntryKind};
use crate::fuse::directory_contents::tests::drive;

/// The time the tests decide at.
pub(in crate::fuse) const NOW: i64 = 1_000;

/// The drive of the directory contents' tests (`docs` with `c.txt`; `a.txt` and `b.txt` in
/// the root), with `detections` waiting in the outbox.
pub(in crate::fuse) fn store(detections: Vec<Detection>) -> TreeStore {
    let mut store = TreeStore::in_memory().unwrap();
    drive(&mut store, detections);
    store
}

pub(in crate::fuse) fn handle(n: u8) -> FileHandle {
    FileHandle { kind: 1, bytes: vec![n, 0, 0, 0, 1, 0, 0, 0] }
}

/// A file of `size` bytes on disk, with the handle `n`.
pub(in crate::fuse) fn file(n: u8, size: u64) -> DiskEntry {
    DiskEntry { handle: handle(n), metadata: DiskMetadata { kind: EntryKind::File, size, blocks: 0, modified: UNIX_EPOCH } }
}

/// A directory on disk, with the handle `n`.
pub(in crate::fuse) fn directory(n: u8) -> DiskEntry {
    DiskEntry { handle: handle(n), metadata: DiskMetadata { kind: EntryKind::Directory, size: 4096, blocks: 8, modified: UNIX_EPOCH } }
}

/// `on_disk` as a local-only entry at `place`.
pub(in crate::fuse) fn local_only(on_disk: &DiskEntry, place: &EntryPlace) -> LocalOnlyEntry {
    LocalOnlyEntry { on_disk: on_disk.clone(), place: place.clone() }
}

/// `name` in the folder itself.
pub(in crate::fuse) fn in_root(name: &str) -> EntryPlace {
    EntryPlace { directory: DirectoryReference { onedrive_id: Some("R".into()), path: "".into() }, name: name.into() }
}

/// `name` in `docs`.
pub(in crate::fuse) fn in_docs(name: &str) -> EntryPlace {
    EntryPlace { directory: DirectoryReference { onedrive_id: Some("D".into()), path: "docs".into() }, name: name.into() }
}

/// The detection that the local-only entry `on_disk` is at `place`, in `state`.
pub(in crate::fuse) fn detection_of(kind: OutboxKind, on_disk: &DiskEntry, place: &EntryPlace, state: OutboxState) -> Detection {
    Detection {
        local_item_id: None,
        kind,
        item_id: None,
        object: Some(on_disk.handle.clone()),
        rel: place.path(),
        base: None,
        target_parent: place.directory.onedrive_id.clone(),
        target_name: Some(place.name.clone()),
        same_content: false,
        state,
        reason: None,
        next_try: None,
        size: on_disk.metadata.file_size(),
    }
}

/// `d`, waiting for its last writer.
pub(in crate::fuse) fn writing(mut d: Detection) -> Detection {
    super::wait_for_writer(&mut d, NOW);
    d
}

/// `d`, due a moment later.
pub(in crate::fuse) fn quiet(mut d: Detection) -> Detection {
    super::wait_for_quiet(&mut d, NOW);
    d
}

pub(in crate::fuse) fn detect(event: impl FolderEvent, store: &TreeStore) -> OutboxBatch {
    event.detect(store, NOW).unwrap()
}

/// The outbox's rows, oldest first.
pub(in crate::fuse) fn rows(store: &TreeStore) -> Vec<OutboxRow> {
    store.outbox_rows().unwrap()
}

pub(in crate::fuse) fn removes(rows: &[&OutboxRow]) -> Vec<OutboxOp> {
    rows.iter().map(|row| OutboxOp::Remove(row.seq)).collect()
}

pub(in crate::fuse) fn rel(rel: &str) -> PathBuf {
    PathBuf::from(rel)
}

#[test]
fn the_waiting_detections_are_due_after_the_writer_or_a_quiet_moment() {
    let d = detection_of(OutboxKind::Create, &file(1, 3), &in_root("new.txt"), OutboxState::Ready);
    let w = writing(d.clone());
    assert_eq!((w.state, w.reason, w.next_try), (OutboxState::Waiting, Some(Reason::OpenForWriting), Some(NOW + 30)));
    let q = quiet(d);
    assert_eq!((q.state, q.reason, q.next_try), (OutboxState::Waiting, Some(Reason::Changed), Some(NOW + 2)));
}

/// The directory on disk of a directory that was removed from OneDrive backs it unless it is
/// marked with another OneDrive id.
#[test]
fn a_directory_on_disk_backs_the_removed_directory_unless_it_is_marked_with_another_id() {
    assert!(is_directory_of(None, "D") && is_directory_of(Some("D"), "D"));
    assert!(!is_directory_of(Some("E"), "D"));
}
