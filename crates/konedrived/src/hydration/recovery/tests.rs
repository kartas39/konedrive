use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use std::ffi::OsString;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use konedrive_fs::placeholder::{read_progress, read_stamp, read_state, write_progress, write_stamp, Progress, State, write_state};
use nix::sys::socket::{bind, socket, AddressFamily, SockFlag, SockType, UnixAddr};
use konedrive_fs::MAX_DEPTH;
use nix::sys::stat::Mode;
use xattr::FileExt;

use crate::helper::{Clearance, HelperLink};
use crate::folder::locks::{InodeKey, InodeLocks};
use crate::hydration::dehydrate::tests::{asked_to, blocks_of, connected, fake_helper, open_rw, with_syscalls_denied};
use crate::folder::root::tests::test_root;
use crate::folder::root::{SyncRoot, uuid_v4};
use super::*;

/// The recovery every test in this module runs: through a link to its
/// fake helper, which is what an intercepted root recovers through.
/// Shadows `super::recover` on purpose, so the tests read as they did
/// before recovery took a [`Clearance`].
async fn recover(link: &HelperLink, root: &SyncRoot) -> Result<RecoveryReport, RecoveryError> {
    super::recover(&Clearance::Link(link.clone()), root, &InodeLocks::new()).await
}

// --- Startup recovery -------------------------------------

/// A file in the state a crash left it in: content on disk, the state
/// xattr saying what was happening to it, and the stamp a finished
/// hydration would have written — which `docs/design/hydration.md` §9 requires recovery to remove.
fn interrupted_file(dir: &Path, name: &str, state: State, size: usize) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, vec![3u8; size]).unwrap();
    let file = open_rw(&path);
    write_state(&file, state).unwrap();
    write_stamp(&file).unwrap();
    path
}

fn state_of(path: &Path) -> Option<State> {
    read_state(&File::open(path).unwrap()).unwrap()
}

/// The original proposal's own scenario, extended over a real tree: a crash
/// mid-hydration and mid-dehydration each leave a file with content that
/// must not be trusted, at the top of the root and two levels down.
/// Both are punched back to `online-only` and lose their stamps; a clean
/// `hydrated` file, an ordinary `online-only` placeholder, a file that is
/// not ours at all, a symlink and a FIFO are left exactly as found.
#[tokio::test]
async fn interrupted_work_is_reset_to_online_only() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), 0, || {});
    let link = connected(&socket_path).await;

    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let nested = root.path.join("sub");
    std::fs::create_dir(&nested).unwrap();
    let deeper = nested.join("deeper");
    std::fs::create_dir(&deeper).unwrap();

    // One directly in the root: 's own test put all four inside
    // `sub/`, so nothing pinned that the root's own files are walked.
    interrupted_file(&root.path, "top.bin", State::Hydrating, 8192);
    for (name, state) in [
        ("a.bin", State::Hydrating),
        ("b.bin", State::Dehydrating),
        ("c.bin", State::Hydrated),
        ("d.bin", State::OnlineOnly),
    ] {
        interrupted_file(&nested, name, state, 8192);
    }
    interrupted_file(&deeper, "e.bin", State::Dehydrating, 8192);

    // None of these are ours, and none of them may be counted or touched.
    std::fs::write(root.path.join("theirs.txt"), vec![7u8; 8192]).unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let outside = interrupted_file(elsewhere.path(), "outside.bin", State::Hydrating, 8192);
    std::os::unix::fs::symlink(&outside, root.path.join("pointer.bin")).unwrap();
    nix::unistd::mkfifo(&root.path.join("pipe"), Mode::S_IRUSR | Mode::S_IWUSR).unwrap();

    let report = recover(&link, &root).await.unwrap();

    assert_eq!(
        report,
        RecoveryReport { scanned: 6, reset: 4, failed: 0, skipped: 0, busy: 0, deferred: 0 },
        "six managed files at three levels, four of them interrupted"
    );
    for path in [
        root.path.join("top.bin"),
        nested.join("a.bin"),
        nested.join("b.bin"),
        deeper.join("e.bin"),
    ] {
        let file = File::open(&path).unwrap();
        assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly), "{path:?}");
        assert_eq!(file.metadata().unwrap().len(), 8192, "{path:?}: size preserved");
        assert!(file.metadata().unwrap().blocks() < 64, "{path:?}: content discarded");
        assert_eq!(
            read_stamp(&file).unwrap(),
            None,
            "{path:?}: the stamp described a hydrated file and must not outlive it"
        );
    }
    for (path, state) in [
        (nested.join("c.bin"), Some(State::Hydrated)),
        (nested.join("d.bin"), Some(State::OnlineOnly)),
        (root.path.join("theirs.txt"), None),
    ] {
        assert_eq!(state_of(&path), state, "{path:?}");
        assert!(blocks_of(&path) > 0, "{path:?} was punched and should not have been");
    }
    assert_eq!(state_of(&outside), Some(State::Hydrating), "a symlink led out of the root");
    assert!(blocks_of(&outside) > 0, "a symlink led out of the root");
}

#[tokio::test]
async fn an_empty_root_is_reported_as_nothing_to_do() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = fake_helper(socket_path.clone(), 0, || {});
    let link = connected(&socket_path).await;

    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    std::fs::create_dir(root.path.join("empty-sub")).unwrap();

    assert_eq!(recover(&link, &root).await.unwrap(), RecoveryReport::default());
    assert!(
        helper.recv_timeout(Duration::from_millis(200)).is_err(),
        "the helper must not be asked anything when there is nothing to recover"
    );
}

