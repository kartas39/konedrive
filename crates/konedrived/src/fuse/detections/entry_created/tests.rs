use konedrive_tree::outbox::{OutboxKind, OutboxState, Reason};

use super::EntryCreated;
use crate::fuse::detections::tests::{detect, dir, file, in_docs, in_root, local, quiet, store, writing};
use crate::fuse::directory_contents::tests::of;

#[test]
fn a_new_file_is_created_a_moment_after_it_is_closed() {
    let (store, new) = (store(Vec::new()), file(1, 3));
    let batch = detect(EntryCreated::Fresh { place: in_docs("new.txt"), object: new.clone(), open: false }, &store);
    assert_eq!(batch.detections, [quiet(local(OutboxKind::Create, &new, &in_docs("new.txt"), OutboxState::Ready))]);
    assert!(batch.ops.is_empty());
}

#[test]
fn a_new_file_held_open_waits_for_its_last_writer() {
    let (store, new) = (store(Vec::new()), file(1, 0));
    let batch = detect(EntryCreated::Fresh { place: in_root("new.txt"), object: new.clone(), open: true }, &store);
    assert_eq!(batch.detections, [writing(local(OutboxKind::Create, &new, &in_root("new.txt"), OutboxState::Ready))]);
}

#[test]
fn a_new_directory_is_made_at_once() {
    let (store, new) = (store(Vec::new()), dir(1));
    let batch = detect(EntryCreated::Fresh { place: in_root("new"), object: new.clone(), open: false }, &store);
    assert_eq!(batch.detections, [local(OutboxKind::Mkdir, &new, &in_root("new"), OutboxState::Ready)]);
}

#[test]
fn a_name_onedrive_refuses_or_a_file_too_large_waits_for_the_user() {
    let store = store(Vec::new());
    let refused = detect(EntryCreated::Fresh { place: in_root("a:b.txt"), object: file(1, 3), open: true }, &store);
    assert_eq!((refused.detections[0].state, refused.detections[0].reason.clone()), (OutboxState::Blocked, Some(Reason::NameCharacters)));
    let large = detect(EntryCreated::Fresh { place: in_root("big.iso"), object: file(2, 251 << 30), open: false }, &store);
    assert_eq!((large.detections[0].state, large.detections[0].reason.clone()), (OutboxState::Blocked, Some(Reason::TooLarge)));
}

/// The item keeps its id: its new content goes up as an update, ready at once unless a
/// program still writes it.
#[test]
fn a_file_made_where_an_items_file_was_removed_is_that_items_new_content() {
    let store = store(vec![of(OutboxKind::Delete, "A", "a.txt", "a.txt", (None, None))]);
    let base = store.get(konedrive_tree::Table::Items, "A").unwrap().unwrap();
    let again = file(1, 5);
    let batch = detect(EntryCreated::ReplacingRemovedFile { place: in_root("a.txt"), file: again.clone(), open: false, base: base.clone() }, &store);
    let update = &batch.detections[0];
    assert_eq!((update.kind, update.item_id.as_deref(), update.object.as_ref()), (OutboxKind::Update, Some("A"), Some(&again.handle)));
    assert_eq!((update.state, update.size, update.base.as_ref().and_then(|b| b.name.as_deref())), (OutboxState::Ready, Some(5), Some("a.txt")));
    let open = detect(EntryCreated::ReplacingRemovedFile { place: in_root("a.txt"), file: again, open: true, base }, &store);
    assert_eq!((open.detections[0].state, open.detections[0].reason.clone()), (OutboxState::Waiting, Some(Reason::OpenForWriting)));
}
