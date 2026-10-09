use crate::fuse::upload_rules::UploadDecision;
use konedrive_tree::outbox::{OutboxKind, OutboxOp, OutboxState};

use super::EntryKeptAside;
use crate::fuse::detections::WriteState;
use crate::fuse::detections::tests::{detection_of, directory, file, in_docs, quiet, rel, NOW};

#[test]
fn a_file_kept_aside_is_created_under_its_conflict_copys_name() {
    let mine = file(1, 3);
    let kept = EntryKeptAside { on_disk: mine.clone(), from: rel("docs/c.txt"), to: in_docs("c-fedora.txt"), upload_decision: UploadDecision::Upload, write_state: WriteState::NotOpen }.detect(NOW);
    assert_eq!(kept.detections, [quiet(detection_of(OutboxKind::Create, &mine, &in_docs("c-fedora.txt"), OutboxState::Ready))]);
    assert!(kept.ops.is_empty());
}

#[test]
fn a_directory_kept_aside_takes_the_rows_below_it_along() {
    let mine = directory(1);
    let kept = EntryKeptAside { on_disk: mine.clone(), from: rel("docs/c.txt"), to: in_docs("c-fedora.txt"), upload_decision: UploadDecision::Upload, write_state: WriteState::NotOpen }.detect(NOW);
    assert_eq!(kept.detections, [detection_of(OutboxKind::Mkdir, &mine, &in_docs("c-fedora.txt"), OutboxState::Ready)]);
    assert_eq!(kept.ops, [OutboxOp::Rebase { from: rel("docs/c.txt"), to: rel("docs/c-fedora.txt") }]);
}

#[test]
fn a_conflict_copy_under_an_ignored_name_is_not_uploaded() {
    let file_kept = EntryKeptAside { on_disk: file(1, 3), from: rel("docs/c.tmp"), to: in_docs("c-fedora.tmp"), upload_decision: UploadDecision::Ignore, write_state: WriteState::NotOpen }.detect(NOW);
    assert!(file_kept.is_empty());
    let directory_kept = EntryKeptAside { on_disk: directory(2), from: rel("docs/c"), to: in_docs("c-fedora"), upload_decision: UploadDecision::Ignore, write_state: WriteState::NotOpen }.detect(NOW);
    assert!(directory_kept.detections.is_empty());
    assert_eq!(directory_kept.ops, [OutboxOp::Rebase { from: rel("docs/c"), to: rel("docs/c-fedora") }]);
}

/// A conflict copy a program still writes waits for its last writer.
#[test]
fn a_file_kept_aside_while_a_program_writes_it_waits_for_its_writer() {
    let kept = EntryKeptAside { on_disk: file(1, 3), from: rel("docs/c.txt"), to: in_docs("c-fedora.txt"), upload_decision: UploadDecision::Upload, write_state: WriteState::OpenForWriting }.detect(NOW);
    assert_eq!(kept.detections.iter().map(|detection| (detection.state, detection.reason.clone())).collect::<Vec<_>>(), [(OutboxState::Waiting, Some(konedrive_tree::outbox::Reason::OpenForWriting))]);
}