/// The recovery half of `dehydration_keeps_the_remote_mtime`:
/// `fallocate` moves the mtime to now. A whole tree of files that
/// recovery touched then looks locally modified — and every one of them
/// is a hole full of zeros, which is an upload-over-remote hazard the
/// moment a delta engine exists. The stamp is gone by then, so
/// `dehydrate`'s "modified locally" guard is not what saves you.
#[tokio::test]
async fn recovery_keeps_the_mtime_of_the_file_it_resets() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), 0, || {});
    let link = connected(&socket_path).await;

    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let path = interrupted_file(&root.path, "f.bin", State::Dehydrating, 1 << 20);
    let remote = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let file = open_rw(&path);
    file.set_times(std::fs::FileTimes::new().set_modified(remote)).unwrap();
    drop(file);

    assert_eq!(recover(&link, &root).await.unwrap().reset, 1);

    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        remote,
        "recovery must not move the file's mtime to now"
    );
}

/// A `hydrating` file whose download left a checkpoint goes back
/// to `online-only` with the checkpointed prefix and the checkpoint kept;
/// the next open resumes. One without a checkpoint is reset as before.
#[tokio::test]
async fn recovery_keeps_a_checkpointed_prefix_and_empties_the_rest() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), 0, || {});
    let link = connected(&socket_path).await;
    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());

    let kept = interrupted_file(&root.path, "kept.bin", State::Hydrating, 1 << 20);
    write_progress(&open_rw(&kept), &Progress { ctag: "c1".into(), bytes: 256 * 1024 }).unwrap();
    let reset = interrupted_file(&root.path, "reset.bin", State::Hydrating, 1 << 20);
    let mtime_before = std::fs::metadata(&kept).unwrap().modified().unwrap();

    let report = recover(&link, &root).await.unwrap();
    assert_eq!(report.reset, 2);

    let file = File::open(&kept).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
    assert_eq!(read_progress(&file).unwrap(), Some(Progress { ctag: "c1".into(), bytes: 256 * 1024 }));
    assert_eq!(read_stamp(&file).unwrap(), None);
    let mut content = Vec::new();
    std::io::Read::read_to_end(&mut File::open(&kept).unwrap(), &mut content).unwrap();
    assert!(content[..256 * 1024].iter().all(|b| *b == 3), "the prefix is kept");
    assert!(content[256 * 1024..].iter().all(|b| *b == 0), "the rest is punched");
    assert_eq!(std::fs::metadata(&kept).unwrap().modified().unwrap(), mtime_before);

    assert_eq!(state_of(&reset), Some(State::OnlineOnly));
    assert!(blocks_of(&reset) < 64);
    assert_eq!(read_progress(&File::open(&reset).unwrap()).unwrap(), None);
}

/// Under the read-only lock every file is 0444; recovery must
/// still reset one, and leave it 0444.
#[tokio::test]
async fn recovery_resets_a_locked_file_and_leaves_it_locked() {
    use std::os::unix::fs::PermissionsExt;
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), 0, || {});
    let link = connected(&socket_path).await;
    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let path = interrupted_file(&root.path, "locked.bin", State::Dehydrating, 8192);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();

    let report = recover(&link, &root).await.unwrap();

    assert_eq!((report.reset, report.skipped), (1, 0), "{report:?}");
    assert_eq!(state_of(&path), Some(State::OnlineOnly));
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o444);
}

/// A file that is not ours is never made writable, not even for a moment,
/// just because recovery walked past it.
#[tokio::test]
async fn recovery_does_not_touch_the_mode_of_a_file_that_is_not_ours() {
    use std::os::unix::fs::MetadataExt;
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), 0, || {});
    let link = connected(&socket_path).await;
    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let theirs = root.path.join("theirs.txt");
    std::fs::write(&theirs, b"x").unwrap();
    std::fs::set_permissions(&theirs, std::os::unix::fs::PermissionsExt::from_mode(0o444)).unwrap();
    let ctime = |path: &Path| {
        let meta = std::fs::metadata(path).unwrap();
        (meta.ctime(), meta.ctime_nsec())
    };
    let ctime_before = ctime(&theirs);
    recover(&link, &root).await.unwrap();
    assert_eq!(ctime(&theirs), ctime_before, "a chmod changes the ctime");
}

/// The recovery half of
/// `dehydrate_refuses_anything_that_is_not_a_file_inside_the_root`: a
/// root whose registration has gone is not a root, and nothing inside it
/// may be emptied on the strength of a path that used to be one.
#[tokio::test]
async fn recovery_refuses_a_root_that_is_no_longer_registered() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = fake_helper(socket_path.clone(), 0, || {});
    let link = connected(&socket_path).await;

    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let path = interrupted_file(&root.path, "f.bin", State::Hydrating, 8192);
    let unregistered = SyncRoot { path: root.path.clone(), root_id: uuid_v4() };

    let error = recover(&link, &unregistered).await.unwrap_err();
    assert!(matches!(error, RecoveryError::NotRegistered(_)), "{error:?}");
    assert!(blocks_of(&path) > 0, "a root that is not registered punched a file");
    assert!(
        helper.recv_timeout(Duration::from_millis(200)).is_err(),
        "the helper must not be asked about a root this daemon does not hold"
    );

    // And the same folder, still registered, is recovered normally.
    assert_eq!(recover(&link, &root).await.unwrap().reset, 1);
}

