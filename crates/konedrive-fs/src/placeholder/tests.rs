use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{
    create_placeholder, punch_all, read_item_id, read_state, stamp_matches, write_stamp,
    write_state, remove_stamp, State, StateError, XATTR_STATE, with_owner_write,
    XATTR_PROGRESS, Progress, write_progress, read_progress, remove_progress, write_ctag,
    read_ctag, LOCKED_FILE_MODE,
    PlaceholderSpec, create_placeholder_with, create_dir_item, reopen_writable, punch_from,
    strip, strip_konedrive_xattrs, write_item_id, combine_op_and_restore_results,
};

fn dir() -> (tempfile::TempDir, File) {
    let dir = tempfile::tempdir().unwrap();
    let handle = File::open(dir.path()).unwrap();
    (dir, handle)
}

#[test]
fn creates_a_sparse_placeholder_with_the_real_size() {
    let (dir, handle) = dir();
    let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    create_placeholder(&handle, "movie.mkv", "ITEM1", 4_700_000_000, mtime).unwrap();

    let path = dir.path().join("movie.mkv");
    let meta = std::fs::metadata(&path).unwrap();
    assert_eq!(meta.len(), 4_700_000_000);
    assert!(meta.blocks() < 64, "expected a sparse file, got {} blocks", meta.blocks());
    assert_eq!(meta.modified().unwrap(), mtime);

    let file = File::open(&path).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
    assert_eq!(read_item_id(&file).unwrap().as_deref(), Some("ITEM1"));
}

#[test]
fn zero_byte_files_are_created_hydrated() {
    let (dir, handle) = dir();
    create_placeholder(&handle, "empty.txt", "ITEM2", 0, SystemTime::now()).unwrap();
    let file = File::open(dir.path().join("empty.txt")).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated));
}

#[test]
fn a_half_built_placeholder_is_never_visible() {
    // linkat is the last step, so the name appears only once everything is set.
    let (dir, handle) = dir();
    create_placeholder(&handle, "doc.pdf", "ITEM3", 1024, SystemTime::now()).unwrap();
    let file = File::open(dir.path().join("doc.pdf")).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
    assert_eq!(read_item_id(&file).unwrap().as_deref(), Some("ITEM3"));
    assert_eq!(file.metadata().unwrap().len(), 1024);
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

/// The distinction the helper's "never allow zeros" rule turns on: a file
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
fn stamp_detects_local_modification() {
    let (dir, _handle) = dir();
    let path = dir.path().join("f.bin");
    std::fs::write(&path, b"12345").unwrap();
    let file = File::options().read(true).write(true).open(&path).unwrap();
    write_stamp(&file).unwrap();
    assert!(stamp_matches(&file).unwrap());

    let mut appended = File::options().append(true).open(&path).unwrap();
    appended.write_all(b"6").unwrap();
    drop(appended);
    let file = File::open(&path).unwrap();
    assert!(!stamp_matches(&file).unwrap(), "a changed file must not match its stamp");
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
    // `fallocate(..., len=0, ...)` always returns EINVAL; dehydration and
    // startup recovery call `punch_all` unconditionally, including on
    // the zero-byte placeholders this crate treats as already hydrated.
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
    write_stamp(&file).unwrap();
    write_ctag(&file, "c1").unwrap();
    write_progress(&file, &Progress { ctag: "c1".into(), bytes: 3 }).unwrap();
    remove_progress(&file).unwrap();
    remove_stamp(&file).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated));
    assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c1"));
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

#[test]
fn a_read_phase_placeholder_carries_its_ctag_and_the_locks_mode() {
    let (dir, handle) = dir();
    let spec = PlaceholderSpec { item_id: "I1", size: 1 << 20, mtime: UNIX_EPOCH + Duration::from_secs(1_700_000_000), ctag: Some("c1"), mode: LOCKED_FILE_MODE };
    create_placeholder_with(&handle, "a.bin", &spec).unwrap();
    let path = dir.path().join("a.bin");
    let file = File::open(&path).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
    assert_eq!(read_item_id(&file).unwrap().as_deref(), Some("I1"));
    assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c1"));
    assert_eq!(mode_of(&path), 0o444);
    assert_eq!(file.metadata().unwrap().modified().unwrap(), spec.mtime);
}

/// An empty file is created `hydrated` — nothing to fetch — and, unlike
/// part 1's placeholders, stamped: a `hydrated` file without a stamp is
/// what `Hydrate()` refills (H109) and what a reconcile would take for a
/// file changed locally.
#[test]
fn an_empty_read_phase_placeholder_is_hydrated_and_stamped() {
    let (dir, handle) = dir();
    let spec = PlaceholderSpec { item_id: "I2", size: 0, mtime: UNIX_EPOCH + Duration::from_secs(1_700_000_000), ctag: Some("c2"), mode: LOCKED_FILE_MODE };
    create_placeholder_with(&handle, "empty", &spec).unwrap();
    let file = File::open(dir.path().join("empty")).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated));
    assert!(stamp_matches(&file).unwrap());
}

#[test]
fn a_folder_is_labelled_before_it_has_its_real_name() {
    let (dir, handle) = dir();
    let made = create_dir_item(&handle, ".konedrive-new-D1", "D1").unwrap();
    assert_eq!(read_item_id(&made).unwrap().as_deref(), Some("D1"));
    assert!(dir.path().join(".konedrive-new-D1").is_dir());
    assert_eq!(mode_of(&dir.path().join(".konedrive-new-D1")), 0o755);
}

