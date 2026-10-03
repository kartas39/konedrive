use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use konedrive_fs::placeholder::{read_stamp, read_state, write_stamp, State, write_state};
use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use nix::sys::socket::{accept, bind, listen as sock_listen, socket, AddressFamily, Backlog, SockFlag, SockType, UnixAddr};
use xattr::FileExt;

use crate::helper::HelperLink;
use crate::folder::root::tests::test_root;
use crate::folder::root::{SyncRoot, uuid_v4};
use super::*;

/// A hydrated file of `size` bytes with a matching stamp, exactly as a
/// finished hydration leaves one.
fn hydrated_file(root: &Path, name: &str, size: usize) -> PathBuf {
    let path = root.join(name);
    std::fs::write(&path, vec![1u8; size]).unwrap();
    let file = File::options().read(true).write(true).open(&path).unwrap();
    write_state(&file, State::Hydrated).unwrap();
    write_stamp(&file).unwrap();
    path
}

pub(crate) fn open_rw(path: &Path) -> File {
    File::options().read(true).write(true).open(path).unwrap()
}

pub(crate) fn blocks_of(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().blocks()
}

// --- The dehydration guard -------------------------------------------

#[tokio::test]
async fn dehydration_refuses_a_locally_modified_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = hydrated_file(dir.path(), "f.bin", 4096);

    let mut appended = File::options().append(true).open(&path).unwrap();
    appended.write_all(b"changed").unwrap();
    drop(appended);

    let error = check_dehydratable(&open_rw(&path)).unwrap_err();
    assert!(matches!(error, DehydrateError::ModifiedLocally), "{error:?}");
}

/// The state gate, arm by arm. Only `hydrated` may be emptied: a file
/// that is `online-only` has nothing to free, and one that is `hydrating`
/// or `dehydrating` is in the middle of something — punching any of them
/// on the strength of a stale stamp is how a download in flight becomes
/// zeros.
#[tokio::test]
async fn dehydration_refuses_every_state_but_hydrated() {
    let dir = tempfile::tempdir().unwrap();
    for state in [State::OnlineOnly, State::Hydrating, State::Dehydrating] {
        let path = hydrated_file(dir.path(), &format!("{}.bin", state.as_str()), 4096);
        let file = open_rw(&path);
        write_state(&file, state).unwrap();

        let error = check_dehydratable(&file).unwrap_err();
        assert!(matches!(error, DehydrateError::NotHydrated), "{state:?}: {error:?}");
    }
}

/// A file with no `user.konedrive.state` at all is not ours. Nothing
/// about it — not its name, not where it sits — makes it something this
/// daemon may empty.
#[tokio::test]
async fn dehydration_refuses_a_file_that_is_not_managed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mine.txt");
    std::fs::write(&path, vec![9u8; 4096]).unwrap();

    let error = check_dehydratable(&open_rw(&path)).unwrap_err();
    assert!(matches!(error, DehydrateError::NotManaged), "{error:?}");
}

#[tokio::test]
async fn dehydration_empties_a_clean_file_and_keeps_its_size() {
    let dir = tempfile::tempdir().unwrap();
    let path = hydrated_file(dir.path(), "f.bin", 1 << 20);

    let file = open_rw(&path);
    let restore = mark_dehydrating(&file).unwrap();
    punch_clean_file(&file, restore).unwrap();
    drop(file);

    let meta = std::fs::metadata(&path).unwrap();
    assert_eq!(meta.len(), 1 << 20);
    assert!(meta.blocks() < 64, "{} blocks left", meta.blocks());
    let file = File::open(&path).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
    assert_eq!(
        read_stamp(&file).unwrap(),
        None,
        "the stamp described a hydrated file and must not outlive it"
    );
}

/// An `online-only` file's mtime is the remote
/// `lastModifiedDateTime`. `fallocate` bumps it to now, so dehydration
/// has to put it back — otherwise "free up space" silently makes every
/// file look modified today, which is precisely the signal the Sync
/// engine's change detection will key on.
#[tokio::test]
async fn dehydration_keeps_the_remote_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin");
    std::fs::write(&path, vec![1u8; 1 << 20]).unwrap();
    let remote = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let file = open_rw(&path);
    file.set_times(std::fs::FileTimes::new().set_modified(remote)).unwrap();
    write_state(&file, State::Hydrated).unwrap();
    write_stamp(&file).unwrap();

    let restore = mark_dehydrating(&file).unwrap();
    punch_clean_file(&file, restore).unwrap();

    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        remote,
        "dehydration must not move the file's mtime to now"
    );
}