/// at the level of one name: what is opened is decided by the
/// `fstat` of the descriptor, not by the `d_type` the listing offered.
/// A symlink is refused by `O_NOFOLLOW` before it resolves anywhere, a
/// FIFO and a directory are never handed back as files, and an entry on
/// another filesystem — a bind mount or a removable disk mounted inside
/// the sync folder — is reported as `Entry::OtherFilesystem` rather than
/// opened as this root's content, because the helper's own validation is
/// scoped to the root's device and would not catch a punch across one.
#[test]
fn open_entry_refuses_anything_that_is_not_a_regular_file_on_the_root_device() {
    let dir = tempfile::tempdir().unwrap();
    let handle = File::open(dir.path()).unwrap();
    let dev = handle.metadata().unwrap().dev();

    std::fs::write(dir.path().join("f.bin"), b"x").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("f.bin"), dir.path().join("link")).unwrap();
    nix::unistd::mkfifo(&dir.path().join("pipe"), Mode::S_IRUSR | Mode::S_IWUSR).unwrap();

    let of = |name: &str| {
        let kind = std::fs::symlink_metadata(dir.path().join(name)).unwrap().file_type();
        (OsString::from(name), kind)
    };

    let (name, kind) = of("f.bin");
    assert!(matches!(open_entry(&handle, &name, kind, dev).unwrap(), Entry::File(..)));
    assert!(
        matches!(open_entry(&handle, &name, kind, dev + 1).unwrap(), Entry::OtherFilesystem),
        "a file on another filesystem must be reported, not silently opened as this root's \
         content"
    );

    let (name, kind) = of("sub");
    assert!(matches!(open_entry(&handle, &name, kind, dev).unwrap(), Entry::Directory(_)));
    assert!(matches!(
        open_entry(&handle, &name, kind, dev + 1).unwrap(),
        Entry::OtherFilesystem
    ));

    for name in ["link", "pipe"] {
        let (name, kind) = of(name);
        assert!(
            matches!(open_entry(&handle, &name, kind, dev).unwrap(), Entry::Elsewhere),
            "{name:?}"
        );
        // And the same entry lied about, as a mid-walk swap would: the
        // listing said "regular file", the thing on disk is not one.
        let lying = std::fs::metadata(dir.path().join("f.bin")).unwrap().file_type();
        assert!(
            matches!(open_entry(&handle, &name, lying, dev).unwrap(), Entry::Elsewhere),
            "{name:?} was accepted as a file because the listing claimed it was one"
        );
    }
}

/// The recovery twin of
/// `dehydrate_punches_the_file_it_checked_even_if_the_name_is_taken_over`
/// — the exact defect that destroyed 300 KiB of real data three runs out
/// of three, in the one place that would reintroduce it invisibly. The
/// file is replaced while recovery waits for the helper's `ClearIgnore`
/// ack; a by-path reopen after that point punches the replacement, which
/// was never classified, carries no konedrive xattr, and is somebody's
/// fresh work.
#[tokio::test]
async fn recovery_punches_the_file_it_classified_even_if_the_name_is_taken_over() {
    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let path = interrupted_file(&root.path, "f.bin", State::Hydrating, 300 * 1024);
    // Both of these live outside the root, so that the walk has exactly
    // one name to look at and the outcome cannot depend on the order the
    // directory happens to be read in. `original` is a second name for
    // the same inode, so the file the walk classified stays reachable
    // after the swap; a hard link is not an open descriptor, so it does
    // not disturb the write lease.
    let staging = tempfile::tempdir().unwrap();
    let original = staging.path().join("original.link");
    std::fs::hard_link(&path, &original).unwrap();
    let swap_in = staging.path().join("replacement.tmp");
    std::fs::write(&swap_in, vec![0xABu8; 300 * 1024]).unwrap();

    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let swapped_to = path.clone();
    let _helper = fake_helper(socket_path.clone(), 0, move || {
        std::fs::rename(&swap_in, &swapped_to).unwrap();
    });
    let link = connected(&socket_path).await;

    let report = recover(&link, &root).await.unwrap();
    assert_eq!(report, RecoveryReport { scanned: 1, reset: 1, failed: 0, skipped: 0, busy: 0, deferred: 0 });

    let replacement = std::fs::read(&path).unwrap();
    assert!(
        replacement.iter().all(|b| *b == 0xAB),
        "the replacement file was punched: {} of its {} bytes are zero",
        replacement.iter().filter(|b| **b == 0).count(),
        replacement.len()
    );
    assert!(blocks_of(&path) > 64, "the replacement file lost its blocks");
    assert_eq!(
        state_of(&path),
        None,
        "a file konedrive never managed was stamped by the recovery"
    );

    assert!(blocks_of(&original) < 64, "the file that was classified was not punched");
    assert_eq!(state_of(&original), Some(State::OnlineOnly));
}

