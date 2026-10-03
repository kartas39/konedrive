use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use konedrive_fs::placeholder::{
    read_progress, read_stamp, read_state, write_progress, write_stamp, Progress, State,
};
use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use nix::sys::socket::{
    accept, bind, listen as sock_listen, socket, AddressFamily, Backlog, SockFlag, SockType,
    UnixAddr,
};

use super::*;

/// A sync root the way `register_root` would have left one: resolved
/// path, root id on the folder.
fn test_root(dir: &Path) -> SyncRoot {
    let path = dir.canonicalize().unwrap();
    let root_id = uuid_v4();
    let handle = File::open(&path).unwrap();
    handle.set_xattr(XATTR_ROOT, root_id.as_bytes()).unwrap();
    SyncRoot { path, root_id }
}

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

fn open_rw(path: &Path) -> File {
    File::options().read(true).write(true).open(path).unwrap()
}

/// The recovery every test in this module runs: through a link to its
/// fake helper, which is what an intercepted root recovers through.
/// Shadows `super::recover` on purpose, so the tests read as they did
/// before recovery took a [`Clearance`].
async fn recover(link: &HelperLink, root: &SyncRoot) -> Result<RecoveryReport, RecoveryError> {
    super::recover(&Clearance::Link(link.clone()), root, &InodeLocks::new()).await
}

fn blocks_of(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().blocks()
}

// --- The local guard -------------------------------------------------

#[tokio::test]
async fn refuses_a_non_empty_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stray.txt"), b"x").unwrap();
    let error = check_root_candidate(dir.path()).unwrap_err();
    assert!(matches!(error, RegisterError::NotEmpty), "{error:?}");
}

#[tokio::test]
async fn accepts_an_empty_directory_on_a_supported_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    check_root_candidate(dir.path()).unwrap();
}

/// A file, a missing path and a symlink are all "not a folder you can
/// register", and each has to say so as itself rather than as whatever
/// the next syscall along happens to complain about.
#[tokio::test]
async fn refuses_anything_that_is_not_a_real_directory() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();
    assert!(
        matches!(check_root_candidate(&file).unwrap_err(), RegisterError::NotADirectory),
        "a plain file must be refused as not a directory"
    );
    assert!(
        matches!(
            check_root_candidate(&dir.path().join("nope")).unwrap_err(),
            RegisterError::NotADirectory
        ),
        "a path that does not exist must be refused as not a directory"
    );

    let target = dir.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let error = check_root_candidate(&link).unwrap_err();
    assert!(
        matches!(&error, RegisterError::Unsupported(why) if why.contains("symbolic link")),
        "{error:?}"
    );
}

/// The probe is not decoration: a directory can be empty, be a
/// directory, and still be unable to hold a single placeholder. Here it
/// is one we cannot write into at all — the cheapest unprivileged stand-in
/// for a filesystem that refuses the features, and the one thing that
/// fails if `probe_dir` is dropped from the checks.
#[tokio::test]
async fn refuses_a_directory_the_probe_cannot_use() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let readonly = dir.path().join("readonly");
    std::fs::create_dir(&readonly).unwrap();
    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o500)).unwrap();

    let error = check_root_candidate(&readonly).unwrap_err();

    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        matches!(&error, RegisterError::Unsupported(why) if why.contains(&readonly.display().to_string())),
        "{error:?}"
    );
}

/// The empty requirement belongs to *first* registration
/// only. A folder that already carries a valid `user.konedrive.root`
/// is a root being re-registered, and re-registration is expected to
/// find it full of exactly the placeholders and hydrated files this
/// daemon itself put there — refusing it as though it were some other,
/// foreign non-empty folder would make every restart unregister every
/// root.
///. H78 waives the empty check for a folder that "already
/// carries a root id", and nothing ever removes that xattr again — so if
/// any string counts, one `setfattr -n user.konedrive.root -v x` makes a
/// folder full of somebody's existing documents registerable, for good.
/// Only the id form this daemon actually mints may waive it.
#[tokio::test]
async fn a_value_that_is_not_one_of_our_root_ids_does_not_waive_the_empty_check() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("their-thesis.odt"), b"x").unwrap();
    let handle = File::open(dir.path()).unwrap();

    for bogus in [
        "",                                     // present but empty
        "x",                                    // the one-character setfattr
        "not-a-uuid",                           // a word
        "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5",  // 35 characters
        "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d6", // 37
        "1c2e4f5a-0b3c-3d5e-8f60-71829a3b4c5d", // version 3, not 4
        "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4czd", // not hex
        "1c2e4f5a0b3c4d5e8f6071829a3b4c5d6e7f", // 36 characters, no dashes
    ] {
        handle.set_xattr(XATTR_ROOT, bogus.as_bytes()).unwrap();
        let error = check_root_candidate(dir.path()).unwrap_err();
        assert!(
            matches!(error, RegisterError::NotEmpty),
            "{bogus:?} waived the empty check: {error:?}"
        );
    }

    handle.set_xattr(XATTR_ROOT, uuid_v4().as_bytes()).unwrap();
    check_root_candidate(dir.path()).expect("a real root id must still waive it");
}