/// The lease is the proof that nobody else has the file open, so a
/// refusal has to stop everything — and put the state back, or the file
/// is left claiming to be mid-dehydration when nothing is happening to
/// it at all.
#[tokio::test]
async fn dehydration_refuses_while_another_process_holds_the_file_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = hydrated_file(dir.path(), "f.bin", 4096);

    let file = open_rw(&path);
    let restore = mark_dehydrating(&file).unwrap();
    let _held_open = File::open(&path).unwrap();

    let error = punch_clean_file(&file, restore).unwrap_err();
    assert!(matches!(error, DehydrateError::InUse), "{error:?}");
    assert_eq!(
        read_state(&file).unwrap(),
        Some(State::Hydrated),
        "a refused lease must roll the state back, not leave the file dehydrating"
    );
    assert!(blocks_of(&path) > 0, "nothing may be punched without the lease");
}

/// The lease has to still be held at the moment the blocks go
/// away *and* until the file has been published `online-only` — an open
/// that slips in after an early release is not suspended, and reads a
/// file being emptied under it.
///
/// The version this replaces took `fcntl(F_GETLEASE)` from the hook and
/// asserted it said `F_WRLCK`. That measures "a lease existed when the
/// hook ran", which is not what its name claimed: releasing the lease
/// *between the hook and the punch*, or *between the punch and the state
/// flip*, both survived it. Measured — and the shipped code was correct,
/// so the test was the thing that was wrong.
///
/// So the lease is measured by what it does, at both of its ends: a
/// thread started at `UnderLease` opens the file, and it must **still be
/// suspended** at `BeforeRelease`, with the punch and the state flip both
/// behind it. Each end waits long enough (200 ms) that an early release
/// would have let the opener through many times over, so neither
/// assertion is a race in either direction — a correctly held lease
/// cannot let it through at all, and a released one needs microseconds.
/// What the opener finally sees when it does get through is checked too.
/// `konedrive-fs`'s `a_lease_break_does_not_kill_the_process` has the
/// same shape.
#[test]
fn an_open_arriving_during_the_punch_waits_for_all_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = hydrated_file(dir.path(), "f.bin", 1 << 20);

    let file = open_rw(&path);
    let restore = mark_dehydrating(&file).unwrap();

    // The opener's own two signals: it is about to call `open()`, and it
    // has come back from it.
    let entering = Arc::new(AtomicBool::new(false));
    let through = Arc::new(AtomicBool::new(false));
    let (openers, opener) = std::sync::mpsc::channel();

    let hook = {
        let path = path.clone();
        let (entering, through) = (Arc::clone(&entering), Arc::clone(&through));
        let mut path = Some(path);
        move |watch: Watch, _: &File| {
            if let Some(path) = path.take() {
                let (signals, watched) = (Arc::clone(&entering), Arc::clone(&through));
                openers
                    .send(std::thread::spawn(move || {
                        signals.store(true, Ordering::SeqCst);
                        let opened = File::open(&path).unwrap();
                        watched.store(true, Ordering::SeqCst);
                        // What the application would see, sampled the
                        // instant its `open()` succeeded.
                        let meta = opened.metadata().unwrap();
                        (read_state(&opened).unwrap(), meta.blocks())
                    }))
                    .unwrap();
                while !entering.load(Ordering::SeqCst) {
                    std::thread::yield_now();
                }
            }
            std::thread::sleep(Duration::from_millis(200));
            assert!(
                !through.load(Ordering::SeqCst),
                "{watch:?}: the open went through while the file was being emptied — the \
                 lease was not held across all of the punch and the publish, so an \
                 application read a file mid-dehydration"
            );
        }
    };
    punch_clean_file_watched(&file, restore, hook).unwrap();

    let (state, blocks) = opener.recv().unwrap().join().unwrap();
    assert_eq!(
        state,
        Some(State::OnlineOnly),
        "the open completed while the file was still dehydrating: the lease did not cover \
         the whole sequence"
    );
    assert!(blocks < 64, "the open completed while the file still had its blocks");
}

