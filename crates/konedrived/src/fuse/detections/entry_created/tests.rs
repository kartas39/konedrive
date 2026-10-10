use crate::fuse::detections::WriteState;
use konedrive_tree::outbox::{OutboxKind, OutboxState, Reason};

use super::EntryCreated;
use crate::fuse::detections::tests::{detect, detection_of, directory, file, in_docs, in_root, local_only, quiet, store, writing};
use crate::fuse::directory_contents::tests::of;

#[test]
fn a_fresh_file_is_created_a_moment_after_it_is_closed() {
    let (store, new) = (store(Vec::new()), file(1, 3));
    let batch = detect(EntryCreated::Fresh { entry: local_only(&new, &in_docs("new.txt")), write_state: WriteState::NotOpen }, &store);
    assert_eq!(batch.detections, [quiet(detection_of(OutboxKind::Create, &new, &in_docs("new.txt"), OutboxState::Ready))]);
    assert!(batch.ops.is_empty());
}

#[test]
fn a_fresh_file_held_open_waits_for_its_last_writer() {
    let (store, new) = (store(Vec::new()), file(1, 0));
    let batch = detect(EntryCreated::Fresh { entry: local_only(&new, &in_root("new.txt")), write_state: WriteState::OpenForWriting }, &store);
    assert_eq!(batch.detections, [writing(detection_of(OutboxKind::Create, &new, &in_root("new.txt"), OutboxState::Ready))]);
}

#[test]
fn a_fresh_directory_is_made_at_once() {
    let (store, new) = (store(Vec::new()), directory(1));
    let batch = detect(EntryCreated::Fresh { entry: local_only(&new, &in_root("new")), write_state: WriteState::NotOpen }, &store);
    assert_eq!(batch.detections, [detection_of(OutboxKind::Mkdir, &new, &in_root("new"), OutboxState::Ready)]);
}

#[test]
fn a_name_onedrive_refuses_or_a_file_too_large_waits_for_the_user() {
    let store = store(Vec::new());
    let refused = detect(EntryCreated::Fresh { entry: local_only(&file(1, 3), &in_root("a:b.txt")), write_state: WriteState::OpenForWriting }, &store);
    assert_eq!((refused.detections[0].state, refused.detections[0].reason.clone()), (OutboxState::Blocked, Some(Reason::NameCharacters)));
    let large = detect(EntryCreated::Fresh { entry: local_only(&file(2, 251 << 30), &in_root("big.iso")), write_state: WriteState::NotOpen }, &store);
    assert_eq!((large.detections[0].state, large.detections[0].reason.clone()), (OutboxState::Blocked, Some(Reason::TooLarge)));
}

/// The file keeps its OneDrive id: its new content goes up as an update, ready at once
/// unless a program still writes it.
#[test]
fn a_file_replacing_a_removed_onedrive_file_is_that_files_new_content() {
    let store = store(vec![of(OutboxKind::Delete, "A", "a.txt", "a.txt", (None, None))]);
    let base = store.get(konedrive_tree::Table::OneDriveItems, "A").unwrap().unwrap();
    let again = file(1, 5);
    let batch = detect(EntryCreated::ReplacingRemovedFile { place: in_root("a.txt"), file: again.clone(), write_state: WriteState::NotOpen, base: base.clone() }, &store);
    let update = &batch.detections[0];
    assert_eq!((update.kind, update.item_id.as_deref(), update.object.as_ref()), (OutboxKind::Update, Some("A"), Some(&again.handle)));
    assert_eq!((update.state, update.size, update.base.as_ref().and_then(|b| b.name.as_deref())), (OutboxState::Ready, Some(5), Some("a.txt")));
    let open = detect(EntryCreated::ReplacingRemovedFile { place: in_root("a.txt"), file: again, write_state: WriteState::OpenForWriting, base }, &store);
    assert_eq!((open.detections[0].state, open.detections[0].reason.clone()), (OutboxState::Waiting, Some(Reason::OpenForWriting)));
}
