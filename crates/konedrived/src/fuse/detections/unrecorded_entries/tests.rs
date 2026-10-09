
use konedrive_tree::outbox::{OutboxKind, OutboxState};

use crate::fuse::directory_reference::DirectoryReference;
use crate::fuse::entry_place::EntryPlace;
use super::UnrecordedEntries;
use crate::fuse::detections::tests::{detect, detection_of, directory, file, in_root, local_only, quiet, store};

#[test]
fn a_local_only_entry_the_outbox_has_no_row_for_is_recorded_anew() {
    let (known, lost, lost_directory) = (file(1, 1), file(2, 2), directory(3));
    let store = store(vec![detection_of(OutboxKind::Create, &known, &in_root("known.txt"), OutboxState::Ready)]);
    let deep = EntryPlace { directory: DirectoryReference { onedrive_id: None, path: "new".into() }, name: "deep".into() };
    let entries = vec![local_only(&known, &in_root("known.txt")), local_only(&lost, &in_root("lost.txt")), local_only(&lost_directory, &deep)];
    let batch = detect(UnrecordedEntries(entries), &store);
    assert_eq!(batch.detections, [quiet(detection_of(OutboxKind::Create, &lost, &in_root("lost.txt"), OutboxState::Ready)), detection_of(OutboxKind::Mkdir, &lost_directory, &deep, OutboxState::Ready)]);
}