/// Makes `fsync`/`fdatasync` fail with `EIO` for this process, for good,
/// so that a missing durability barrier becomes an observable difference
/// rather than an invisible one.
///
/// A seccomp filter is the only way to do that unprivileged: nothing in
/// user space can make tmpfs refuse an `fsync`, and the crash injection
/// that would show the barrier's real purpose needs the VM suite (spec
/// §11.2). The filter applies to the calling thread only (no `TSYNC`)
/// and cannot be lifted, so the one test that uses it does so on a
/// thread it is willing to lose.
///
/// `Err` means seccomp is not available here (an old kernel, a sandbox
/// that blocks it); the caller then skips rather than fails.
fn deny_fsync() -> Result<(), ()> {
    deny_syscalls(&[libc::SYS_fsync, libc::SYS_fdatasync])
}

/// The general form: make each of `numbers` fail with `EIO` for this
/// thread, for good. Used for `fsync`/`fdatasync` (a missing durability
/// barrier) and for `fallocate` (a punch that fails), neither of which
/// an unprivileged test can provoke any other way on tmpfs.
fn deny_syscalls(numbers: &[libc::c_long]) -> Result<(), ()> {
    // `no_new_privs` is a per-thread, inherited flag; it is what lets an
    // unprivileged thread install a filter at all.
    const LOAD_SYSCALL_NR: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
    const JUMP_IF_EQUAL: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
    const RETURN: u16 = 0x06; // BPF_RET | BPF_K
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
    const SECCOMP_MODE_FILTER: i32 = 2;

    // nr; one `jeq <number> -> deny` per number; allow; deny. Each jump
    // is taken to the last instruction, so `jt` counts the instructions
    // between it and the end.
    let mut program = vec![libc::sock_filter { code: LOAD_SYSCALL_NR, jt: 0, jf: 0, k: 0 }];
    for (i, number) in numbers.iter().enumerate() {
        program.push(libc::sock_filter {
            code: JUMP_IF_EQUAL,
            jt: (numbers.len() - i) as u8,
            jf: 0,
            k: *number as u32,
        });
    }
    program.push(libc::sock_filter { code: RETURN, jt: 0, jf: 0, k: SECCOMP_RET_ALLOW });
    program.push(libc::sock_filter {
        code: RETURN,
        jt: 0,
        jf: 0,
        k: SECCOMP_RET_ERRNO | (libc::EIO as u32 & 0xffff),
    });
    let filter = libc::sock_fprog {
        len: program.len() as u16,
        filter: program.as_mut_ptr(),
    };
    // SAFETY: both prctls take a live, correctly sized `sock_fprog`
    // whose filter array outlives the call.
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(());
        }
        if libc::prctl(libc::PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &filter) != 0 {
            return Err(());
        }
    }
    Ok(())
}

/// Runs an async body with `denied` failing on **every** thread it can
/// reach: the thread the future runs on, and every blocking thread
/// `spawn_blocking` hands work to (`on_thread_start` covers the blocking
/// pool as well as the runtime's own threads — `tokio`'s
/// `blocking::pool::Inner::run` calls `after_start`). Without that, a
/// filter installed on the test's thread would not reach the phase under
/// test, which is always on a blocking thread.
///
/// The whole runtime lives on one thread of its own, which is then
/// dropped, because a seccomp filter cannot be lifted. Anything the test
/// needs *unfiltered* — the fake helper — must be started before this is
/// called, since threads created from inside inherit the filter.
///
/// `None` means seccomp is unavailable here (an old kernel, a sandbox
/// that blocks it); the caller then skips rather than fails.
pub(crate) fn with_syscalls_denied<T, F>(
    denied: &'static [libc::c_long],
    body: impl FnOnce() -> F + Send + 'static,
) -> Option<T>
where
    T: Send + 'static,
    F: std::future::Future<Output = T>,
{
    std::thread::spawn(move || {
        deny_syscalls(denied).ok()?;
        let unfiltered = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&unfiltered);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .on_thread_start(move || {
                if deny_syscalls(denied).is_err() {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            })
            .build()
            .unwrap();
        let outcome = runtime.block_on(body());
        (unfiltered.load(Ordering::SeqCst) == 0).then_some(outcome)
    })
    .join()
    .expect("the thread running the filtered work must not panic")
}

