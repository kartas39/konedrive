use std::sync::atomic::Ordering;

use fuser::Errno;
use konedrive_tree::outbox::{OutboxKind, OutboxState, Reason};

use crate::fuse::open_writers::AfterClose;
use crate::fuse::write_operations::tests::{at, folder};

#[test]
fn a_local_file_written_waits_for_its_writer_and_is_due_once_closed() {
    let f = folder(Vec::new());
    f.ops.make(1, "new.txt", false, false).unwrap();
    let node = f.node(1, "new.txt");
    let opened = f.ops.open_to_write(node, false).unwrap();
    assert!(opened.ino == node && opened.id.is_none() && f.writers.open(node));
    assert_eq!(f.rows()[0].reason, Some(Reason::OpenForWriting));
    f.writers.written(node);
    assert_eq!(f.writers.closed(node), AfterClose::ContentChanged);
    f.ops.closed(node, true);
    let row = &f.rows()[0];
    assert_eq!((row.kind, row.state, row.reason.clone()), (OutboxKind::Create, OutboxState::Waiting, Some(Reason::Changed)));
}

/// Opened and closed with nothing written: the outbox lets go of the item again.
#[test]
fn an_items_copy_closed_untouched_leaves_nothing_to_send() {
    let f = folder(Vec::new());
    f.download("a.txt", "A", b"a");
    let node = f.item("A", 1);
    let opened = f.ops.open_to_write(node, false).unwrap();
    assert_eq!(opened.id.as_deref(), Some("A"));
    assert_eq!(f.said(), [(OutboxKind::Update, Some("A".into()), at("a.txt"))]);
    assert_eq!(f.writers.closed(node), AfterClose::ContentUntouched);
    f.ops.closed(node, false);
    assert!(f.rows().is_empty());
}

#[test]
fn an_items_file_that_is_not_downloaded_is_downloaded_before_it_is_written() {
    let f = folder(Vec::new());
    let node = f.item("A", 1);
    assert_eq!(f.ops.needs_download(node, false).map(|(row, rel)| (row.id, rel)), Some(("A".into(), at("a.txt"))));
    assert!(f.ops.needs_download(node, true).is_none());
    assert_eq!(f.ops.open_to_write(node, false).err(), Some(Errno::EIO));
}

/// Cut to nothing it needs no download: the copy is made empty, and is the item's new
/// content.
#[test]
fn an_item_cut_to_nothing_is_its_empty_copy() {
    let f = folder(Vec::new());
    let node = f.item("A", 1);
    f.ops.truncate(node, 0).unwrap();
    assert!(f.under.has_copy(&at("a.txt"), "A"));
    assert_eq!(std::fs::metadata(f.disk.join("a.txt")).unwrap().len(), 0);
    let row = &f.rows()[0];
    assert_eq!((row.kind, row.item_id.as_deref(), row.reason.clone()), (OutboxKind::Update, Some("A"), Some(Reason::Changed)));
}

#[test]
fn a_directory_or_a_read_only_folder_is_not_written() {
    let f = folder(Vec::new());
    assert_eq!(f.ops.open_to_write(f.docs(), false).err(), Some(Errno::EISDIR));
    assert_eq!(f.ops.truncate(f.docs(), 0).err(), Some(Errno::EISDIR));
    f.writable.store(false, Ordering::SeqCst);
    assert_eq!(f.ops.open_to_write(f.item("A", 1), true).err(), Some(Errno::EROFS));
    assert!(f.rows().is_empty());
}