/// The other half of H90: a value that is not an id of ours names no
/// registration the helper could be holding, so "never
/// overwrite an existing id" does not apply to it — a real one is minted
/// over the top rather than the junk being offered to the helper as this
/// root's name.
#[tokio::test]
async fn a_bogus_root_id_is_replaced_by_a_real_one() {
    let dir = tempfile::tempdir().unwrap();
    File::open(dir.path()).unwrap().set_xattr(XATTR_ROOT, b"x").unwrap();

    let (_dir, root) = prepare_root(dir.path()).unwrap();

    // Spelled out rather than asked of `looks_like_a_root_id`, which is
    // the function under test: it would agree with itself.
    assert_ne!(root.root_id, "x", "the junk value was offered to the helper as this root");
    assert_eq!(root.root_id.len(), 36, "{}", root.root_id);
    let fields: Vec<&str> = root.root_id.split('-').collect();
    assert_eq!(fields.iter().map(|f| f.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
    assert!(fields[2].starts_with('4'), "{}", root.root_id);
    assert_eq!(
        File::open(dir.path()).unwrap().get_xattr(XATTR_ROOT).unwrap().as_deref(),
        Some(root.root_id.as_bytes())
    );
}

/// The probe answers first. `read_root_id` is a `getxattr` in
/// the `user.*` namespace — the very thing the probe exists to establish
/// is available — so asking it first replaces the probe's purpose-built
/// message with an errno about an attribute name, on a folder it does not
/// name. Here the folder is both unusable and non-empty, and it is the
/// unusability that must be reported: a folder that cannot hold a
/// placeholder at all cannot be a sync root whether it is empty or not.
#[tokio::test]
async fn an_unusable_directory_is_reported_as_unusable_even_when_it_is_not_empty() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let readonly = dir.path().join("readonly");
    std::fs::create_dir(&readonly).unwrap();
    std::fs::write(readonly.join("stray.txt"), b"x").unwrap();
    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o500)).unwrap();

    let error = check_root_candidate(&readonly).unwrap_err();

    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        matches!(&error, RegisterError::Unsupported(why) if why.contains(&readonly.display().to_string())),
        "{error:?}"
    );
}

#[tokio::test]
async fn accepts_a_non_empty_directory_that_already_carries_its_own_root_id() {
    let dir = tempfile::tempdir().unwrap();
    let handle = File::open(dir.path()).unwrap();
    handle.set_xattr(XATTR_ROOT, uuid_v4().as_bytes()).unwrap();
    std::fs::write(dir.path().join("stray.txt"), b"x").unwrap();

    check_root_candidate(dir.path()).unwrap();
}

// --- Root registration -----------------------------------------------

#[tokio::test]
async fn register_root_stamps_the_folder_and_tells_the_helper() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = fake_helper(socket_path.clone(), 0, || {});
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let root = register_root(&link, dir.path()).await.unwrap();

    assert_eq!(root.path, dir.path().canonicalize().unwrap());
    let on_disk = File::open(dir.path()).unwrap().get_xattr(XATTR_ROOT).unwrap();
    assert_eq!(
        on_disk.as_deref(),
        Some(root.root_id.as_bytes()),
        "the folder must carry the id the helper was told about"
    );
    asked_to(&helper, "RegisterRoot");
}

/// The first attempt fails after the folder has been
/// stamped; the second must offer the helper the *same* id, because the
/// helper may already be holding it — a fresh one is refused as a
/// conflicting registration of the same directory, for good.
#[tokio::test]
async fn register_root_reuses_the_id_already_on_the_folder() {
    let dir = tempfile::tempdir().unwrap();

    let refusing = tempfile::tempdir().unwrap();
    let refusing_socket = refusing.path().join("helper.sock");
    let _refusing = fake_helper_refusing_everything(refusing_socket.clone());
    let (refused_link, _r) = HelperLink::connect(&refusing_socket).await.unwrap();
    let error = register_root(&refused_link, dir.path()).await.unwrap_err();
    assert!(matches!(error, RegisterError::Helper(_)), "{error:?}");

    let first = File::open(dir.path()).unwrap().get_xattr(XATTR_ROOT).unwrap().unwrap();

    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), 0, || {});
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let root = register_root(&link, dir.path()).await.unwrap();

    assert_eq!(
        root.root_id.as_bytes(),
        first.as_slice(),
        "the retry minted a new id; the helper would refuse it as a second registration of \
         the same directory, EINVAL, forever"
    );
    let on_disk = File::open(dir.path()).unwrap().get_xattr(XATTR_ROOT).unwrap();
    assert_eq!(on_disk.as_deref(), Some(first.as_slice()));
}

