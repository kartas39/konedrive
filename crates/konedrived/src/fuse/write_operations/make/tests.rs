use std::sync::atomic::Ordering;

use fuser::Errno;
use konedrive_tree::outbox::{OutboxKind, OutboxState, Reason};

use crate::fuse::directory_contents::tests::of;
use crate::fuse::directory_contents::Entry;
use crate::fuse::write_operations::tests::{at, folder};

#[test]
fn a_file_made_is_on_disk_and_waits_in_the_outbox_to_be_created() {
    let f = folder(Vec::new());
    let made = f.ops.make(1, "new.txt", false, false).unwrap();
    assert!(made.opened.is_none() && f.disk.join("new.txt").is_file());
    let rows = f.rows();
    assert_eq!(f.said(), [(OutboxKind::Create, None, at("new.txt"))]);
    assert_eq!((rows[0].state, rows[0].reason.clone(), rows[0].target_parent.as_deref()), (OutboxState::Waiting, Some(Reason::Changed), Some("R")));
    assert!(matches!(f.directory_contents.find_entry(&f.root(), "new.txt"), Some(Entry::Local(_))));
}

#[test]
fn a_file_created_open_is_handed_back_and_waits_for_its_writer() {
    let f = folder(Vec::new());
    let opened = f.ops.make(1, "new.txt", false, true).unwrap().opened.unwrap();
    assert!(f.open_writers.is_open(opened.node_id) && opened.id.is_none() && opened.rel == at("new.txt"));
    assert_eq!(f.rows()[0].reason, Some(Reason::OpenForWriting));
}

#[test]
fn a_directory_made_is_made_in_onedrive_inside_its_items_directory() {
    let f = folder(Vec::new());
    f.ops.make(f.docs(), "new", true, false).unwrap();
    assert!(f.disk.join("docs/new").is_dir());
    assert_eq!(f.backing_directory.read_onedrive_id(&at("docs")).as_deref(), Some("D"));
    assert_eq!(f.said(), [(OutboxKind::Mkdir, None, at("docs/new"))]);
    assert_eq!(f.rows()[0].target_parent.as_deref(), Some("D"));
}

#[test]
fn what_may_not_be_made_is_refused_and_nothing_is_made() {
    let f = folder(Vec::new());
    assert_eq!(f.ops.make(1, "a.txt", false, false).err(), Some(Errno::EEXIST));
    assert_eq!(f.ops.make(1, crate::folder::disk::HOLDING, true, false).err(), Some(Errno::EPERM));
    assert_eq!(f.ops.make(1, ".Trash-1000", true, false).err(), Some(Errno::EPERM));
    assert_eq!(f.ops.make(99, "new.txt", false, false).err(), Some(Errno::ENOENT));
    f.writable.store(false, Ordering::SeqCst);
    assert_eq!(f.ops.make(1, "new.txt", false, false).err(), Some(Errno::EROFS));
    assert_eq!(std::fs::read_dir(&f.disk).unwrap().count(), 0);
    assert!(f.rows().is_empty());
}

#[test]
fn a_name_that_stays_here_is_made_and_not_recorded() {
    let f = folder(Vec::new());
    f.ops.make(1, "x.tmp", false, false).unwrap();
    assert!(f.disk.join("x.tmp").is_file() && f.rows().is_empty());
}

/// Programs replace a file by removing it and making it anew: the item keeps its id.
#[test]
fn a_file_made_where_an_items_file_was_removed_is_that_items_new_content() {
    let f = folder(vec![of(OutboxKind::Delete, "A", "a.txt", "a.txt", (None, None))]);
    f.ops.make(1, "a.txt", false, false).unwrap();
    assert!(f.backing_directory.has_downloaded_copy(&at("a.txt"), "A"));
    assert_eq!(f.said(), [(OutboxKind::Update, Some("A".into()), at("a.txt"))]);
    assert_eq!(f.shown("a.txt"), Some(Some("A".into())));
}
