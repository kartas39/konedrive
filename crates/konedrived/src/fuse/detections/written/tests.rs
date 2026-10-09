use konedrive_tree::outbox::{OutboxKind, OutboxState, Reason};

use super::{ItemWrite, LocalWrite, Written};
use crate::fuse::detections::tests::{detect, file, in_docs, in_root, local, quiet, store, writing};

fn item(write: ItemWrite) -> Written {
    Written::Item { id: "C".into(), place: in_docs("c.txt"), write }
}

fn said(written: Written) -> (OutboxKind, Option<String>, OutboxState, Option<Reason>, bool) {
    let batch = detect(written, &store(Vec::new()));
    let [d] = batch.detections.as_slice() else { panic!("one detection: {batch:?}") };
    (d.kind, d.item_id.clone(), d.state, d.reason.clone(), d.same_content)
}

#[test]
fn an_items_file_opened_for_writing_is_its_new_content_and_waits() {
    assert_eq!(said(item(ItemWrite::Opened(file(1, 4)))), (OutboxKind::Update, Some("C".into()), OutboxState::Waiting, Some(Reason::OpenForWriting), false));
}

#[test]
fn an_items_file_closed_changed_or_cut_is_sent_a_moment_later() {
    let quiet = (OutboxKind::Update, Some("C".into()), OutboxState::Waiting, Some(Reason::Changed), false);
    assert_eq!(said(item(ItemWrite::ClosedChanged(file(1, 4)))), quiet);
    assert_eq!(said(item(ItemWrite::Cut(file(1, 0)))), quiet);
    let batch = detect(item(ItemWrite::Cut(file(1, 0))), &store(Vec::new()));
    assert_eq!((batch.detections[0].object.as_ref(), batch.detections[0].size), (Some(&file(1, 0).handle), Some(0)));
}

/// Nothing written: the item is where it stands, with the base's content — which takes back
/// the row its opening wrote.
#[test]
fn an_items_file_closed_untouched_is_said_to_be_where_it_is_as_it_was() {
    assert_eq!(said(item(ItemWrite::ClosedUntouched)), (OutboxKind::Move, Some("C".into()), OutboxState::Ready, None, true));
    let batch = detect(item(ItemWrite::ClosedUntouched), &store(Vec::new()));
    assert_eq!((batch.detections[0].object.as_ref(), batch.detections[0].target_parent.as_deref()), (None, Some("D")));
}

#[test]
fn an_item_the_base_no_longer_has_is_recorded_as_nothing() {
    let gone = Written::Item { id: "gone".into(), place: in_root("gone.txt"), write: ItemWrite::Opened(file(1, 0)) };
    assert!(detect(gone, &store(Vec::new())).is_empty());
}

/// A local file is created either way: closed with nothing written, it is closed all the
/// same.
#[test]
fn a_local_file_written_is_created_when_its_last_writer_closed_it() {
    let new = file(1, 3);
    let created = |write| detect(Written::LocalFile { place: in_root("new.txt"), file: new.clone(), write }, &store(Vec::new())).detections;
    let ready = local(OutboxKind::Create, &new, &in_root("new.txt"), OutboxState::Ready);
    assert_eq!(created(LocalWrite::Opened), [writing(ready.clone())]);
    assert_eq!(created(LocalWrite::Closed), [quiet(ready.clone())]);
    assert_eq!(created(LocalWrite::Cut), [quiet(ready)]);
}
