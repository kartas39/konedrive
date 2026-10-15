use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, UNIX_EPOCH};

use super::{
    combine_op_and_restore_results, punch_all, read_progress, read_state, remove_progress, set_mtime,
    with_owner_write, write_ctag, write_progress, write_state, Progress, State, StateError,
    XATTR_CTAG, XATTR_PROGRESS, XATTR_STATE,
};

fn dir() -> (tempfile::TempDir, File) {
    let dir = tempfile::tempdir().unwrap();
    let handle = File::open(dir.path()).unwrap();
    (dir, handle)
}

#[test]
fn state_round_trips_and_unmanaged_files_have_none() {
    let (dir, _handle) = dir();
    let path = dir.path().join("plain.txt");
    std::fs::write(&path, b"x").unwrap();
    let file = File::options().read(true).write(true).open(&path).unwrap();
    assert_eq!(read_state(&file).unwrap(), None);
    write_state(&file, State::Hydrating).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::Hydrating));
    write_state(&file, State::Hydrated).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated));
}

/// A distinction that must not be lost: a file
/// with no state attribute at all is not ours and is none of our business,
/// but a file whose state attribute we cannot make sense of is a managed
/// file in an unknown condition, and the two must not report the same way.
#[test]
fn an_unparseable_state_is_an_error_not_an_absent_one() {
    let (dir, _handle) = dir();
    let path = dir.path().join("corrupt.bin");
    std::fs::write(&path, b"x").unwrap();
    let file = File::options().read(true).write(true).open(&path).unwrap();
    assert_eq!(read_state(&file).unwrap(), None, "no attribute means no attribute");

    for bad in ["", "hydrate", "HYDRATED", "online only", "\u{fffd}"] {
        xattr::FileExt::set_xattr(&file, XATTR_STATE, bad.as_bytes()).unwrap();
        match read_state(&file) {
            Err(StateError::Corrupt(value)) => assert_eq!(value, bad),
            other => panic!("{bad:?} must not be readable as a state: {other:?}"),
        }
    }
}

#[test]
fn punch_all_frees_blocks_and_keeps_the_size() {
    let (dir, _handle) = dir();
    let path = dir.path().join("big.bin");
    std::fs::write(&path, vec![7u8; 1 << 20]).unwrap();
    let file = File::options().read(true).write(true).open(&path).unwrap();
    punch_all(&file).unwrap();

    let meta = std::fs::metadata(&path).unwrap();
    assert_eq!(meta.len(), 1 << 20, "size must be preserved");
    assert!(meta.blocks() < 64, "expected the blocks to be freed, got {}", meta.blocks());
    let mut content = Vec::new();
    File::open(&path).unwrap().read_to_end(&mut content).unwrap();
    assert!(content.iter().all(|b| *b == 0));
}

#[test]
fn punch_all_on_a_zero_length_file_is_a_no_op() {
    // `fallocate(..., len=0, ...)` always returns EINVAL; `punch_all` is
    // given an empty file too, and answers it as done.
    let (dir, _handle) = dir();
    let path = dir.path().join("empty.bin");
    let file =
        File::options().create(true).truncate(true).read(true).write(true).open(&path).unwrap();
    assert_eq!(file.metadata().unwrap().len(), 0);
    punch_all(&file).unwrap();
    assert_eq!(file.metadata().unwrap().len(), 0);
}

fn locked(dir: &std::path::Path, name: &str, content: &[u8]) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    path
}

