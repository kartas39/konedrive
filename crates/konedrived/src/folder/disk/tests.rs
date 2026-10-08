

use konedrive_fs::placeholder::XATTR_ROOT;

use super::*;

/// An unlocked registered root, its path, and the `Disk` on it.
fn unlocked_root() -> (tempfile::TempDir, std::path::PathBuf, Disk) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    let root_id = "3a9d5c1e-7f20-4b6a-8e4d-2c1b0a9f8e7d".to_owned();
    xattr::set(&path, XATTR_ROOT, root_id.as_bytes()).unwrap();
    let disk = Disk::open(&SyncRoot { path: path.clone(), root_id }, false).unwrap();
    (dir, path, disk)
}

/// A rename never lands on anything: whatever appeared at the target
/// between a probe and the rename is left alone, and the
/// move fails instead.
#[test]
fn a_rename_never_replaces_what_is_at_the_target() {
    let (_dir, path, disk) = unlocked_root();
    std::fs::write(path.join("a"), b"ours").unwrap();
    std::fs::write(path.join("b"), b"the user's").unwrap();
    let root = disk.dir(Path::new("")).unwrap();
    let err = disk.rename(&root, OsStr::new("a"), &root, OsStr::new("b")).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EEXIST));
    assert_eq!(std::fs::read(path.join("b")).unwrap(), b"the user's");
    assert_eq!(std::fs::read(path.join("a")).unwrap(), b"ours");
}