/// Reproduced: `sub/` is replaced by a symlink to a
/// directory outside the root while recovery waits for a `ClearIgnore`
/// ack. A walk that re-resolves subdirectory paths follows it and empties
/// a file that was never inside any sync root —
/// `report=RecoveryReport { reset: 2, scanned: 2 }`, `victim blocks=0
/// state=Some(OnlineOnly)`. The helper does not back this out: its check
/// is same-uid-same-filesystem, which any file in the user's home passes.
///
/// Either order of the two names in the root defeats the attack now: if
/// `sub` is reached first the walk already holds its descriptor, and if
/// `top.bin` is reached first the swapped-in symlink is refused by
/// `O_NOFOLLOW`.
#[tokio::test]
async fn recovery_stays_inside_the_root_when_a_directory_is_swapped_mid_walk() {
    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let nested = root.path.join("sub");
    std::fs::create_dir(&nested).unwrap();
    interrupted_file(&root.path, "top.bin", State::Hydrating, 8192);
    interrupted_file(&nested, "inside.bin", State::Hydrating, 8192);

    let elsewhere = tempfile::tempdir().unwrap();
    let victim = interrupted_file(elsewhere.path(), "victim.bin", State::Hydrating, 8192);

    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let decoy = elsewhere.path().to_path_buf();
    let swapped = nested.clone();
    let stashed = root.path.join("sub.stashed");
    let _helper = fake_helper(socket_path.clone(), 0, move || {
        std::fs::rename(&swapped, &stashed).unwrap();
        std::os::unix::fs::symlink(&decoy, &swapped).unwrap();
    });
    let link = connected(&socket_path).await;

    let report = recover(&link, &root).await.unwrap();

    assert_eq!(
        state_of(&victim),
        Some(State::Hydrating),
        "a file outside the root was relabelled by recovery: {report:?}"
    );
    assert!(
        blocks_of(&victim) > 0,
        "a file outside the root was emptied by recovery: {report:?}"
    );
}

/// `list_dir(dir).await?` and `entry?` used to propagate out
/// of `recover`, so one mode-`000` subdirectory returned `Err(EACCES)`
/// for the whole root: the count of what had already been punched was
/// lost and no sibling subtree was ever visited. A directory removed
/// while the daemon starts did the same with `ENOENT`, which is a
/// routine race, not a corruption. Recovery is the one component whose
/// entire job is coping with a messy on-disk state.
#[tokio::test]
async fn an_unreadable_subdirectory_does_not_stop_the_walk() {
    use std::os::unix::fs::PermissionsExt;

    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), 0, || {});
    let link = connected(&socket_path).await;

    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let locked = root.path.join("locked");
    std::fs::create_dir(&locked).unwrap();
    interrupted_file(&locked, "hidden.bin", State::Hydrating, 8192);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let sibling = root.path.join("sibling");
    std::fs::create_dir(&sibling).unwrap();
    let reachable = interrupted_file(&sibling, "reachable.bin", State::Hydrating, 8192);

    let report = recover(&link, &root).await.unwrap();

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        report,
        RecoveryReport { scanned: 1, reset: 1, failed: 0, skipped: 1, busy: 0, deferred: 0 },
        "the sibling subtree must still be recovered, and the unreachable directory said so"
    );
    assert_eq!(state_of(&reachable), Some(State::OnlineOnly));
    assert!(blocks_of(&reachable) < 64);
    assert_eq!(
        state_of(&locked.join("hidden.bin")),
        Some(State::Hydrating),
        "what could not be reached must be left for the next start"
    );
}

/// The other way a directory goes unread: it was opened while it was
/// readable and stopped being readable before it was listed. The listing
/// goes through `/proc/self/fd/<n>`, which re-checks permission on the
/// inode, so this is a real outcome rather than a theoretical one — and
/// like every other unreachable thing it is counted, and the walk
/// continues with whatever else is on the stack.
#[tokio::test]
async fn a_directory_that_cannot_be_listed_is_counted_and_does_not_end_the_walk() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    interrupted_file(&sub, "inside.bin", State::Hydrating, 8192);
    let handle = File::open(&sub).unwrap();
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000)).unwrap();

    let mut stack = Vec::new();
    let mut report = RecoveryReport::default();
    descend(&mut stack, Arc::new(handle), sub.clone(), 0, &mut report).await;

    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(stack.is_empty(), "a directory that could not be listed must not be walked");
    assert_eq!(report, RecoveryReport { scanned: 0, reset: 0, failed: 0, skipped: 1, busy: 0, deferred: 0 });
}

/// other half. A file that cannot be opened was `Err(_) =>
/// continue`: no error, no log, no count — which is how 991 files stayed
/// `hydrating` with untrusted content while the report said
/// `reset: 1009, scanned: 1009`. Whatever the reason (permissions,
/// `EMFILE`, a race with deletion), a file recovery could not look at may
/// be hiding an interrupted one, and the report has to say so.
#[tokio::test]
async fn a_file_that_cannot_be_opened_is_counted_not_passed_over_in_silence() {
    use std::os::unix::fs::PermissionsExt;

    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), 0, || {});
    let link = connected(&socket_path).await;

    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let locked = interrupted_file(&root.path, "locked.bin", State::Hydrating, 8192);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let reachable = interrupted_file(&root.path, "reachable.bin", State::Hydrating, 8192);

    let report = recover(&link, &root).await.unwrap();

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(report, RecoveryReport { scanned: 1, reset: 1, failed: 0, skipped: 1, busy: 0, deferred: 0 });
    assert_eq!(state_of(&reachable), Some(State::OnlineOnly));
    assert_eq!(state_of(&locked), Some(State::Hydrating));
}

