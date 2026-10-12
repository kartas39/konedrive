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
