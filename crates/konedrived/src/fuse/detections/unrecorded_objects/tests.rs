use konedrive_tree::outbox::{OutboxKind, OutboxState};

use super::UnrecordedObjects;
use crate::fuse::detections::tests::{detect, dir, file, in_root, local, quiet, store};
use crate::fuse::detections::{FolderPlace, LocalEntry};

#[test]
fn a_local_object_the_outbox_has_no_row_for_is_recorded_anew() {
    let (known, lost, lost_dir) = (file(1, 1), file(2, 2), dir(3));
    let store = store(vec![local(OutboxKind::Create, &known, &in_root("known.txt"), OutboxState::Ready)]);
    let entries = vec![
        LocalEntry { place: in_root("known.txt"), object: known },
        LocalEntry { place: in_root("lost.txt"), object: lost.clone() },
        LocalEntry { place: FolderPlace::new(None, std::path::Path::new("new"), "deep"), object: lost_dir.clone() },
    ];
    let batch = detect(UnrecordedObjects(entries), &store);
    let deep = FolderPlace::new(None, std::path::Path::new("new"), "deep");
    assert_eq!(batch.detections, [quiet(local(OutboxKind::Create, &lost, &in_root("lost.txt"), OutboxState::Ready)), local(OutboxKind::Mkdir, &lost_dir, &deep, OutboxState::Ready)]);
}