/// Streaming has a second consequence worth pinning: because only one
/// file is open at a time, everything else in the directory is still
/// just a name when the walk is waiting on the helper. Here both files
/// are deleted at that moment — the one being recovered continues on its
/// descriptor, and the one that was never reached is counted rather than
/// ending the walk. A user deleting a folder while the daemon starts is
/// routine.
#[tokio::test]
async fn a_file_that_disappears_mid_walk_is_counted_and_the_walk_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let first = interrupted_file(&root.path, "one.bin", State::Hydrating, 8192);
    let second = interrupted_file(&root.path, "two.bin", State::Hydrating, 8192);

    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), 0, move || {
        std::fs::remove_file(&first).unwrap();
        std::fs::remove_file(&second).unwrap();
    });
    let link = connected(&socket_path).await;

    let report = recover(&link, &root).await.unwrap();

    assert_eq!(
        report,
        RecoveryReport { scanned: 1, reset: 1, failed: 0, skipped: 1, busy: 0, deferred: 0 },
        "one file was open and was finished; the other was still only a name"
    );
}

/// `read_state(&file).unwrap_or(None)` collapsed `Corrupt` and genuine
/// I/O errors into "not one of ours". Safe in direction — a file whose
/// state cannot be read must never be punched — but the file was then
/// not counted, not logged and never noticed, while §5.1 has the helper
/// deny every open of it with `EIO` for as long as it stays that way.
#[tokio::test]
async fn recovery_never_punches_a_file_whose_state_it_cannot_read() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = fake_helper(socket_path.clone(), 0, || {});
    let link = connected(&socket_path).await;

    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let path = interrupted_file(&root.path, "corrupt.bin", State::Hydrating, 8192);
    open_rw(&path).set_xattr("user.konedrive.state", b"hydratin").unwrap();

    let report = recover(&link, &root).await.unwrap();

    assert_eq!(
        report,
        RecoveryReport { scanned: 0, reset: 0, failed: 0, skipped: 1, busy: 0, deferred: 0 },
        "a managed file in an unknown state is neither ours to punch nor ours to ignore"
    );
    assert!(blocks_of(&path) > 0, "a file whose state could not be read was punched");
    assert!(
        helper.recv_timeout(Duration::from_millis(200)).is_err(),
        "nothing may be asked of the helper about a file that must not be touched"
    );
}

/// `punch_clean_file` takes a write lease before it empties
/// anything, so that an application opening the file mid-punch is
/// suspended by the kernel instead of reading blocks as they go away.
/// The window is wider at startup, not narrower: the helper's
/// `register_root` walk must mark the whole tree before anything is
/// intercepted at all, so until it finishes any thumbnailer, backup or
/// indexer can hold an interrupted file open while this runs. A refusal
/// means exactly that, and the file waits for the next start — or for
/// the open that has it, which fills it. It is counted busy, not failed.
#[tokio::test]
async fn recovery_leaves_a_file_that_is_in_use_for_the_next_start() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), 0, || {});
    let link = connected(&socket_path).await;

    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let path = interrupted_file(&root.path, "busy.bin", State::Hydrating, 1 << 20);
    let held_open = File::open(&path).unwrap();

    let report = recover(&link, &root).await.unwrap();

    assert_eq!(
        report,
        RecoveryReport { scanned: 1, reset: 0, failed: 0, skipped: 0, busy: 1, deferred: 0 },
        "a file in use is busy, not a failure to recover (the final review's m11): the next \
         open fills it, and the next start resets it if it is still interrupted"
    );
    assert_eq!(
        state_of(&path),
        Some(State::Hydrating),
        "a file nobody could take a lease on must be left exactly as found"
    );
    assert!(blocks_of(&path) > 64, "a file open in another process was emptied under it");

    // And once it is closed, the next start finishes the job.
    drop(held_open);
    assert_eq!(recover(&link, &root).await.unwrap().reset, 1);
    assert_eq!(state_of(&path), Some(State::OnlineOnly));
}

/// An open file of the inode that is on its way out — here one closed
/// 30 ms after the helper is asked to clear the mark, so it is still
/// there when recovery first asks for the lease — does not make the file
/// `busy`: the refusal is retried briefly (`lease_retrying_briefly`). A
/// process being spawned holds exactly such a copy of the walk's own
/// read-only descriptor until its `exec`.
#[tokio::test]
async fn recovery_waits_out_an_open_that_is_about_to_close() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let path = interrupted_file(&root.path, "x.bin", State::Hydrating, 8192);
    let opened = path.clone();
    let _helper = fake_helper(socket_path.clone(), 0, move || {
        let held = File::open(&opened).unwrap();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            drop(held);
        });
    });
    let link = connected(&socket_path).await;

    let report = recover(&link, &root).await.unwrap();

    assert_eq!((report.reset, report.busy), (1, 0), "{report:?}");
    assert_eq!(state_of(&path), Some(State::OnlineOnly));
}

/// After a reconnect the previous
/// connection's fills keep running while the new connection's recovery
/// walks, and recovery read a file's state only when it opened it. A fill
/// that commits `hydrated` — and closes its descriptor — between
/// recovery's `ClearIgnore` and its lease left recovery punching a
/// complete, `hydrated` file; in the VM an opener had the file
/// ignore-marked in that gap, and the next reader got 65 536 zero bytes
/// after no fetch. The fake helper commits the fill when it is asked to
/// clear the mark, which is exactly that gap.
#[tokio::test]
async fn recovery_does_not_punch_a_file_a_fill_finished_after_it_looked() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let path = interrupted_file(&root.path, "x.bin", State::Hydrating, 1 << 20);
    let committed = path.clone();
    let _helper = fake_helper(socket_path.clone(), 0, move || {
        let file = open_rw(&committed);
        write_stamp(&file).unwrap();
        write_state(&file, State::Hydrated).unwrap();
        file.sync_all().unwrap();
    });
    let link = connected(&socket_path).await;

    let report = recover(&link, &root).await.unwrap();

    assert_eq!(
        state_of(&path),
        Some(State::Hydrated),
        "recovery changed the state of a file whose fill committed while it waited ({report:?})"
    );
    assert!(
        blocks_of(&path) > 64,
        "recovery punched a file whose fill had committed `hydrated` after recovery read it \
         `hydrating` — the helper lets every opener of a `hydrated` file through, and may have \
         ignore-marked it ({report:?})"
    );
    assert_eq!(report.reset, 0, "{report:?}");
}

