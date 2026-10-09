use std::path::Path;

use super::FuseConnection;

#[test]
fn a_connection_is_aborted_through_the_file_of_its_number() {
    assert_eq!(FuseConnection { number: 57 }.abort_path(), Path::new("/sys/fs/fuse/connections/57/abort"));
}

/// A plain directory is no mount: the number read there is its filesystem's, and whatever
/// it is, the connection is one value that says the same path twice.
#[test]
fn the_connection_of_a_folder_is_numbered_by_its_device() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(FuseConnection::of_mount_at(dir.path()), FuseConnection::of_mount_at(dir.path()));
}

#[test]
fn a_folder_that_cannot_be_read_is_numbered_as_no_mount() {
    assert_eq!(FuseConnection::of_mount_at(Path::new("/nonexistent/konedrive-folder")), FuseConnection { number: 0 });
}
