use super::*;

const PEER: u32 = 1000;

fn seen(uid: u32, dev: u64, mode: u32, nlink: u64) -> Seen {
    Seen { uid, dev, ino: 7, mode, nlink }
}

fn dir() -> Seen {
    seen(PEER, 42, libc::S_IFDIR | 0o755, 1)
}

#[test]
fn the_directory_must_be_the_peers_own_on_one_of_its_roots() {
    assert_eq!(check_anchor(PEER, &dir(), true), Ok(()));
    assert_eq!(check_anchor(PEER, &dir(), false), Err(libc::EPERM), "no root on its device");
    assert_eq!(check_anchor(PEER, &seen(1001, 42, libc::S_IFDIR | 0o755, 1), true), Err(libc::EPERM));
    assert_eq!(
        check_anchor(PEER, &seen(PEER, 42, libc::S_IFREG | 0o644, 1), true),
        Err(libc::EPERM),
        "not a directory"
    );
}

#[test]
fn only_the_peers_own_file_or_directory_on_the_same_device_passes() {
    let file = seen(PEER, 42, libc::S_IFREG | 0o644, 1);
    assert_eq!(check_object(PEER, &dir(), &file), Ok(Kind::File));
    assert_eq!(check_object(PEER, &dir(), &dir()), Ok(Kind::Directory));
    assert_eq!(check_object(PEER, &dir(), &seen(1001, 42, libc::S_IFREG | 0o644, 1)), Err(libc::EPERM));
    assert_eq!(
        check_object(PEER, &dir(), &seen(PEER, 43, libc::S_IFREG | 0o644, 1)),
        Err(libc::EPERM),
        "another device: another filesystem, or another Btrfs subvolume"
    );
    for other in [libc::S_IFLNK, libc::S_IFIFO, libc::S_IFSOCK, libc::S_IFCHR, libc::S_IFBLK] {
        assert_eq!(check_object(PEER, &dir(), &seen(PEER, 42, other | 0o644, 1)), Err(libc::EPERM), "{other:o}");
    }
}

#[test]
fn a_deleted_object_is_gone_but_only_the_peers_says_so() {
    assert_eq!(check_object(PEER, &dir(), &seen(PEER, 42, libc::S_IFREG | 0o644, 0)), Err(libc::ESTALE));
    assert_eq!(check_object(PEER, &dir(), &seen(PEER, 42, libc::S_IFDIR | 0o755, 0)), Err(libc::ESTALE));
    assert_eq!(
        check_object(PEER, &dir(), &seen(1001, 42, libc::S_IFREG | 0o644, 0)),
        Err(libc::EPERM),
        "somebody else's deleted file is refused like any of theirs"
    );
}

#[test]
fn a_file_is_opened_read_only_and_non_blocking_a_directory_as_one() {
    let file = open_flags(Kind::File);
    assert_eq!(file & libc::O_ACCMODE, libc::O_RDONLY);
    assert_ne!(file & libc::O_NONBLOCK, 0);
    assert_ne!(file & libc::O_NOFOLLOW, 0);
    assert_eq!(file & libc::O_DIRECTORY, 0);
    let directory = open_flags(Kind::Directory);
    assert_eq!(directory & libc::O_ACCMODE, libc::O_RDONLY);
    assert_ne!(directory & libc::O_DIRECTORY, 0);
}

/// The limits are checked before anything is looked at: a handle the
/// kernel could not have given is `EINVAL`, whatever the directory.
#[test]
fn a_malformed_handle_is_einval_before_anything_else() {
    let tmp = tempfile::tempdir().unwrap();
    let anchor = File::open(tmp.path()).unwrap();
    let never = |_: u64| -> bool { panic!("the roots must not be consulted") };
    for handle in [
        FileHandle { kind: 1, bytes: Vec::new() },
        FileHandle { kind: 1, bytes: vec![0; 129] },
        FileHandle { kind: -1, bytes: vec![0; 8] },
    ] {
        assert_eq!(open(PEER, &anchor, &handle, never).unwrap_err(), libc::EINVAL, "{handle:?}");
    }
}

/// Everything up to the kernel call, on the host: a directory that is not
/// on one of the peer's roots is refused before any handle is opened.
#[test]
fn a_directory_off_the_peers_roots_is_refused_before_the_handle_is_opened() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("f"), b"x").unwrap();
    let anchor = File::open(tmp.path()).unwrap();
    let handle = FileHandle::at(&anchor, std::ffi::OsStr::new("f")).unwrap();
    let me = nix::unistd::geteuid().as_raw();
    assert_eq!(open(me, &anchor, &handle, |_| false).unwrap_err(), libc::EPERM);
    assert_eq!(open(me + 1, &anchor, &handle, |_| true).unwrap_err(), libc::EPERM, "not the peer's");
}