/// The lock: every fill and every free-up of this daemon
/// holds the per-inode lock for as long as it works on the file, and a
/// fill from the previous connection is still one of them. Recovery must
/// not touch a file whose lock is held — it is being filled or freed up
/// right now — and must not wait for it either, or a reconnect would wait
/// for a download of any length. The fill here has already
/// closed its descriptor, so only the lock stands between it and the
/// punch.
#[tokio::test]
async fn recovery_leaves_a_file_this_daemon_is_filling_to_the_fill() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), 0, || {});
    let link = connected(&socket_path).await;
    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let path = interrupted_file(&root.path, "x.bin", State::Hydrating, 1 << 20);
    let locks = InodeLocks::new();
    let fill = locks.lock(InodeKey::of(&open_rw(&path)).unwrap()).await;

    let report = tokio::time::timeout(
        Duration::from_secs(5),
        super::recover(&Clearance::Link(link.clone()), &root, &locks),
    )
    .await
    .expect("recovery waited for a fill of the same file")
    .unwrap();

    assert_eq!(
        report,
        RecoveryReport { scanned: 1, reset: 0, failed: 0, skipped: 0, busy: 1, deferred: 0 },
        "a file this daemon is filling is busy, and left to the fill"
    );
    assert_eq!(state_of(&path), Some(State::Hydrating));
    assert!(blocks_of(&path) > 64, "recovery punched a file a fill of this daemon held");

    drop(fill);
    let report = super::recover(&Clearance::Link(link), &root, &locks).await.unwrap();
    assert_eq!(report.reset, 1, "with the fill gone, the interrupted file is reset");
}

/// Point 2/3 of the task brief, and invariant M3: a file left
/// `dehydrating` by a crash between `write_state(Dehydrating)` and a
/// successful `ClearIgnore` may still carry its ignore mark. Recovery
/// must ask the helper to clear it — on the very descriptor it is about
/// to punch — exactly as `dehydrate` does, never skip straight to the
/// punch on the strength of the state xattr alone.
#[tokio::test]
async fn recovery_clears_the_ignore_mark_before_punching() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = fake_helper(socket_path.clone(), 0, || {});
    let link = connected(&socket_path).await;

    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let path = interrupted_file(&root.path, "a.bin", State::Hydrating, 8192);
    let expected_ino = std::fs::metadata(&path).unwrap().ino();

    recover(&link, &root).await.unwrap();

    let seen = asked_to(&helper, "ClearIgnore");
    assert_eq!(seen.ino, expected_ino, "the helper was handed a different file");
    assert_eq!(
        seen.state,
        Some(State::Hydrating),
        "the mark was cleared on a descriptor that is not the interrupted file"
    );
}

/// The worst outcome in this project, guarded against here exactly as
/// `dehydrate_leaves_the_file_untouched_when_clear_ignore_fails` guards
/// it there: a file whose `ClearIgnore` is refused must come out of
/// recovery completely untouched, not punched. Punching a file whose
/// ignore mark could not be confirmed cleared would leave it empty and
/// permanently un-intercepted, reading as zeros forever, with recovery
/// itself reporting success.
#[tokio::test]
async fn recovery_leaves_a_file_untouched_when_clear_ignore_fails() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), libc::EIO, || {});
    let link = connected(&socket_path).await;

    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let path = interrupted_file(&root.path, "a.bin", State::Dehydrating, 8192);

    let report = recover(&link, &root).await.unwrap();
    assert_eq!(
        report,
        RecoveryReport { scanned: 1, reset: 0, failed: 1, skipped: 0, busy: 0, deferred: 0 },
        "a file whose ignore mark could not be cleared must be counted as failed, not as \
         reset and not as nothing at all"
    );

    let after = File::open(&path).unwrap();
    assert_eq!(
        read_state(&after).unwrap(),
        Some(State::Dehydrating),
        "left exactly as found, so the next start retries it"
    );
    assert!(
        after.metadata().unwrap().blocks() > 0,
        "must not be punched when ClearIgnore failed"
    );
}

/// The recovery half of
/// `the_punch_is_made_durable_before_the_file_is_called_online_only`. A
/// punch that is only in page cache, published as `online-only`, is a
/// file the next boot can find with its blocks back and its state
/// insisting they are gone — and nothing will ever hydrate it, because
/// `online-only` is exactly the state that means "the content is
/// elsewhere". Recovery is the code that runs *after* that next boot, so
/// it is the last thing that should leave one behind.
#[test]
fn the_recovery_punch_is_made_durable_before_the_file_is_called_online_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let path = interrupted_file(&root.path, "f.bin", State::Dehydrating, 1 << 20);

    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), 0, || {});

    let asked = root.clone();
    let denied = &[libc::SYS_fsync, libc::SYS_fdatasync];
    let report = with_syscalls_denied(denied, move || async move {
        let link = connected(&socket_path).await;
        recover(&link, &asked).await.unwrap()
    });

    let Some(report) = report else {
        eprintln!("seccomp is unavailable here; skipping the durability check");
        return;
    };
    assert_eq!(report, RecoveryReport { scanned: 1, reset: 0, failed: 1, skipped: 0, busy: 0, deferred: 0 });
    assert!(blocks_of(&path) < 64, "the punch itself should still have happened");
    assert_eq!(
        state_of(&path),
        Some(State::Dehydrating),
        "a file whose punch could not be made durable must not be published as online-only"
    );
}