/// Dehydration's step 4: the punch is followed by an `fsync`, and the file is
/// not called `online-only` until that has succeeded. A punch that is
/// only in page cache, published as `online-only`, is a file the next
/// boot can find with its blocks back and its state insisting they are
/// gone — and nothing will hydrate it, because `online-only` is exactly
/// the state that means "the content is elsewhere".
///
/// A barrier that works is invisible, so it is measured by taking it
/// away. The file is marked `dehydrating` first, while `fsync` still
/// works, so the only barrier the filter can remove is the one that
/// follows the punch — the thing under test.
///
/// The work runs on a thread of its own because a seccomp filter cannot
/// be lifted: letting it die with the thread keeps it away from every
/// other test, including under `--test-threads=1`, where libtest runs
/// the test bodies themselves on the main thread. A thread, not a child
/// process: an earlier version of this test forked, and the other
/// lease-taking tests in this binary then failed roughly one run in five.
/// **Why** was never established. This comment used to say that spawning
/// duplicates the descriptor table and that a duplicated descriptor is
/// what `F_SETLEASE` refuses on; that is false — measured on this kernel,
/// 0 failures in 2000 `posix_spawn`s and 0 in 3000 `fork`s either side of
/// an `exec`, against a control where a real second `open()` gives
/// `EAGAIN` immediately, because the check reads
/// `inode->i_readcount`/`i_writecount`, which only a genuine open raises.
/// The flakiness was real, its cause is unknown, and the thread stays.
#[test]
fn the_punch_is_made_durable_before_the_file_is_called_online_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = hydrated_file(dir.path(), "f.bin", 1 << 20);
    let file = open_rw(&path);
    let restore = mark_dehydrating(&file).unwrap();

    let punched = std::thread::spawn(move || match deny_fsync() {
        Err(()) => None,
        Ok(()) => Some(punch_clean_file(&file, restore).is_ok()),
    })
    .join()
    .expect("the thread running the punch must not panic");

    match punched {
        None => eprintln!("seccomp is unavailable here; skipping the durability check"),
        Some(true) => panic!(
            "the dehydration reported success although every fsync failed: the punch is \
             never made durable"
        ),
        Some(false) => {
            assert!(blocks_of(&path) < 64, "the punch itself should still have happened");
            assert_eq!(
                read_state(&File::open(&path).unwrap()).unwrap(),
                Some(State::Dehydrating),
                "a file whose punch could not be made durable must not be published as \
                 online-only"
            );
        }
    }
}

/// Step 2: `state=dehydrating` is made durable
/// **before** the helper is asked to stop intercepting the file. Without
/// that barrier a crash in the window can leave the punch durable while
/// the state is not — an empty file that reads `hydrated` with its ignore
/// mark cleared, which the helper then allows and re-marks: zeros on
/// every later open, which is the one outcome this sub-project exists to
/// prevent.
///
/// `the_punch_is_made_durable_before_the_file_is_called_online_only`
/// cannot reach this one: it installs its filter *after* marking, so the
/// only barrier it can take away is the one after the punch — and
/// deleting this `fsync` broke nothing. Here the filter is in place
/// before `mark_dehydrating` runs, and the assertion is that the helper
/// is never asked anything at all.
#[test]
fn the_dehydrating_state_is_durable_before_the_helper_is_asked_to_stop_intercepting() {
    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let path = hydrated_file(&root.path, "f.bin", 1 << 20);

    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    // Started before the filter exists, on a thread that never inherits
    // it: the helper must be able to answer, so that "it was never asked"
    // is a real observation rather than an artefact.
    let helper = fake_helper(socket_path.clone(), 0, || {});

    let asked = root.clone();
    let target = path.clone();
    let denied = &[libc::SYS_fsync, libc::SYS_fdatasync];
    let outcome = with_syscalls_denied(denied, move || async move {
        let link = connected(&socket_path).await;
        dehydrate(&link, &asked, &target).await
    });

    let Some(outcome) = outcome else {
        eprintln!("seccomp is unavailable here; skipping the durability check");
        return;
    };
    assert!(
        outcome.is_err(),
        "a dehydration whose state could not be made durable reported success"
    );
    assert!(
        helper.recv_timeout(Duration::from_millis(200)).is_err(),
        "the helper was asked to clear the ignore mark although `dehydrating` was never made \
         durable: a crash in that window leaves an empty file reading `hydrated`"
    );
    // And the failure of the barrier itself rolls back, so the
    // file is not left announcing a dehydration that never started.
    assert_eq!(
        read_state(&File::open(&path).unwrap()).unwrap(),
        Some(State::Hydrated),
        "a file whose `dehydrating` could not be made durable must not be left claiming it: \
         startup recovery would empty a fully hydrated file"
    );
    assert!(blocks_of(&path) > 64, "nothing may be punched");
}

