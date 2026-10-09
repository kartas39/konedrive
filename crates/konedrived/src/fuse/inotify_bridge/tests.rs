use std::ffi::OsStr;

use konedrive_tree::Kind;

use crate::fuse::node_table::NodeId;
use super::{find_name_before_change, AnnouncementInProgress, NameBeforeChange};

fn appearing(parent: u64, name: &str) -> AnnouncementInProgress {
    AnnouncementInProgress::NameAppearing { parent: NodeId(parent), name: name.into() }
}

fn vanishing(parent: u64, name: &str, node_id: u64, kind: Kind) -> AnnouncementInProgress {
    AnnouncementInProgress::NameVanishing { parent: NodeId(parent), name: name.into(), node_id: NodeId(node_id), kind }
}

fn before(in_progress: Option<&AnnouncementInProgress>, parent: u64, name: &str) -> Option<NameBeforeChange> {
    find_name_before_change(in_progress, NodeId(parent), OsStr::new(name))
}

#[test]
fn with_no_announcement_in_progress_every_name_is_as_the_folder_has_it() {
    assert_eq!(before(None, 1, "a.txt"), None);
}

/// The store has the name already; the kernel passes the create on only if the name is not
/// there yet.
#[test]
fn a_name_that_is_appearing_was_absent() {
    let in_progress = appearing(4, "new.txt");
    assert_eq!(before(Some(&in_progress), 4, "new.txt"), Some(NameBeforeChange::Absent));
}

/// The store has the name no more; the kernel passes the remove on only if the name is
/// still there, and needs its node and its kind.
#[test]
fn a_name_that_is_vanishing_was_present_with_its_node_and_kind() {
    let file = vanishing(4, "gone.txt", 9, Kind::File);
    assert_eq!(before(Some(&file), 4, "gone.txt"), Some(NameBeforeChange::Present { node_id: NodeId(9), kind: Kind::File }));
    let directory = vanishing(1, "gone", 12, Kind::Folder);
    assert_eq!(before(Some(&directory), 1, "gone"), Some(NameBeforeChange::Present { node_id: NodeId(12), kind: Kind::Folder }));
}

#[test]
fn another_name_or_the_same_name_in_another_directory_is_as_the_folder_has_it() {
    for in_progress in [appearing(4, "new.txt"), vanishing(4, "new.txt", 9, Kind::File)] {
        assert_eq!(before(Some(&in_progress), 4, "other.txt"), None);
        assert_eq!(before(Some(&in_progress), 5, "new.txt"), None);
    }
}