/// The other failure calls out: a `dehydrating` file whose
/// `ClearIgnore` succeeds and whose punch then fails. Nothing may be
/// published, nothing may be counted as reset, and the file waits for
/// the next start — `fallocate` failing is `ENOSPC` on a filesystem with
/// no room for the metadata a hole needs, or `EOPNOTSUPP` on one that
/// cannot punch at all.
#[test]
fn a_punch_that_fails_leaves_the_file_for_the_next_start() {
    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let path = interrupted_file(&root.path, "f.bin", State::Dehydrating, 1 << 20);

    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = fake_helper(socket_path.clone(), 0, || {});

    let asked = root.clone();
    let report = with_syscalls_denied(&[libc::SYS_fallocate], move || async move {
        let link = connected(&socket_path).await;
        recover(&link, &asked).await.unwrap()
    });

    let Some(report) = report else {
        eprintln!("seccomp is unavailable here; skipping the failed-punch check");
        return;
    };
    assert_eq!(report, RecoveryReport { scanned: 1, reset: 0, failed: 1, skipped: 0, busy: 0, deferred: 0 });
    assert!(blocks_of(&path) > 64, "the punch failed, so the blocks must still be there");
    assert_eq!(
        state_of(&path),
        Some(State::Dehydrating),
        "a file that was not emptied must not be called online-only"
    );
    assert!(
        read_stamp(&File::open(&path).unwrap()).unwrap().is_some(),
        "nothing may be published about a file the punch did not reach"
    );
    asked_to(&helper, "ClearIgnore");
}

// --- MAX_DEPTH, the socket disproof, and the xdev split --------------

/// `count` nested directories under `root`, returning the deepest one.
/// `root` itself is nesting level 0, so the returned directory is at
/// level `count` — the same convention [`MAX_DEPTH`] and `Frame::depth`
/// use.
fn nested_dirs(root: &Path, count: usize) -> PathBuf {
    let mut path = root.to_path_buf();
    for i in 0..count {
        path = path.join(format!("d{i}"));
        std::fs::create_dir(&path).unwrap();
    }
    path
}

/// The helper refuses to *mark* anything past `MAX_DEPTH` levels
/// (`crates/konedrive-helper/src/marks.rs`), so a directory below it is
/// unmarked and uninterceptable no matter what recovery finds there —
/// the two halves have to agree on the same number, which is why both
/// import the one `konedrive_fs::MAX_DEPTH`. A file at level 127
/// (inside the deepest directory the helper would still have marked) is
/// recovered normally; a directory at level 128 is never even listed,
/// counted in `skipped` instead, and whatever is inside it — a second
/// interrupted file — is never seen at all, not even as a separate
/// `skipped` entry, because the whole directory was refused as one.
#[tokio::test]
async fn recovery_stops_at_the_same_depth_the_helper_stops_marking() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), 0, || {});
    let link = connected(&socket_path).await;

    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());

    let level_127 = nested_dirs(&root.path, MAX_DEPTH - 1);
    interrupted_file(&level_127, "shallow.bin", State::Dehydrating, 4096);

    let level_128 = level_127.join("too-deep");
    std::fs::create_dir(&level_128).unwrap();
    interrupted_file(&level_128, "deep.bin", State::Dehydrating, 4096);

    let report = recover(&link, &root).await.unwrap();

    assert_eq!(
        report,
        RecoveryReport { scanned: 1, reset: 1, failed: 0, skipped: 1, busy: 0, deferred: 0 },
        "level 127 must be recovered normally; level 128 must be refused as one directory, \
         not silently skipped and not walked into"
    );
    assert!(
        blocks_of(&level_128.join("deep.bin")) > 0,
        "a file deeper than the helper would ever mark must not be punched"
    );
}

