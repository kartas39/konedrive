use crate::fuse::write_operations::EntryToMake;
use std::fs;
use std::path::PathBuf;

use crate::fuse::node_table::NodeId;
use crate::fuse::write_operations::tests::{at, folder};

/// What has a row keeps the one it has; what is ignored gets none.
#[test]
fn the_local_only_entries_without_a_row_are_recorded_anew() {
    let f = folder(Vec::new());
    f.ops.make(NodeId::ROOT, "known.txt", EntryToMake::File).unwrap();
    fs::write(f.disk.join("lost.txt"), b"l").unwrap();
    fs::write(f.disk.join("x.tmp"), b"t").unwrap();
    fs::create_dir(f.disk.join("keep")).unwrap();
    fs::write(f.disk.join("keep/f.txt"), b"f").unwrap();
    f.directory_contents.make_marked_directories(&at("docs")).unwrap();
    fs::write(f.disk.join("docs/mine.txt"), b"m").unwrap();
    f.unrecorded_entry_scan().record_unrecorded_entries();
    let mut paths: Vec<PathBuf> = f.rows().into_iter().map(|row| row.rel).collect();
    paths.sort();
    assert_eq!(paths, [at("docs/mine.txt"), at("keep"), at("keep/f.txt"), at("known.txt"), at("lost.txt")]);
}

/// The walk is asked for when the folder starts taking changes; in one that is read-only by
/// the time it runs, nothing is recorded.
#[test]
fn nothing_is_recorded_anew_in_a_read_only_folder() {
    let f = folder(Vec::new());
    fs::write(f.disk.join("lost.txt"), b"l").unwrap();
    f.access_mode_switch.switch_to_read_only();
    f.unrecorded_entry_scan().record_unrecorded_entries();
    assert!(f.rows().is_empty());
}
