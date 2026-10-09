use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::FuseConnection;

#[test]
fn a_connection_is_aborted_through_the_file_of_its_number() {
    assert_eq!(FuseConnection { number: 57 }.abort_path(), Path::new("/sys/fs/fuse/connections/57/abort"));
}

/// A plain directory is no mount: the number read there is the minor number of the device
/// of its own filesystem, as `stat` gives it.
#[test]
fn the_connection_of_a_folder_is_numbered_by_the_minor_number_of_its_device() {
    let dir = tempfile::tempdir().unwrap();
    let device = std::fs::metadata(dir.path()).unwrap().dev();
    assert_eq!(FuseConnection::of_mount_at(dir.path()), FuseConnection { number: (device & 0xff) as u32 | ((device >> 12) & 0xffff_ff00) as u32 });
}

#[test]
fn a_folder_that_cannot_be_read_is_numbered_as_no_mount() {
    assert_eq!(FuseConnection::of_mount_at(Path::new("/nonexistent/konedrive-folder")), FuseConnection { number: 0 });
}