/// The previous round called deleting this function's `d_type` guard —
/// the `else { return Ok(Entry::Elsewhere) }` arm below, taken whenever
/// `kind` is neither a directory nor a regular file — an *equivalent*
/// mutant: `O_NOFOLLOW` stops a symlink and the post-open `fstat` stops
/// a FIFO, independently of the guard, which is true as far as it goes.
/// It is not equivalent, though. `open(2)` on a **Unix domain socket**
/// returns `ENXIO` — not `ELOOP`, not a successful open of something the
/// `fstat` then rejects — so with the guard gone that errno reaches
/// [`recover`]'s `Err(e) => report.skipped += 1` arm instead of the
/// silent `Entry::Elsewhere` every other non-file, non-directory entry
/// gets. `RecoveryReport::skipped` is documented as "a non-zero value
/// means the root was **not** fully recovered", so that is an observable
/// change in a `pub` field, not an equivalent mutant.
///
/// A dangling symlink and a FIFO sit beside the socket because they are
/// the two kinds the earlier round's reasoning actually covers — the
/// point of running them together is that only the socket's count moves.
/// Measured against this test by hand: unmutated,
/// `RecoveryReport { scanned: 1, reset: 1, failed: 0, skipped: 0, busy: 0, deferred: 0 }`; with
/// the guard's `else` arm deleted (folding every non-directory kind into
/// the same open the regular-file branch uses), `skipped: 1`.
#[tokio::test]
async fn a_unix_socket_is_silently_elsewhere_not_counted_as_skipped() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), 0, || {});
    let link = connected(&socket_path).await;

    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    interrupted_file(&root.path, "f.bin", State::Dehydrating, 4096);

    // The control: both already established as unaffected either way.
    std::os::unix::fs::symlink(root.path.join("nowhere"), root.path.join("dangling"))
        .unwrap();
    nix::unistd::mkfifo(&root.path.join("pipe"), Mode::S_IRUSR | Mode::S_IWUSR).unwrap();

    // A real AF_UNIX socket special file: bound, never connected, whose
    // descriptor is dropped immediately — the directory entry it leaves
    // behind persists exactly like a closed regular file's would.
    let sock_fd =
        socket(AddressFamily::Unix, SockType::Stream, SockFlag::SOCK_CLOEXEC, None).unwrap();
    bind(sock_fd.as_raw_fd(), &UnixAddr::new(&root.path.join("sock")).unwrap()).unwrap();
    drop(sock_fd);

    let report = recover(&link, &root).await.unwrap();

    assert_eq!(
        report,
        RecoveryReport { scanned: 1, reset: 1, failed: 0, skipped: 0, busy: 0, deferred: 0 },
        "a socket must be silently Elsewhere, exactly like the symlink and the FIFO beside \
         it — not counted in skipped"
    );
}

/// `st_dev` check does its job — [`open_entry`] never opens
/// across it — but until [`Entry::OtherFilesystem`] existed, the whole
/// excluded subtree vanished into the same silent `Entry::Elsewhere` a
/// symlink gets: `skipped == 0` while a real subtree went unrecovered,
/// contradicting `RecoveryReport::skipped`'s own doc comment ("a
/// non-zero value means the root was **not** fully recovered").
///
/// Reproducing a genuine cross-device boundary unprivileged needs a real
/// mount, which needs a mount namespace this test process does not have.
/// Acquiring one from inside an already multi-threaded `cargo test`
/// binary is refused by the kernel outright (`unshare(CLONE_NEWUSER)`
/// requires a single-threaded caller), so this test re-executes its own
/// binary as a fresh, single-threaded process under `unshare -Urm`
/// instead. `KONEDRIVE_XDEV_CHILD` is what tells that re-exec apart from
/// the original run: only the child mounts a tmpfs, places one
/// interrupted file under it, runs [`recover`], and turns its own
/// assertions into the process's exit status for the parent half to
/// check.
#[test]
fn a_subtree_on_another_filesystem_is_counted_and_logged() {
    if std::env::var_os("KONEDRIVE_XDEV_CHILD").is_some() {
        xdev_child();
        return;
    }

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("cannot find this test binary ({e}); skipping the xdev check");
            return;
        }
    };
    let invocation = std::process::Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "--"])
        .arg(&exe)
        .args([
            "hydration::recovery::tests::a_subtree_on_another_filesystem_is_counted_and_logged",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("KONEDRIVE_XDEV_CHILD", "1")
        .output();
    let output = match invocation {
        Ok(output) => output,
        Err(e) => {
            eprintln!("`unshare` is unavailable here ({e}); skipping the xdev check");
            return;
        }
    };
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success()
        && (stderr.contains("unshare failed")
            || stderr.contains("Operation not permitted")
            || stderr.contains("Permission denied"))
    {
        eprintln!(
            "unprivileged user namespaces are unavailable here; skipping the xdev check: \
             {stderr}"
        );
        return;
    }
    assert!(
        output.status.success(),
        "the cross-device recovery check failed:\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout),
    );
}

/// The half of
/// [`a_subtree_on_another_filesystem_is_counted_and_logged`] that
/// actually runs under `unshare -Urm`: a real tmpfs mounted a level
/// inside the sync root, and one interrupted file under that mount.
/// Panicking here fails the re-exec'd process, which the parent half
/// reports.
fn xdev_child() {
    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let mount_point = root.path.join("mnt");
    std::fs::create_dir(&mount_point).unwrap();
    nix::mount::mount(
        Some("tmpfs"),
        &mount_point,
        Some("tmpfs"),
        nix::mount::MsFlags::empty(),
        None::<&str>,
    )
    .expect("mounting a tmpfs inside the sync root under unshare -Urm");

    let victim = interrupted_file(&mount_point, "victim.bin", State::Dehydrating, 8192);

    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");

    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let _helper = fake_helper(socket_path.clone(), 0, || {});
        let link = connected(&socket_path).await;

        let report = recover(&link, &root).await.unwrap();
        assert_eq!(
            report,
            RecoveryReport { scanned: 0, reset: 0, failed: 0, skipped: 1, busy: 0, deferred: 0 },
            "a subtree on another filesystem must be counted in skipped, not silently \
             passed over: {report:?}"
        );
        assert!(blocks_of(&victim) > 0, "a file on another filesystem must not be punched");
        assert_eq!(
            state_of(&victim),
            Some(State::Dehydrating),
            "a file on another filesystem must be left exactly as found"
        );
    });
}