// --- The helper round trip -------------------------------------------

/// What a fake helper saw: the message, and what the descriptor it came
/// with looked like at that moment.
#[derive(Debug)]
pub(crate) struct Seen {
    pub(crate) message: String,
    pub(crate) state: Option<State>,
    pub(crate) ino: u64,
}

/// A stand-in helper: greets, acks the `Hello` handshake, then answers
/// `ClearIgnore` with `clear_ignore_errno` and everything else with 0.
///
/// `on_clear_ignore` runs on the helper's thread while the daemon is
/// still awaiting the ack — the one moment in `dehydrate` when the file
/// is marked, the helper has the descriptor, and nothing has been punched
/// yet. That makes it the injection point for everything that used to be
/// a race.
///
/// The listener is bound on the caller's thread before this returns, so
/// the test's own `connect` cannot race `bind`.
pub(crate) fn fake_helper(
    path: PathBuf,
    clear_ignore_errno: i32,
    on_clear_ignore: impl FnOnce() + Send + 'static,
) -> std::sync::mpsc::Receiver<Seen> {
    let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
        .unwrap();
    let addr = UnixAddr::new(&path).unwrap();
    bind(fd.as_raw_fd(), &addr).unwrap();
    sock_listen(&fd, Backlog::new(16).unwrap()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let listener = fd;
        let accepted = accept(listener.as_raw_fd()).unwrap();
        // SAFETY: `accept` just returned a freshly opened descriptor that
        // this process now solely owns.
        let stream = unsafe { UnixStream::from_raw_fd(accepted) };
        let mut channel = Channel::new(stream).unwrap();
        channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
        let (hello, _) = channel.recv::<ToHelper>().unwrap();
        assert!(
            matches!(hello, ToHelper::Hello { version } if version == PROTOCOL_VERSION),
            "{hello:?}"
        );
        channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();

        let mut hook = Some(on_clear_ignore);
        while let Ok((message, fd)) = channel.recv::<ToHelper>() {
            let clearing = matches!(message, ToHelper::ClearIgnore);
            let (state, ino) = match fd {
                Some(fd) => {
                    let file = File::from(fd);
                    (
                        read_state(&file).ok().flatten(),
                        file.metadata().map(|m| m.ino()).unwrap_or(0),
                    )
                }
                None => (None, 0),
            };
            let _ = tx.send(Seen { message: format!("{message:?}"), state, ino });
            if clearing {
                if let Some(hook) = hook.take() {
                    hook();
                }
            }
            let errno = if clearing { clear_ignore_errno } else { 0 };
            if channel.send(&ToDaemon::Ack { errno }, None).is_err() {
                break;
            }
        }
    });
    rx
}

/// The same, but every request after the handshake is refused. Used to
/// fail a registration the way a real helper would.
pub(crate) fn fake_helper_refusing_everything(path: PathBuf) -> std::sync::mpsc::Receiver<Seen> {
    fake_helper_with_errno(path, libc::EPERM)
}

