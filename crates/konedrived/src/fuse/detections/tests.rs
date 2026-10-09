//! What the tests of the events share: the drive of the view's tests in a store in memory,
//! local objects by made-up file handles, and places in it.

use std::path::{Path, PathBuf};

use konedrive_fs::handle::FileHandle;
use konedrive_tree::outbox::{Detection, OutboxKind, OutboxOp, OutboxRow, OutboxState, Reason};
use konedrive_tree::TreeStore;

use super::{is_directory_of, FolderEvent, FolderPlace, LocalObject, ObjectKind, OutboxBatch};
use crate::fuse::directory_contents::tests::drive;

/// The time the tests decide at.
pub(in crate::fuse) const NOW: i64 = 1_000;

/// The view's drive (`docs` with `c.txt`; `a.txt` and `b.txt` in the root), with
/// `detections` waiting in the outbox.
pub(in crate::fuse) fn store(detections: Vec<Detection>) -> TreeStore {
    let mut store = TreeStore::in_memory().unwrap();
    drive(&mut store, detections);
    store
}

pub(in crate::fuse) fn handle(n: u8) -> FileHandle {
    FileHandle { kind: 1, bytes: vec![n, 0, 0, 0, 1, 0, 0, 0] }
}

pub(in crate::fuse) fn file(n: u8, size: u64) -> LocalObject {
    LocalObject { handle: handle(n), kind: ObjectKind::File { size } }
}

pub(in crate::fuse) fn dir(n: u8) -> LocalObject {
    LocalObject { handle: handle(n), kind: ObjectKind::Directory }
}

/// `name` in the folder itself.
pub(in crate::fuse) fn in_root(name: &str) -> FolderPlace {
    FolderPlace::new(Some("R"), Path::new(""), name)
}

/// `name` in `docs`.
pub(in crate::fuse) fn in_docs(name: &str) -> FolderPlace {
    FolderPlace::new(Some("D"), Path::new("docs"), name)
}

/// The detection that the local object `object` stands at `place`, in `state`.
pub(in crate::fuse) fn local(kind: OutboxKind, object: &LocalObject, place: &FolderPlace, state: OutboxState) -> Detection {
    Detection {
        kind,
        item_id: None,
        object: Some(object.handle.clone()),
        rel: place.rel(),
        base: None,
        target_parent: place.above.clone(),
        target_name: Some(place.name.clone()),
        same_content: false,
        state,
        reason: None,
        next_try: None,
        size: match object.kind {
            ObjectKind::File { size } => Some(size),
            ObjectKind::Directory => None,
        },
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
    let d = local(OutboxKind::Create, &file(1, 3), &in_root("new.txt"), OutboxState::Ready);
    let w = writing(d.clone());
    assert_eq!((w.state, w.reason, w.next_try), (OutboxState::Waiting, Some(Reason::OpenForWriting), Some(NOW + 30)));
    let q = quiet(d);
    assert_eq!((q.state, q.reason, q.next_try), (OutboxState::Waiting, Some(Reason::Changed), Some(NOW + 2)));
}

/// The directory of a folder that went from OneDrive is its own unless it carries another
/// item's id.
#[test]
fn a_directory_is_a_gone_folders_unless_it_carries_another_item() {
    assert!(is_directory_of(None, "D") && is_directory_of(Some("D"), "D"));
    assert!(!is_directory_of(Some("E"), "D"));
}