fn mode_of(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

#[test]
fn combine_op_and_restore_results_all_four_cases() {
    const MODE: u32 = 0o444;

    // Case 1: op ok, restore ok -> return op value
    let result = combine_op_and_restore_results::<i32>(Ok(42), Ok(()), MODE);
    assert!(matches!(result, Ok(42)), "case 1: op ok, restore ok should return op value");

    // Case 2: op ok, restore err -> return error mentioning write succeeded
    let restore_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "fchmod failed");
    let result: std::io::Result<i32> = combine_op_and_restore_results(Ok(42), Err(restore_err), MODE);
    assert!(result.is_err());
    let err_msg = format!("{}", result.unwrap_err());
    assert!(err_msg.contains("write succeeded"), "error should mention write succeeded: {err_msg}");
    assert!(err_msg.contains("0o444") || err_msg.contains("444"), "error should mention mode: {err_msg}");

    // Case 3: op err, restore ok -> return op error
    let op_err = std::io::Error::new(std::io::ErrorKind::Other, "setfattr failed");
    let result: std::io::Result<i32> = combine_op_and_restore_results(Err(op_err), Ok(()), MODE);
    assert!(result.is_err());
    let err_msg = format!("{}", result.unwrap_err());
    assert!(err_msg.contains("setfattr failed"), "error should contain op error: {err_msg}");
    assert!(!err_msg.contains("mode could not be put back"), "should not mention restore in op-only error: {err_msg}");

    // Case 4: op err, restore err -> return error carrying both
    let op_err = std::io::Error::new(std::io::ErrorKind::Other, "setfattr failed");
    let restore_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "fchmod failed");
    let result: std::io::Result<i32> = combine_op_and_restore_results(Err(op_err), Err(restore_err), MODE);
    assert!(result.is_err());
    let err_msg = format!("{}", result.unwrap_err());
    assert!(err_msg.contains("setfattr failed"), "error should contain op error: {err_msg}");
    assert!(err_msg.contains("mode could not be put back"), "error should mention restore failure: {err_msg}");
    assert!(err_msg.contains("0o444") || err_msg.contains("444"), "error should mention mode: {err_msg}");
}

/// Measured on Btrfs: the owner's `setfattr` on a
/// `0444` file fails `EACCES`. Everything the daemon writes as an
/// attribute must still land, and the file must stay `0444`.
#[test]
fn attribute_writes_work_on_a_locked_file_and_leave_it_locked() {
    let (dir, _handle) = dir();
    let path = locked(dir.path(), "f.bin", b"12345");
    let file = File::open(&path).unwrap();
    assert!(
        xattr::FileExt::set_xattr(&file, "user.probe", b"x").is_err(),
        "the premise: a plain attribute write on a 0444 file is refused"
    );
    write_state(&file, State::Hydrated).unwrap();
    write_ctag(&file, "c1").unwrap();
    write_progress(&file, &Progress { ctag: "c1".into(), bytes: 3 }).unwrap();
    remove_progress(&file).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated));
    assert_eq!(xattr::FileExt::get_xattr(&file, XATTR_CTAG).unwrap().as_deref(), Some(&b"c1"[..]));
    assert_eq!(read_progress(&file).unwrap(), None);
    assert_eq!(mode_of(&path), 0o444);
}

#[test]
fn the_mode_is_put_back_even_when_the_write_fails() {
    let (dir, _handle) = dir();
    let path = locked(dir.path(), "f.bin", b"x");
    let file = File::open(&path).unwrap();
    let result: std::io::Result<()> = with_owner_write(&file, || Err(std::io::Error::other("refused")));
    assert!(result.is_err());
    assert_eq!(mode_of(&path), 0o444);
}

#[test]
fn progress_round_trips_and_nonsense_reads_as_none() {
    let (dir, _handle) = dir();
    let path = dir.path().join("f.bin");
    std::fs::write(&path, b"x").unwrap();
    let file = File::options().read(true).write(true).open(&path).unwrap();
    let progress = Progress { ctag: "\"{A B},2\"".into(), bytes: 16 << 20 };
    write_progress(&file, &progress).unwrap();
    assert_eq!(read_progress(&file).unwrap(), Some(progress), "a cTag may contain a space");
    for bad in ["", "123", "c1 x", " 5"] {
        xattr::FileExt::set_xattr(&file, XATTR_PROGRESS, bad.as_bytes()).unwrap();
        assert_eq!(read_progress(&file).unwrap(), None, "{bad:?}");
    }
}

/// A file's time may be before 1970, and not on a whole second: `tv_nsec` counts forward from
/// the second before it.
#[test]
fn a_time_before_1970_keeps_its_part_of_a_second() {
    let (dir, _handle) = dir();
    let path = dir.path().join("old.txt");
    std::fs::write(&path, b"x").unwrap();
    let file = File::options().read(true).write(true).open(&path).unwrap();
    set_mtime(&file, UNIX_EPOCH - Duration::from_secs(86_400)).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().mtime(), -86_400);
    set_mtime(&file, UNIX_EPOCH - Duration::from_millis(1_250)).unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    assert_eq!((meta.mtime(), meta.mtime_nsec()), (-2, 750_000_000));
}