/// The actual startup scenario: the daemon registers a
/// folder, populates it (placeholders, hydrated files — anything, here
/// just a plain file stands in), then restarts and registers the same
/// folder again. The second call must not be refused `NotEmpty`.
#[tokio::test]
async fn register_root_accepts_a_populated_folder_on_a_second_registration() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), 0, || {});
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let first = register_root(&link, dir.path()).await.unwrap();

    // Stand in for what a real run would have left behind.
    std::fs::write(dir.path().join("placeholder.bin"), vec![1u8; 4096]).unwrap();

    let second = register_root(&link, dir.path()).await.unwrap();
    assert_eq!(
        second.root_id, first.root_id,
        "re-registration must not mint a new id (Ruling H70)"
    );
}

#[test]
fn uuid_v4_mints_a_fresh_identifier_every_time() {
    let minted: std::collections::HashSet<String> = (0..64).map(|_| uuid_v4()).collect();
    assert_eq!(minted.len(), 64, "root ids must be unique: two folders must never collide");
    for id in &minted {
        assert_eq!(id.len(), 36, "{id}");
        let fields: Vec<&str> = id.split('-').collect();
        assert_eq!(fields.iter().map(|f| f.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()), "{id}");
        assert!(fields[2].starts_with('4'), "version 4 expected: {id}");
        assert!(matches!(&fields[3][0..1], "8" | "9" | "a" | "b"), "variant expected: {id}");
    }
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
fn with_syscalls_denied<T, F>(
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
struct Seen {
    message: String,
    state: Option<State>,
    ino: u64,
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
fn fake_helper(
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
fn fake_helper_refusing_everything(path: PathBuf) -> std::sync::mpsc::Receiver<Seen> {
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

async fn connected(socket_path: &Path) -> HelperLink {
    HelperLink::connect(socket_path).await.unwrap().0
}

/// The first request of a given kind the fake helper saw, or a failure
/// if it never arrived. Bounded, so a mutation that stops calling the
/// helper at all fails the test instead of hanging it.
fn asked_to(seen: &std::sync::mpsc::Receiver<Seen>, what: &str) -> Seen {
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

// --- Startup recovery -------------------------------------

/// A file in the state a crash left it in: content on disk, the state
/// xattr saying what was happening to it, and the stamp a finished
/// hydration would have written — which §4.4 requires recovery to remove.
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

/// How many descriptors this process has open right now. Both samples
/// include the one `read_dir` itself uses, so the difference is the walk's.
fn open_descriptors() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
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

/// Open, decide, punch, close — one file at a time. The
/// version this replaces opened every regular file in a directory
/// `O_RDWR`, whatever its state, and held all of those descriptors until
/// the directory was finished; with `RLIMIT_NOFILE` at systemd's default
/// of 1024 that silently defeated recovery of a large folder, healthy or
/// not. Measured from inside the walk — the helper's hook runs while
/// recovery is blocked on the `ClearIgnore` ack for the one interrupted
/// file, which is the moment the old version was holding all 400 of the
/// others.
#[tokio::test]
async fn recovery_holds_one_file_open_at_a_time() {
    const BYSTANDERS: usize = 400;

    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    for i in 0..BYSTANDERS {
        interrupted_file(&root.path, &format!("hydrated-{i}.bin"), State::Hydrated, 64);
    }
    interrupted_file(&root.path, "interrupted.bin", State::Dehydrating, 64);

    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let during = Arc::new(AtomicUsize::new(0));
    let sampler = Arc::clone(&during);
    let _helper = fake_helper(socket_path.clone(), 0, move || {
        sampler.store(open_descriptors(), Ordering::SeqCst);
    });
    let link = connected(&socket_path).await;

    let before = open_descriptors();
    let report = recover(&link, &root).await.unwrap();
    assert_eq!(report.reset, 1, "{report:?}");
    assert_eq!(report.scanned, BYSTANDERS + 1, "{report:?}");

    let held = during.load(Ordering::SeqCst).saturating_sub(before);
    assert!(
        held < 64,
        "{held} more descriptors were open mid-walk than before it, with {BYSTANDERS} \
         bystander files in the directory: the walk is holding a descriptor per file, so a \
         large folder exhausts the table and the rest of it is skipped in silence"
    );
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
/// not counted, not logged and never noticed, while §5.2 has the helper
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

/// A refusal has to keep its kind all the way out of
/// `reset_interrupted`. `Result<(), String>` flattened `Refused`,
/// `Timeout`, `NotRunning` and `ENOSPC` into one text field, and those
/// are four different situations with four different answers — retry
/// now, retry later, start the helper, free some space.
#[tokio::test]
async fn a_refusal_keeps_its_kind() {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let _helper = fake_helper(socket_path.clone(), libc::EPERM, || {});
    let link = connected(&socket_path).await;

    let dir = tempfile::tempdir().unwrap();
    let root = test_root(dir.path());
    let path = interrupted_file(&root.path, "a.bin", State::Dehydrating, 8192);

    let error = reset_interrupted(&Clearance::Link(link.clone()), open_rw(&path)).await.unwrap_err();
    assert!(
        matches!(error, ResetError::Helper(HelperError::Refused(libc::EPERM))),
        "{error:?}"
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
            "sync::root::tests::a_subtree_on_another_filesystem_is_counted_and_logged",
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