fn fake_helper_with_errno(path: PathBuf, errno: i32) -> std::sync::mpsc::Receiver<Seen> {
    let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
        .unwrap();
    let addr = UnixAddr::new(&path).unwrap();
    bind(fd.as_raw_fd(), &addr).unwrap();
    sock_listen(&fd, Backlog::new(16).unwrap()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let listener = fd;
        let accepted = accept(listener.as_raw_fd()).unwrap();
        // SAFETY: as above — a descriptor `accept` just handed us.
        let stream = unsafe { UnixStream::from_raw_fd(accepted) };
        let mut channel = Channel::new(stream).unwrap();
        channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
        let (_hello, _) = channel.recv::<ToHelper>().unwrap();
        channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
        while let Ok((message, _fd)) = channel.recv::<ToHelper>() {
            let _ = tx.send(Seen { message: format!("{message:?}"), state: None, ino: 0 });
            if channel.send(&ToDaemon::Ack { errno }, None).is_err() {
                break;
            }
        }
    });
    rx
}

pub(crate) async fn connected(socket_path: &Path) -> HelperLink {
    HelperLink::connect(socket_path).await.unwrap().0
}

/// The first request of a given kind the fake helper saw, or a failure
/// if it never arrived. Bounded, so a mutation that stops calling the
/// helper at all fails the test instead of hanging it.
pub(crate) fn asked_to(seen: &std::sync::mpsc::Receiver<Seen>, what: &str) -> Seen {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while let Ok(request) =
        seen.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
    {
        if request.message.starts_with(what) {
            return request;
        }
    }
    panic!("the helper was never asked to {what}");
}

/// Invariant M3, and the one failure in this module that is both silent
/// and unrecoverable: if the ignore mark was not cleared, the file must
/// come out of this untouched. A `let _ = link.clear_ignore(...)` here
/// punches anyway and leaves a file that is empty *and* invisible to the
/// helper — zeros on every later open, forever.
#[tokio::test]
async fn dehydrate_leaves_the_file_untouched_when_clear_ignore_fails() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), libc::EIO, || {});
    let link = connected(&socket_path).await;

    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let path = hydrated_file(&root.path, "f.bin", 1 << 20);

    let error = dehydrate(&link, &root, &path).await.unwrap_err();
    assert!(matches!(error, DehydrateError::Io(_)), "{error:?}");

    let meta = std::fs::metadata(&path).unwrap();
    assert_eq!(meta.len(), 1 << 20, "size must be untouched");
    assert!(meta.blocks() > 64, "the file must NOT be punched when ClearIgnore failed");
    let file = File::open(&path).unwrap();
    assert_eq!(
        read_state(&file).unwrap(),
        Some(State::Hydrated),
        "a failed ClearIgnore must roll the state back, not leave the file dehydrating"
    );
}

/// / steps 1–2, in that order. By the time the helper
/// is asked to stop intercepting this file, the file must already say
/// `dehydrating` — otherwise an open landing in the gap is answered with
/// a *fresh* ignore mark and an allow, and the punch that follows leaves
/// the file empty and permanently un-intercepted, with no error anywhere.
/// The state is read off the very descriptor the helper was handed.
#[tokio::test]
async fn dehydrate_marks_the_file_dehydrating_before_it_asks_the_helper() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = fake_helper(socket_path.clone(), 0, || {});
    let link = connected(&socket_path).await;

    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let path = hydrated_file(&root.path, "f.bin", 1 << 20);
    let expected_ino = std::fs::metadata(&path).unwrap().ino();

    dehydrate(&link, &root, &path).await.unwrap();

    let seen = asked_to(&helper, "ClearIgnore");
    assert_eq!(
        seen.state,
        Some(State::Dehydrating),
        "the helper saw state={:?}: the mark is being cleared while the file still reads \
         hydrated, so an open in that window gets a fresh ignore mark and an allow",
        seen.state
    );
    assert_eq!(seen.ino, expected_ino, "the helper was handed a different file");
}

