use super::FuseConnection;

#[test]
fn a_connection_whose_number_could_not_be_read_is_no_mounts_and_its_abort_fails() {
    let directory = tempfile::tempdir().unwrap();
    // Nothing is there: no device, so no number.
    let connection = FuseConnection::of_mount_at(&directory.path().join("gone"));

    assert_eq!(connection, FuseConnection { number: 0 });
    assert_eq!(connection.abort_path(), std::path::PathBuf::from("/sys/fs/fuse/connections/0/abort"));
    assert!(connection.abort().is_err(), "the kernel has no connection 0");
}
