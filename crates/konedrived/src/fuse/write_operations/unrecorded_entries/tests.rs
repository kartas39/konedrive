use std::fs;
use std::path::PathBuf;

use crate::fuse::write_operations::tests::{at, folder};

/// What has a row keeps the one it has; what stays on this computer gets none.
#[test]
fn the_local_objects_without_a_row_are_recorded_anew() {
    let f = folder(Vec::new());
    f.ops.make(1, "known.txt", false, false).unwrap();
    fs::write(f.disk.join("lost.txt"), b"l").unwrap();
    fs::write(f.disk.join("x.tmp"), b"t").unwrap();
    fs::create_dir(f.disk.join("keep")).unwrap();
    fs::write(f.disk.join("keep/f.txt"), b"f").unwrap();
    f.view.backing(&at("docs")).unwrap();
    fs::write(f.disk.join("docs/mine.txt"), b"m").unwrap();
    f.ops.record_unrecorded_objects();
    let mut places: Vec<PathBuf> = f.rows().into_iter().map(|row| row.rel).collect();
    places.sort();
    assert_eq!(places, [at("docs/mine.txt"), at("keep"), at("keep/f.txt"), at("known.txt"), at("lost.txt")]);
}