/// The defect that destroyed 300 KiB of real data three runs
/// out of three. The file is replaced — an editor's save-and-replace, a
/// `mv`, anything — at the one moment the daemon is not holding still:
/// while it waits for the helper's `ClearIgnore` ack. A by-path reopen
/// after that point punches the replacement, which was never checked,
/// never carried a konedrive xattr, and is somebody's fresh work. With a
/// single descriptor there is nothing to re-resolve: the punch lands on
/// the inode the guard passed, whatever the name now points at.
#[tokio::test]
async fn dehydrate_punches_the_file_it_checked_even_if_the_name_is_taken_over() {
    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let path = hydrated_file(&root.path, "f.bin", 300 * 1024);
    // A second name for the same inode, so the original stays reachable
    // after the swap. A hard link is not an open descriptor, so it does
    // not disturb the write lease.
    let original = root.path.join("original.link");
    std::fs::hard_link(&path, &original).unwrap();

    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let swap_in = root.path.join("replacement.tmp");
    std::fs::write(&swap_in, vec![0xABu8; 300 * 1024]).unwrap();
    let swapped_to = path.clone();
    let _helper = fake_helper(socket_path.clone(), 0, move || {
        std::fs::rename(&swap_in, &swapped_to).unwrap();
    });
    let link = connected(&socket_path).await;

    dehydrate(&link, &root, &path).await.unwrap();

    let replacement = std::fs::read(&path).unwrap();
    assert!(
        replacement.iter().all(|b| *b == 0xAB),
        "the replacement file was punched: {} of its {} bytes are zero",
        replacement.iter().filter(|b| **b == 0).count(),
        replacement.len()
    );
    assert!(blocks_of(&path) > 64, "the replacement file lost its blocks");
    assert_eq!(
        File::open(&path).unwrap().get_xattr("user.konedrive.state").unwrap(),
        None,
        "a file konedrive never managed was stamped by the dehydration"
    );

    assert!(blocks_of(&original) < 64, "the file that was checked was not punched");
    assert_eq!(
        read_state(&File::open(&original).unwrap()).unwrap(),
        Some(State::OnlineOnly)
    );
}

/// The guard runs before the helper is involved at all: a file that may
/// not be dehydrated must not have its ignore mark cleared either, since
/// that alone costs an interception the file still needs.
#[tokio::test]
async fn dehydrate_refuses_an_unmanaged_file_without_calling_the_helper() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = fake_helper(socket_path.clone(), 0, || {});
    let link = connected(&socket_path).await;

    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let path = root.path.join("mine.txt");
    std::fs::write(&path, vec![9u8; 1 << 20]).unwrap();

    let error = dehydrate(&link, &root, &path).await.unwrap_err();
    assert!(matches!(error, DehydrateError::NotManaged), "{error:?}");

    assert!(blocks_of(&path) > 64, "an unmanaged file was punched");
    assert!(
        helper.recv_timeout(Duration::from_millis(200)).is_err(),
        "the helper must not be asked anything about a file that may not be dehydrated"
    );
}

/// A path is only a request; being inside a registered root
/// is the authority to empty something. Neither a path outside the root,
/// nor a symlink inside it pointing out, nor a root whose registration
/// has gone may reach the punch.
#[tokio::test]
async fn dehydrate_refuses_anything_that_is_not_a_file_inside_the_root() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), 0, || {});
    let link = connected(&socket_path).await;

    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let elsewhere = tempfile::tempdir().unwrap();
    let outside = hydrated_file(elsewhere.path(), "outside.bin", 4096);

    let error = dehydrate(&link, &root, &outside).await.unwrap_err();
    assert!(matches!(error, DehydrateError::OutsideRoot), "{error:?}");
    assert!(blocks_of(&outside) > 0, "a file outside the root was punched");

    let pointer = root.path.join("pointer.bin");
    std::os::unix::fs::symlink(&outside, &pointer).unwrap();
    let error = dehydrate(&link, &root, &pointer).await.unwrap_err();
    assert!(matches!(error, DehydrateError::OutsideRoot), "{error:?}");
    assert!(blocks_of(&outside) > 0, "a symlink walked out of the root");

    let inside = hydrated_file(&root.path, "f.bin", 4096);
    let unregistered = SyncRoot { path: root.path.clone(), root_id: uuid_v4() };
    let error = dehydrate(&link, &unregistered, &inside).await.unwrap_err();
    assert!(matches!(error, DehydrateError::OutsideRoot), "{error:?}");
    assert!(blocks_of(&inside) > 0, "a root that is not registered punched a file");
}
