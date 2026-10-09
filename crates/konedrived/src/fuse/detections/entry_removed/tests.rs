use konedrive_tree::outbox::{OutboxKind, OutboxState};

use super::EntryRemoved;
use crate::fuse::detections::tests::{detect, detection_of, file, in_root, rel, store};

#[test]
fn a_removed_onedrive_file_is_deleted_against_its_base() {
    let store = store(Vec::new());
    let batch = detect(EntryRemoved::InOneDrive { onedrive_id: "C".into(), path: rel("docs/c.txt") }, &store);
    let [delete] = batch.detections.as_slice() else { panic!("one detection: {batch:?}") };
    assert_eq!((delete.kind, delete.item_id.as_deref(), delete.rel.as_path(), delete.object.as_ref()), (OutboxKind::Delete, Some("C"), rel("docs/c.txt").as_path(), None));
    assert_eq!(delete.base.as_ref().and_then(|b| b.parent.as_deref()), Some("D"));
}

#[test]
fn what_the_base_no_longer_has_leaves_nothing_to_delete() {
    let batch = detect(EntryRemoved::InOneDrive { onedrive_id: "gone".into(), path: rel("gone.txt") }, &store(Vec::new()));
    assert!(batch.is_empty());
}

/// A local-only entry the outbox held is forgotten there; one it never heard of says nothing.
#[test]
fn a_removed_local_only_entry_is_forgotten_by_the_outbox() {
    let new = file(1, 3);
    let store = store(vec![detection_of(OutboxKind::Create, &new, &in_root("new.txt"), OutboxState::Ready)]);
    let batch = detect(EntryRemoved::LocalOnly { handle: new.handle.clone(), path: rel("new.txt") }, &store);
    let [delete] = batch.detections.as_slice() else { panic!("one detection: {batch:?}") };
    assert_eq!((delete.kind, delete.item_id.as_deref(), delete.object.as_ref()), (OutboxKind::Delete, None, Some(&new.handle)));
    let unknown = detect(EntryRemoved::LocalOnly { handle: file(2, 0).handle, path: rel("other.txt") }, &store);
    assert!(unknown.is_empty());
}
