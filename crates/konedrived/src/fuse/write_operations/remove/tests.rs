use fuser::Errno;
use konedrive_tree::outbox::OutboxKind;

use crate::fuse::write_operations::tests::{at, folder};

#[test]
fn a_onedrive_file_removed_is_deleted_in_onedrive_and_its_copy_is_removed() {
    let f = folder(Vec::new());
    f.download("a.txt", "A", b"a");
    f.ops.remove(1, "a.txt", false).unwrap();
    assert!(!f.disk.join("a.txt").exists());
    assert_eq!(f.said(), [(OutboxKind::Delete, Some("A".into()), at("a.txt"))]);
    assert_eq!(f.shown("a.txt"), None);
}

/// Never sent: its create is taken back, and its node is gone.
#[test]
fn a_local_only_file_removed_is_forgotten_by_the_outbox() {
    let f = folder(Vec::new());
    f.ops.make(1, "new.txt", false, false).unwrap();
    let node_id = f.node_id(1, "new.txt");
    f.ops.remove(1, "new.txt", false).unwrap();
    assert!(!f.disk.join("new.txt").exists() && f.rows().is_empty());
    assert!(f.node_resolver.find_entry(node_id).is_none() && f.node_resolver.find_parent(node_id).is_none());
}

#[test]
fn what_cannot_be_removed_is_refused() {
    let f = folder(Vec::new());
    assert_eq!(f.ops.remove(1, "none.txt", false).err(), Some(Errno::ENOENT));
    assert_eq!(f.ops.remove(1, "docs", false).err(), Some(Errno::EISDIR));
    assert_eq!(f.ops.remove(1, "a.txt", true).err(), Some(Errno::ENOTDIR));
    assert_eq!(f.ops.remove(1, "docs", true).err(), Some(Errno::ENOTEMPTY));
    assert!(f.rows().is_empty());
}
