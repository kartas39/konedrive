use std::fs::File;

use super::*;

/// The kernel breaks a lease by signalling its holder, and
/// the signal it uses (`SIGIO`) terminates the process by default. This
/// is the whole failure, end to end: take a lease, have another process
/// open the file — which is what every lease this crate takes exists to
/// notice — and stay alive long enough to release it.
///
/// Without [`silence_sigio`] this does not fail, it *kills the test
/// binary*: `error: test failed ... signal: 29, SIGIO`, taking every
/// other test in the process with it. That is exactly what would happen
/// to the daemon, which holds this lease across the punch and the fsync
/// of a whole file (steps 3–5).
#[test]
fn a_lease_break_does_not_kill_the_process() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin");
    std::fs::write(&path, vec![7u8; 4096]).unwrap();
    let file = File::options().read(true).write(true).open(&path).unwrap();
    let lease = WriteLease::take(&file).unwrap().expect("expected to get the lease");

    // Somebody else opening the file is what breaks the lease. The
    // kernel signals the holder — this process — and suspends the opener
    // until the lease is released (or `/proc/sys/fs/lease-break-time`
    // expires), so it is released once the signal has had its chance.
    //
    // The opener is a thread rather than a child process because an
    // earlier version of this test spawned `cat` and made the other two
    // lease tests in this crate fail intermittently — about one workspace
    // run in twenty. **Why** was never established. This comment used to
    // claim that `fork`/`posix_spawn` duplicates the descriptor table and
    // that every duplicate raises a `struct file` reference count
    // `F_SETLEASE` refuses on; that is false, and measuring it says so:
    // 0 failures in 2000 `posix_spawn`s, 0 in 3000 `fork`+`_exit`s and 0
    // in 3000 `fork`+`exec`s, against a control where a genuine second
    // `open()` gives `EAGAIN` every time. `check_conflicting_open()`
    // compares `inode->i_readcount`/`i_writecount`, which only
    // `do_dentry_open` raises — one per `struct file`, not per descriptor
    // — while `fork` duplicates the fd *table* and raises `f_count`,
    // which the lease check never reads. The flakiness was real; its
    // cause is still unknown, so the threads stay.
    //
    // A thread shares the descriptor table, and leases have no
    // same-process exemption: this open breaks the lease like any other.
    let opened = path.clone();
    let opener = std::thread::spawn(move || File::open(&opened).unwrap());
    std::thread::sleep(std::time::Duration::from_millis(500));
    drop(lease);

    opener.join().expect("the opener should have proceeded once the lease was released");
    // Reaching this line at all is the assertion: the lease break did not
    // terminate us.
}

/// The same guarantee stated directly, so a regression is a readable
/// failure rather than a killed test binary: once anything in this
/// process has taken a lease, `SIGIO` must no longer carry its default
/// (terminate) disposition.
#[test]
fn taking_a_lease_leaves_sigio_unable_to_terminate_the_process() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin");
    std::fs::write(&path, b"data").unwrap();
    let file = File::options().read(true).write(true).open(&path).unwrap();
    let _lease = WriteLease::take(&file).unwrap().expect("expected to get the lease");

    // SAFETY: querying the current disposition with a null `act` never
    // changes it; `current` is a live, correctly sized `sigaction`.
    let current = unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        assert_eq!(libc::sigaction(libc::SIGIO, std::ptr::null(), &mut current), 0);
        current
    };
    assert_ne!(
        current.sa_sigaction,
        libc::SIG_DFL,
        "SIGIO still has its default disposition (terminate); a lease break would kill us"
    );
}

#[test]
fn taken_when_nobody_else_has_the_file_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin");
    std::fs::write(&path, b"data").unwrap();
    let file = File::options().read(true).write(true).open(&path).unwrap();
    let lease = WriteLease::take(&file).unwrap();
    assert!(lease.is_some(), "expected to get the lease");
}

#[test]
fn refused_while_another_descriptor_is_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin");
    std::fs::write(&path, b"data").unwrap();
    let file = File::options().read(true).write(true).open(&path).unwrap();
    let _other = File::open(&path).unwrap();
    assert!(WriteLease::take(&file).unwrap().is_none(), "expected refusal");
}

/// The write design assumed (§15) that a read lease is refused while
/// anyone has the file open for writing, from the kernel source rather
/// than the man page. Measured here: a writer refuses it, a second reader
/// does not, and the refusal ends when the writer closes.
#[test]
fn a_read_lease_is_refused_only_while_someone_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin");
    std::fs::write(&path, b"data").unwrap();
    let probe = File::open(&path).unwrap();
    assert!(!open_for_writing(&probe).unwrap(), "nobody writes");
    let reader = File::open(&path).unwrap();
    assert!(!open_for_writing(&probe).unwrap(), "a second reader is no writer");
    let writer = File::options().append(true).open(&path).unwrap();
    assert!(open_for_writing(&probe).unwrap(), "a writer refuses the lease");
    drop(writer);
    assert!(!open_for_writing(&probe).unwrap(), "the refusal ends with the writer");
    drop(reader);
    // The probe released its lease: an open for writing does not wait
    // for a lease break.
    let started = std::time::Instant::now();
    drop(File::options().write(true).open(&path).unwrap());
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
}

// `EACCES` (file owned by another uid) only comes out of a real
// `F_SETLEASE` call against a file this process does not own, which
// needs a second, differently-uid'd file — i.e. root — to set up. That
// is not available here (no sudo), so the errno-to-outcome mapping
// itself is tested directly instead, with the same real errno values
// the kernel would report.

#[test]
fn eagain_means_retry_later() {
    interpret_setlease_failure(io::Error::from_raw_os_error(libc::EAGAIN))
        .expect("EAGAIN must not be a hard error");
}

#[test]
fn eacces_is_reported_as_owned_by_another_user_not_as_a_retryable_refusal() {
    let error = interpret_setlease_failure(io::Error::from_raw_os_error(libc::EACCES))
        .expect_err("EACCES must be a hard error, not Ok(None)");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error:?}");
    assert!(
        error.to_string().contains("another user"),
        "error text should say the file is owned by another user: {error}"
    );
}
