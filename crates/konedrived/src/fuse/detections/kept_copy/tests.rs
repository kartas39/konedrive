use konedrive_tree::outbox::{OutboxKind, OutboxOp, OutboxState};

use super::KeptCopy;
use crate::fuse::detections::tests::{dir, file, in_docs, local, quiet, rel, NOW};

#[test]
fn a_file_kept_beside_an_item_is_created_under_its_copys_name() {
    let mine = file(1, 3);
    let kept = KeptCopy { object: mine.clone(), from: rel("docs/c.txt"), to: in_docs("c-fedora.txt"), uploaded: true }.detect(NOW);
    assert_eq!(kept.detections, [quiet(local(OutboxKind::Create, &mine, &in_docs("c-fedora.txt"), OutboxState::Ready))]);
    assert!(kept.ops.is_empty());
}

#[test]
fn a_directory_kept_aside_takes_the_rows_below_it_along() {
    let mine = dir(1);
    let kept = KeptCopy { object: mine.clone(), from: rel("docs/c.txt"), to: in_docs("c-fedora.txt"), uploaded: true }.detect(NOW);
    assert_eq!(kept.detections, [local(OutboxKind::Mkdir, &mine, &in_docs("c-fedora.txt"), OutboxState::Ready)]);
    assert_eq!(kept.ops, [OutboxOp::Rebase { from: rel("docs/c.txt"), to: rel("docs/c-fedora.txt") }]);
}

#[test]
fn a_copy_under_a_name_that_stays_here_is_not_uploaded() {
    let file_kept = KeptCopy { object: file(1, 3), from: rel("docs/c.tmp"), to: in_docs("c-fedora.tmp"), uploaded: false }.detect(NOW);
    assert!(file_kept.is_empty());
    let dir_kept = KeptCopy { object: dir(2), from: rel("docs/c"), to: in_docs("c-fedora"), uploaded: false }.detect(NOW);
    assert!(dir_kept.detections.is_empty() && dir_kept.ops.len() == 1);
}