#[test]
fn a_locked_file_is_reopened_writable_on_the_same_inode() {
    use std::io::{Seek, SeekFrom};
    use std::os::unix::fs::{FileExt, MetadataExt};
    let (dir, _handle) = dir();
    let path = locked(dir.path(), "f.bin", b"hello");
    let read_only = File::open(&path).unwrap();
    let writable = reopen_writable(&read_only).unwrap();
    let (a, b) = (read_only.metadata().unwrap(), writable.metadata().unwrap());
    assert_eq!((a.dev(), a.ino()), (b.dev(), b.ino()));
    writable.write_all_at(b"J", 0).unwrap();
    let mut back = String::new();
    let mut reader = File::open(&path).unwrap();
    reader.seek(SeekFrom::Start(0)).unwrap();
    reader.read_to_string(&mut back).unwrap();
    assert_eq!(back, "Jello");
    assert_eq!(mode_of(&path), 0o444);
}

#[test]
fn punching_from_an_offset_keeps_what_is_before_it() {
    let (dir, _handle) = dir();
    let path = dir.path().join("big.bin");
    std::fs::write(&path, vec![7u8; 1 << 20]).unwrap();
    let file = File::options().read(true).write(true).open(&path).unwrap();
    punch_from(&file, 256 << 10).unwrap();
    let mut content = Vec::new();
    File::open(&path).unwrap().read_to_end(&mut content).unwrap();
    assert_eq!(content.len(), 1 << 20);
    assert!(content[..256 << 10].iter().all(|b| *b == 7));
    assert!(content[256 << 10..].iter().all(|b| *b == 0));
    punch_from(&file, 2 << 20).unwrap(); // past the end: nothing to do
}

#[test]
fn stripping_leaves_no_konedrive_attribute_and_keeps_the_others() {
    let (dir, _handle) = dir();
    let path = locked(dir.path(), "f.bin", b"x");
    let file = File::open(&path).unwrap();
    write_item_id(&file, "I").unwrap();
    write_state(&file, State::Hydrated).unwrap();
    with_owner_write(&file, || xattr::FileExt::set_xattr(&file, "user.other", b"keep")).unwrap();
    strip_konedrive_xattrs(&file).unwrap();
    let names: Vec<_> = xattr::FileExt::list_xattr(&file).unwrap().collect();
    // Check that no konedrive attributes remain and user.other is preserved
    for name in &names {
        assert!(!name.as_encoded_bytes().starts_with(b"user.konedrive."), "konedrive attribute not stripped: {name:?}");
    }
    assert!(names.iter().any(|n| n == std::ffi::OsStr::new("user.other")), "user.other attribute was not preserved");
}

/// The strip of an object that becomes the user's own: every attribute of
/// konedrive's goes, the item id among them, on a file locked `0444` too, and
/// on a directory; what is not konedrive's stays, and so does the mode. An
/// object with no attribute of konedrive's is left as it is: its mode is not
/// touched even for a moment (its change time stays), so nothing fails on a
/// file the daemon could not change the mode of.
#[test]
fn a_strip_takes_the_id_and_the_rest_and_keeps_the_mode() {
    let (dir, handle) = dir();
    let path = locked(dir.path(), "f.bin", b"x");
    let file = File::open(&path).unwrap();
    write_item_id(&file, "I").unwrap();
    write_state(&file, State::Hydrated).unwrap();
    write_ctag(&file, "c1").unwrap();
    with_owner_write(&file, || xattr::FileExt::set_xattr(&file, "user.other", b"keep")).unwrap();
    strip(&file).unwrap();
    assert_eq!(read_item_id(&file).unwrap(), None);
    let users = |of: &File| -> Vec<_> { xattr::FileExt::list_xattr(of).unwrap().filter(|name| name.as_encoded_bytes().starts_with(b"user.")).collect() };
    assert_eq!(users(&file), [std::ffi::OsString::from("user.other")]);
    assert_eq!(file.metadata().unwrap().mode() & 0o7777, LOCKED_FILE_MODE);
    strip(&file).unwrap();

    let plain = File::open(locked(dir.path(), "plain.bin", b"x")).unwrap();
    let changed = |of: &File| of.metadata().map(|m| (m.ctime(), m.ctime_nsec())).unwrap();
    let before = changed(&plain);
    std::thread::sleep(Duration::from_millis(20));
    strip(&plain).unwrap();
    assert_eq!(changed(&plain), before, "no konedrive attribute: the mode is never lifted");
    assert_eq!(plain.metadata().unwrap().mode() & 0o7777, LOCKED_FILE_MODE);

    write_item_id(&handle, "D").unwrap();
    strip(&handle).unwrap();
    assert!(users(&handle).is_empty());
}

/// A time before 1970 is one a file can carry (`futimens` takes a negative
/// `tv_sec`), so a placeholder for a file dated then is made, with that time.
/// The daemon's read phase never asks for one (it cuts the cloud's time to
/// 1970 first); `PopulateFromDirectory` passes a source file's own time.
#[test]
fn a_placeholder_can_carry_a_time_before_1970() {
    let (dir, handle) = dir();
    let mtime = UNIX_EPOCH - Duration::from_secs(86_400);
    create_placeholder(&handle, "old.txt", "ITEM1", 4096, mtime).unwrap();
    assert_eq!(std::fs::metadata(dir.path().join("old.txt")).unwrap().mtime(), -86_400);
}

/// A time before 1970 that is not on a whole second: `tv_nsec` counts forward
/// from the second before it.
#[test]
fn a_time_before_1970_keeps_its_part_of_a_second() {
    let (dir, handle) = dir();
    create_placeholder(&handle, "old.txt", "ITEM1", 4096, UNIX_EPOCH - Duration::from_millis(1_250)).unwrap();
    let meta = std::fs::metadata(dir.path().join("old.txt")).unwrap();
    assert_eq!((meta.mtime(), meta.mtime_nsec()), (-2, 750_000_000));
}
