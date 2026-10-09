use std::path::PathBuf;

use konedrive_tree::{Kind, Row};

use super::uncommitted_cycle::list_removed_ids;
use super::OneDriveChange;
use crate::fuse::directory_contents::tests::row;

/// The OneDrive id `A` in the directory `D`, placed at `rel`.
pub(super) fn placed(kind: Kind, rel: &str) -> Option<(Row, PathBuf)> {
    let rel = PathBuf::from(rel);
    let name = rel.file_name().unwrap().to_string_lossy().into_owned();
    Some((row("A", Some("D"), &name, kind), rel))
}

#[test]
fn the_ids_a_cycle_removed_are_those_that_vanished() {
    let change = |id: &str, old, new| OneDriveChange::of(id.to_owned(), old, new).unwrap();
    let changes = [
        change("gone", placed(Kind::File, "a.txt"), None),
        change("new", None, placed(Kind::File, "b.txt")),
        change("moved", placed(Kind::File, "c.txt"), placed(Kind::File, "d.txt")),
        change("stayed", placed(Kind::File, "e.txt"), placed(Kind::File, "e.txt")),
        change("gone too", placed(Kind::Folder, "docs"), None),
    ];
    assert_eq!(list_removed_ids(&changes), ["gone", "gone too"]);
}
