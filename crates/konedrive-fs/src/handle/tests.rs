use super::*;

#[test]
fn a_handle_follows_the_inode_through_a_rename_and_not_the_name() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a"), b"one").unwrap();
    let root = File::open(dir.path()).unwrap();
    let before = FileHandle::at(&root, OsStr::new("a")).unwrap();
    std::fs::rename(dir.path().join("a"), dir.path().join("b")).unwrap();
    assert_eq!(FileHandle::at(&root, OsStr::new("b")).unwrap(), before, "the same inode under another name");
    assert_eq!(FileHandle::of(&File::open(dir.path().join("b")).unwrap()).unwrap(), before);
    std::fs::write(dir.path().join("a"), b"two").unwrap();
    assert_ne!(FileHandle::at(&root, OsStr::new("a")).unwrap(), before, "a new inode at the old name");
    assert_eq!(FileHandle::decode(&before.encode()), Some(before));
    assert_eq!(FileHandle::at(&root, OsStr::new("gone")).unwrap_err().raw_os_error(), Some(libc::ENOENT));
}

/// A handle the kernel could never have handed out is refused before any
/// system call: the helper passes the daemon's bytes straight through.
#[test]
fn a_malformed_handle_is_refused_before_the_kernel_sees_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = File::open(dir.path()).unwrap();
    let einval = |handle: FileHandle| {
        assert!(!handle.is_well_formed(), "{handle:?}");
        handle.open(std::os::fd::AsFd::as_fd(&root), libc::O_PATH).unwrap_err().raw_os_error()
    };
    assert_eq!(einval(FileHandle { kind: 1, bytes: Vec::new() }), Some(libc::EINVAL));
    assert_eq!(einval(FileHandle { kind: 1, bytes: vec![0; MAX_HANDLE_BYTES + 1] }), Some(libc::EINVAL));
    assert_eq!(einval(FileHandle { kind: -1, bytes: vec![0; 8] }), Some(libc::EINVAL));
    assert!(FileHandle { kind: 0x4d, bytes: vec![0; MAX_HANDLE_BYTES] }.is_well_formed());
}

#[test]
fn a_symlink_is_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("target"), b"x").unwrap();
    std::os::unix::fs::symlink("target", dir.path().join("link")).unwrap();
    let root = File::open(dir.path()).unwrap();
    // A filesystem may refuse a symlink a handle; it must never give it
    // the target's.
    if let Ok(link) = FileHandle::at(&root, OsStr::new("link")) {
        assert_ne!(link, FileHandle::at(&root, OsStr::new("target")).unwrap());
    }
}
