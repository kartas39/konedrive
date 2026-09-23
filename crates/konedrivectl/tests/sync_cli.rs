//! Exercises `konedrivectl`'s testable sync-side surface (`sync_status_text`,
//! and the `Sync1Proxy` it is built on) over a private test bus, the same
//! harness shape `tests/status.rs` uses for the account side.
//!
//! `register_root` needs a helper connection to mark the root (that is what
//! the helper is for), so — like `konedrived`'s own `tests/sync_dbus.rs` —
//! the harness here runs a fake helper thread that just acknowledges
//! everything, never a real fanotify group. What that fake stands in for is
//! still the *unusual* case on a user's own machine: a standing project
//! ruling keeps the real, privileged helper inside a VM and never installs
//! it on the user's own system, so `status` (see the first half of the test
//! below, before any root is registered) must also read sensibly with no
//! helper connected at all — that is the ordinary case this CLI has to
//! handle gracefully, not an error to shout about.

use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use konedrive_dbus::testing::TestBus;
use konedrive_dbus::Sync1Proxy;
use konedrive_fs::placeholder::{write_state, State};
use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use konedrived::account::AccountService;
use konedrived::config::Paths;
use konedrived::oauth::Endpoints;
use konedrived::secret::MemoryStore;
use konedrived::sync::helper::HelperLink;
use konedrived::sync::SyncService;
use nix::sys::socket::{
    accept, bind, listen as sock_listen, socket, AddressFamily, Backlog, SockFlag, SockType, UnixAddr,
};

struct Harness {
    proxy: Sync1Proxy<'static>,
    dir: tempfile::TempDir,
    /// The daemon-side service itself, for the one test that has to take
    /// its helper away mid-run (`set_link(None)`), which nothing on the bus
    /// can do.
    service: Arc<SyncService>,
    _server: zbus::Connection,
    _account_dir: tempfile::TempDir,
    _helper_dir: tempfile::TempDir,
    _bus: TestBus,
}

/// Accepts one connection on a `SOCK_SEQPACKET` socket at `path`, greets, and
/// acknowledges every request with `Ack { errno: 0 }` — a stand-in for the
/// real, privileged helper, which these unprivileged tests cannot start (see
/// the module doc comment). Lifted from `konedrived`'s own
/// `tests/sync_dbus.rs::fake_helper`, which has the canonical copy.
fn fake_helper(path: PathBuf, refuse_clear_ignore: bool) {
    let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None).unwrap();
    let addr = UnixAddr::new(&path).unwrap();
    bind(fd.as_raw_fd(), &addr).unwrap();
    sock_listen(&fd, Backlog::new(16).unwrap()).unwrap();
    std::thread::spawn(move || {
        let accepted = accept(fd.as_raw_fd()).unwrap();
        // SAFETY: `accept` just returned a freshly opened descriptor that
        // this process now solely owns.
        let stream = unsafe { UnixStream::from_raw_fd(accepted) };
        let mut channel = Channel::new(stream).unwrap();
        channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
        while let Ok((message, _fd)) = channel.recv::<ToHelper>() {
            let errno = match message {
                ToHelper::ClearIgnore if refuse_clear_ignore => 5, // EIO
                _ => 0,
            };
            if channel.send(&ToDaemon::Ack { errno }, None).is_err() {
                break;
            }
        }
    });
}

async fn harness() -> Harness {
    harness_with_helper(true).await
}

/// As [`harness`], but `with_helper: false` never starts the fake helper or
/// connects to it — the shape `register-without-interception` exists for
/// (see the module doc comment: a standing project ruling keeps the real,
/// privileged helper inside a VM and off a user's own machine, so this is
/// the *ordinary* case for anyone running `konedrivectl` by hand).
async fn harness_with_helper(with_helper: bool) -> Harness {
    build_harness(with_helper, false, false).await
}

/// As [`harness`], with a helper that refuses every `ClearIgnore` — how a
/// test makes startup recovery genuinely fail (see [`stuck_root`]).
async fn harness_refusing_clear_ignore() -> Harness {
    build_harness(true, false, true).await
}

/// As [`harness`], but with `RegisterRoot`'s sign-in gate wired to an
/// account nobody has signed in to — every other harness passes no account
/// at all, which `SyncService` treats as "nothing to check".
async fn harness_signed_out() -> Harness {
    build_harness(true, true, false).await
}

async fn build_harness(with_helper: bool, gate_on_sign_in: bool, refuse_clear_ignore: bool) -> Harness {
    let bus = TestBus::start();

    let helper_dir = tempfile::tempdir().unwrap();
    let link = if with_helper {
        let socket_path = helper_dir.path().join("helper.sock");
        fake_helper(socket_path.clone(), refuse_clear_ignore);
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        Some(link)
    } else {
        None
    };
    let account_dir = tempfile::tempdir().unwrap();
    let account_service = AccountService::new(
        Paths::in_dir(account_dir.path()),
        Endpoints::microsoft(),
        Arc::new(MemoryStore::default()),
        Duration::from_secs(5),
    )
    .unwrap();
    // A fresh account starts signed out, which is exactly what the one test
    // that asks for the gate needs.
    let account = gate_on_sign_in.then(|| account_service.state().clone());
    let sync_service = SyncService::new(link, account, None);
    // Without a helper, nothing is bound at this path: a punch with no link
    // goes ahead (Ruling H146) whatever this machine runs at the real one.
    sync_service.set_helper_socket(helper_dir.path().join("helper.sock"));

    let server = konedrived::dbus::serve(bus.builder(), account_service, None).await.unwrap();
    konedrived::sync::dbus::attach(&server, Arc::clone(&sync_service)).await.unwrap();

    let client = bus.connect().await;
    let proxy = Sync1Proxy::new(&client).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    Harness {
        proxy,
        dir,
        service: sync_service,
        _server: server,
        _account_dir: account_dir,
        _helper_dir: helper_dir,
        _bus: bus,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_text_reports_no_folder_then_the_registered_one() {
    let f = harness().await;

    let text = konedrivectl::sync_status_text(&f.proxy).await.unwrap();
    assert!(text.contains("Folder:"), "{text}");
    assert!(text.contains("(none)"), "{text}");

    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.register_root(root.to_str().unwrap()).await.unwrap();
    // `f.proxy` is a caching proxy (the same one `sync_status_text` is handed
    // in `main.rs`, built fresh per CLI invocation there); its properties
    // update from the `PropertiesChanged` signal `sync::dbus::attach` emits,
    // which lands on a task independent of the `RegisterRoot` reply this
    // test just awaited, so it is not yet guaranteed to have landed. Poll
    // rather than assert immediately — the same reason `status.rs` polls for
    // `client_id` after `SetClientId`, and `wait_for_sign_in`'s doc comment
    // spells out for `Account1`.
    for _ in 0..500 {
        if f.proxy.root_state().await.unwrap() == "ready" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let text = konedrivectl::sync_status_text(&f.proxy).await.unwrap();
    assert!(text.contains(root.to_str().unwrap()), "{text}");
    assert!(text.contains("ready"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_surfaces_as_an_error_not_a_success() {
    let f = harness().await;
    let root = f.dir.path().join("NotEmpty");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("x"), b"x").unwrap();

    let error = f.proxy.register_root(root.to_str().unwrap()).await.unwrap_err();
    assert!(format!("{error}").contains("empty"), "{error}");
}

/// Marks `root_dir` as an already-registered root — Ruling H78 exempts a
/// folder that already carries `user.konedrive.root` from the "must be
/// empty" check, which is what lets a file be planted in it *before*
/// `register_root` runs, since recovery runs as part of that call — and
/// leaves one file in it `dehydrating`. Registered through
/// [`harness_refusing_clear_ignore`], whose helper refuses the `ClearIgnore`
/// recovery must have before it may punch that file, recovery fails for it —
/// the exact shape
/// `konedrived::sync::tests::a_failed_recovery_surfaces_through_root_state_and_last_error`
/// uses to force `RecoveryReport::failed > 0`; C1 and C2 both need that same
/// "the call still returns `Ok`, but the root needs attention" outcome, so
/// it is factored out here rather than duplicated. (It used to hold the file
/// open instead, so that recovery's lease was refused; a file in use is
/// `busy` now, not a failure — the final review's m11.)
fn stuck_root(root_dir: &std::path::Path) -> PathBuf {
    xattr::set(root_dir, "user.konedrive.root", b"1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d").unwrap();
    let path = root_dir.join("stuck.bin");
    std::fs::write(&path, vec![1u8; 4096]).unwrap();
    let file = std::fs::File::options().read(true).write(true).open(&path).unwrap();
    write_state(&file, State::Dehydrating).unwrap();
    path
}

/// C2: nothing before this test drove `RootState = error` with a non-empty
/// `LastError` through `sync_status_text` — the fake helper used everywhere
/// else in this file acks every request, so ordinary recovery never fails.
/// `stuck_root`, with a helper that refuses `ClearIgnore`, forces it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_text_shows_a_recovery_failure_as_error_not_a_success() {
    let f = harness_refusing_clear_ignore().await;
    let root = f.dir.path().join("Stuck");
    std::fs::create_dir(&root).unwrap();
    let _stuck_path = stuck_root(&root);

    // `SyncService::bind`'s own doc comment: a per-file recovery failure
    // does not fail the call, so this must still return `Ok(())`.
    f.proxy.register_root(root.to_str().unwrap()).await.unwrap();

    let text = konedrivectl::sync_status_text(&f.proxy).await.unwrap();
    assert!(text.contains("error"), "the state must read error: {text}");
    assert!(text.contains("Last error:"), "the detail must be shown: {text}");
    assert!(
        text.contains('1'),
        "the failure count belongs in the detail, not just the state: {text}"
    );
}

// --- I1: the compiled binary, not just the library it is built from ------
//
// Every test above calls `konedrivectl::sync_status_text` or the raw proxy
// directly. Nothing exercised `main.rs` itself: argument parsing, the seven
// (now eight) `sync` subcommands' own success strings, or `absolute_str`'s
// error path. `TestBus::address()` makes that reachable without a session
// bus: point `DBUS_SESSION_BUS_ADDRESS` at the private one and run the real
// binary, exactly as a user's shell would.

fn run(bus_addr: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_konedrivectl"))
        .args(args)
        .env("DBUS_SESSION_BUS_ADDRESS", bus_addr)
        .output()
        .expect("failed to run the konedrivectl binary")
}

fn out_text(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn err_text(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Drives the whole offline workflow through the binary: register, populate,
/// hydrate, dehydrate, forget — checking both the success strings `main.rs`
/// prints and the *effect* of each command (via `sync state`), not just its
/// exit code. Checking only exit codes would miss the `Hydrate`/`Dehydrate`
/// arms being swapped, since both are `Ok(())` either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_drives_registration_populate_hydrate_and_dehydrate() {
    let f = harness().await;
    let addr = f._bus.address();

    let out = run(addr, &["sync", "status"]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).contains("(none)"), "{}", out_text(&out));

    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    let out = run(addr, &["sync", "register", root.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        out_text(&out).contains(&format!("Folder registered: {}", root.display())),
        "{}",
        out_text(&out)
    );

    let source = f.dir.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("note.txt"), b"hello").unwrap();
    let out = run(addr, &["sync", "populate-from", source.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).contains("Created 1 placeholders."), "{}", out_text(&out));

    let file = root.join("note.txt");
    let out = run(addr, &["sync", "state", file.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out_text(&out).trim(), "online-only", "{}", out_text(&out));

    let out = run(addr, &["sync", "hydrate", file.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).contains("Downloaded."), "{}", out_text(&out));

    let out = run(addr, &["sync", "state", file.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        out_text(&out).trim(),
        "hydrated",
        "swapping the Hydrate/Dehydrate arms would leave this online-only: {}",
        out_text(&out)
    );

    let out = run(addr, &["sync", "dehydrate", file.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).contains("Freed up."), "{}", out_text(&out));

    let out = run(addr, &["sync", "state", file.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        out_text(&out).trim(),
        "online-only",
        "swapping the Hydrate/Dehydrate arms would leave this hydrated: {}",
        out_text(&out)
    );

    let out = run(addr, &["sync", "forget"]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).contains("Folder forgotten."), "{}", out_text(&out));
}

/// I2's whole point, proven at the binary level: `register-without-
/// interception` needs no helper connection at all, and says plainly, every
/// time, that files in the folder read as zeros until hydrated by hand.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_register_without_interception_needs_no_helper_and_says_so() {
    let f = harness_with_helper(false).await;
    let addr = f._bus.address();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();

    let out = run(addr, &["sync", "register-without-interception", root.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        out_text(&out)
            .contains(&format!("Folder registered without interception: {}", root.display())),
        "{}",
        out_text(&out)
    );
    assert!(
        err_text(&out).to_lowercase().contains("zeros"),
        "the zero-read risk must be stated on every success, not just left in LastError: {}",
        err_text(&out)
    );

    let out = run(addr, &["sync", "status"]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).contains("no-interception"), "{}", out_text(&out));
}

/// `absolute_str`'s error path: a path that does not exist must be caught
/// before any D-Bus call is made, with a message that names the path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_reports_a_bad_path_before_touching_the_daemon() {
    let f = harness().await;
    let addr = f._bus.address();

    let out = run(addr, &["sync", "hydrate", "/no/such/path/konedrivectl-test"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(err_text(&out).contains("no such path"), "{}", err_text(&out));
}

/// The final review's m8 (Ruling H144). `absolute_str` canonicalised every
/// path, so `sync register <symlink>` resolved the link and registered its
/// target — silently, while spec §10 says a symbolic link is refused as a
/// root. The link's own name has to reach the daemon, which refuses it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_register_of_a_symlink_is_refused_not_resolved() {
    let f = harness_with_helper(false).await;
    let addr = f._bus.address();
    let target = f.dir.path().join("Target");
    std::fs::create_dir(&target).unwrap();
    let link = f.dir.path().join("Link");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let out = run(addr, &["sync", "register-without-interception", link.to_str().unwrap()]);
    assert!(!out.status.success(), "the symlink's target was registered: {out:?}");
    assert!(err_text(&out).contains("symbolic link"), "{}", err_text(&out));
    let status = run(addr, &["sync", "status"]);
    assert!(
        !out_text(&status).contains(target.to_str().unwrap()),
        "the target is registered: {}",
        out_text(&status)
    );
}

/// A named refusal (`NotEmpty`) must reach the terminal as a failure, never
/// as the bare "Folder registered" success line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_register_refusal_never_prints_a_bare_success() {
    let f = harness().await;
    let addr = f._bus.address();
    let root = f.dir.path().join("NotEmpty");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("x"), b"x").unwrap();

    let out = run(addr, &["sync", "register", root.to_str().unwrap()]);
    assert!(!out.status.success(), "{out:?}");
    assert!(!out_text(&out).contains("Folder registered"), "{}", out_text(&out));
    assert!(err_text(&out).contains("empty"), "{}", err_text(&out));
}

/// C1, at the binary a user actually runs: `register_root` returns `Ok(())`
/// even when startup recovery could not reset every file (`RootState`
/// flips to `error` instead) — before this task's fix, `main.rs` printed
/// "Folder registered: {path}" and exited 0 regardless, which is exactly
/// the failure C1 describes: the one thing this interface exists to make
/// visible, wearing a success message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_register_does_not_print_bare_success_when_recovery_failed() {
    let f = harness_refusing_clear_ignore().await;
    let addr = f._bus.address();
    let root = f.dir.path().join("Stuck");
    std::fs::create_dir(&root).unwrap();
    let _stuck_path = stuck_root(&root);

    let out = run(addr, &["sync", "register", root.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "a register that leaves the root unrecovered must not exit 0: {out:?}"
    );
    assert!(
        !out_text(&out).contains(&format!("Folder registered: {}", root.display())),
        "C1: an unqualified success line must not print when recovery failed: {}",
        out_text(&out)
    );
    assert!(
        err_text(&out).to_lowercase().contains("recover"),
        "{}",
        err_text(&out)
    );
}

// --- Named refusals, explained ------------------------------------------
//
// Every refusal `Sync1` can make arrives as its own D-Bus error name under
// `konedrive_dbus::ERROR_PREFIX`. Until these tests, `konedrivectl` read
// none of them: every refusal reached the terminal as anyhow's rendering of
// the raw `zbus::Error` — `Error: org.konedrive.Error.ModifiedLocally: the
// file was modified locally` — which is the daemon's message with a D-Bus
// name in front of it. Each test below drives one refusal through the real
// binary and asserts on what a person is told: what happened to *their*
// file, and what to do next. None of the phrases asserted on appears in the
// daemon's own message, so echoing that message cannot pass.

/// Runs a `sync` command the daemon must refuse and returns what the person
/// running it was told. Every refusal exits non-zero, prints nothing on
/// stdout (so nothing there can read as success), and keeps the D-Bus error
/// name out of the explanation: the name is how the CLI knows which
/// refusal it is, not something the person reading it should have to decode.
fn refused(bus_addr: &str, args: &[&str]) -> String {
    let out = run(bus_addr, args);
    assert!(!out.status.success(), "a refusal must not exit 0: {out:?}");
    assert!(out_text(&out).is_empty(), "a refusal must print nothing on stdout: {out:?}");
    let told = err_text(&out);
    assert!(
        !told.contains(konedrive_dbus::ERROR_PREFIX),
        "the D-Bus error name is for programs, not for the person reading this: {told}"
    );
    told
}

/// A registered root, populated from a source directory holding one
/// 8 KiB file, `doc.bin`. Returns the placeholder's path.
async fn populated(f: &Harness) -> PathBuf {
    let source = f.dir.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("doc.bin"), vec![7u8; 8192]).unwrap();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.register_root(root.to_str().unwrap()).await.unwrap();
    f.proxy.populate_from_directory(source.to_str().unwrap()).await.unwrap();
    root.join("doc.bin")
}

/// `ModifiedLocally` from `dehydrate`: the one refusal whose whole point is
/// that the alternative loses the user's work.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_dehydrate_of_a_locally_modified_file_says_the_edits_would_be_lost() {
    let f = harness().await;
    let addr = f._bus.address();
    let file = populated(&f).await;
    f.proxy.hydrate(file.to_str().unwrap()).await.unwrap();
    std::fs::write(&file, b"what the user typed").unwrap();

    let told = refused(addr, &["sync", "dehydrate", file.to_str().unwrap()]);
    assert!(told.contains(file.to_str().unwrap()), "name the file: {told}");
    assert!(told.contains("has not been uploaded"), "{told}");
    assert!(told.contains("lose your edits"), "{told}");
    assert_eq!(std::fs::read(&file).unwrap(), b"what the user typed");
}

/// `ModifiedLocally` from `hydrate`: the same refusal pointing the other
/// way — downloading would overwrite the edit rather than free it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_hydrate_of_a_locally_modified_file_says_the_edits_would_be_overwritten() {
    let f = harness().await;
    let addr = f._bus.address();
    let file = populated(&f).await;
    f.proxy.hydrate(file.to_str().unwrap()).await.unwrap();
    std::fs::write(&file, b"what the user typed").unwrap();

    let told = refused(addr, &["sync", "hydrate", file.to_str().unwrap()]);
    assert!(told.contains("has not been uploaded"), "{told}");
    assert!(told.contains("overwrite your edits"), "{told}");
    assert!(!told.contains("lose your edits"), "this is the hydrate wording, not dehydrate's: {told}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_dehydrate_of_an_online_only_file_says_there_is_nothing_to_free() {
    let f = harness().await;
    let addr = f._bus.address();
    let file = populated(&f).await;

    let told = refused(addr, &["sync", "dehydrate", file.to_str().unwrap()]);
    assert!(told.contains("no space to free"), "{told}");
    assert!(!told.contains("has not been uploaded"), "NotHydrated is not ModifiedLocally: {told}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_dehydrate_of_an_open_file_says_to_close_it() {
    let f = harness().await;
    let addr = f._bus.address();
    let file = populated(&f).await;
    f.proxy.hydrate(file.to_str().unwrap()).await.unwrap();
    let _held_open = std::fs::File::open(&file).unwrap();

    let told = refused(addr, &["sync", "dehydrate", file.to_str().unwrap()]);
    assert!(told.contains("open in another program"), "{told}");
    assert!(told.contains("Close it"), "{told}");
}

/// `NoHelper` from `register`: the refusal every user of this CLI on their
/// own machine meets first, so it has to name the way forward.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_register_without_a_helper_offers_the_explicit_mode_and_its_cost() {
    let f = harness_with_helper(false).await;
    let addr = f._bus.address();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();

    let told = refused(addr, &["sync", "register", root.to_str().unwrap()]);
    assert!(told.contains("helper is not running"), "{told}");
    assert!(
        told.contains(&format!("konedrivectl sync register-without-interception {}", root.display())),
        "{told}"
    );
    assert!(told.contains("zeros"), "the explicit mode's cost belongs next to the offer: {told}");
}

/// `NoHelper` from `dehydrate`: a root registered *with* interception whose
/// helper has since gone away. Freeing space then has to be refused (the
/// helper must resume watching the file first), and the file is untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_dehydrate_without_the_helper_changes_nothing_and_says_why() {
    let f = harness().await;
    let addr = f._bus.address();
    let file = populated(&f).await;
    f.proxy.hydrate(file.to_str().unwrap()).await.unwrap();
    f.service.set_link(None);

    let told = refused(addr, &["sync", "dehydrate", file.to_str().unwrap()]);
    assert!(told.contains("helper is not connected"), "{told}");
    assert!(told.contains("nothing was changed"), "{told}");
    assert!(
        !told.contains("register-without-interception"),
        "this is the dehydrate wording, not register's: {told}"
    );
    assert_eq!(std::fs::read(&file).unwrap(), vec![7u8; 8192]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_register_while_signed_out_says_to_log_in() {
    let f = harness_signed_out().await;
    let addr = f._bus.address();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();

    let told = refused(addr, &["sync", "register", root.to_str().unwrap()]);
    assert!(told.contains("konedrivectl login"), "{told}");
    assert!(told.contains("register-without-interception"), "{told}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_a_second_register_names_the_folder_already_registered() {
    let f = harness().await;
    let addr = f._bus.address();
    let first = f.dir.path().join("OneDrive");
    std::fs::create_dir(&first).unwrap();
    f.proxy.register_root(first.to_str().unwrap()).await.unwrap();
    let second = f.dir.path().join("Another");
    std::fs::create_dir(&second).unwrap();

    let told = refused(addr, &["sync", "register", second.to_str().unwrap()]);
    assert!(told.contains(first.to_str().unwrap()), "name the folder that is in the way: {told}");
    assert!(told.contains("konedrivectl sync forget"), "{told}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_a_file_command_with_no_folder_says_how_to_register_one() {
    let f = harness().await;
    let addr = f._bus.address();
    let file = f.dir.path().join("doc.bin");
    std::fs::write(&file, b"x").unwrap();

    let told = refused(addr, &["sync", "hydrate", file.to_str().unwrap()]);
    assert!(told.contains("no sync folder is registered"), "{told}");
    assert!(told.contains("konedrivectl sync register"), "{told}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_hydrate_with_no_source_says_to_populate_first() {
    let f = harness().await;
    let addr = f._bus.address();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.register_root(root.to_str().unwrap()).await.unwrap();
    let file = root.join("doc.bin");
    std::fs::write(&file, b"x").unwrap();

    let told = refused(addr, &["sync", "hydrate", file.to_str().unwrap()]);
    assert!(told.contains("konedrivectl sync populate-from"), "{told}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_a_file_outside_the_folder_is_named_as_outside_it() {
    let f = harness().await;
    let addr = f._bus.address();
    populated(&f).await;
    let outside = f.dir.path().join("elsewhere.bin");
    std::fs::write(&outside, b"not ours").unwrap();

    let told = refused(addr, &["sync", "hydrate", outside.to_str().unwrap()]);
    assert!(told.contains(outside.to_str().unwrap()), "{told}");
    assert!(told.contains("not a regular file inside the sync folder"), "{told}");
    assert_eq!(std::fs::read(&outside).unwrap(), b"not ours");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_a_file_of_the_users_own_is_named_as_theirs() {
    let f = harness().await;
    let addr = f._bus.address();
    let file = populated(&f).await;
    let stray = file.with_file_name("stray.txt");
    std::fs::write(&stray, b"mine").unwrap();

    let told = refused(addr, &["sync", "hydrate", stray.to_str().unwrap()]);
    assert!(told.contains(stray.to_str().unwrap()), "{told}");
    assert!(told.contains("a file of your own"), "{told}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_register_of_a_non_empty_folder_says_to_choose_an_empty_one() {
    let f = harness().await;
    let addr = f._bus.address();
    let root = f.dir.path().join("NotEmpty");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("x"), b"x").unwrap();

    let told = refused(addr, &["sync", "register", root.to_str().unwrap()]);
    assert!(told.contains(root.to_str().unwrap()), "{told}");
    assert!(told.contains("choose an empty folder"), "{told}");
}

/// `Unsupported` carries the daemon's specific reason (which feature is
/// missing, or that the path is not a directory at all); that detail is
/// kept, framed by what it means for the folder the user typed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_register_of_a_file_says_it_cannot_be_the_sync_folder_and_why() {
    let f = harness().await;
    let addr = f._bus.address();
    let file = f.dir.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();

    let told = refused(addr, &["sync", "register", file.to_str().unwrap()]);
    assert!(told.contains(file.to_str().unwrap()), "{told}");
    assert!(told.contains("cannot be used as the sync folder"), "{told}");
    assert!(told.contains("not a directory"), "the daemon's specific reason stays: {told}");
}

/// `Failed` has no name of its own, so its detail is all there is — but the
/// person still has to be told which file it was about.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_an_io_failure_names_the_file_it_was_about() {
    let f = harness().await;
    let addr = f._bus.address();
    let file = populated(&f).await;
    std::fs::remove_file(f.dir.path().join("source/doc.bin")).unwrap();

    let told = refused(addr, &["sync", "hydrate", file.to_str().unwrap()]);
    assert!(told.contains(&format!("downloading {} failed", file.display())), "{told}");
    assert!(told.contains("Input/output error"), "the cause stays: {told}");
}

// --- `sync status` and the no-interception mode ---------------------------
//
// A standing ruling keeps the privileged helper off the user's own machine,
// so `no-interception` is the ordinary state there — and its cost (a file
// that is not downloaded reads as zeros) must be on screen every time the
// user asks, in the CLI's own words, not left to a daemon property that
// happens to restate it or to the user's memory.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_status_says_plainly_that_nothing_intercepts_opens() {
    let f = harness_with_helper(false).await;
    let addr = f._bus.address();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.register_root_without_interception(root.to_str().unwrap()).await.unwrap();

    let out = run(addr, &["sync", "status"]);
    assert!(out.status.success(), "{out:?}");
    let text = out_text(&out);
    let opens = text
        .lines()
        .find(|line| line.starts_with("Opens:"))
        .unwrap_or_else(|| panic!("no Opens: line: {text}"));
    assert!(opens.to_lowercase().contains("not intercepted"), "{text}");
    assert!(opens.contains("zeros"), "{text}");
    assert!(opens.contains("konedrivectl sync hydrate"), "say how to get the real bytes: {text}");
}

/// The contrast: an intercepted folder must not carry the zeros warning, or
/// the warning becomes noise a user learns to skip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_status_of_an_intercepted_folder_does_not_warn_of_zeros() {
    let f = harness().await;
    let addr = f._bus.address();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.register_root(root.to_str().unwrap()).await.unwrap();

    let out = run(addr, &["sync", "status"]);
    assert!(out.status.success(), "{out:?}");
    let text = out_text(&out);
    let opens = text
        .lines()
        .find(|line| line.starts_with("Opens:"))
        .unwrap_or_else(|| panic!("no Opens: line: {text}"));
    assert!(opens.contains("intercepted"), "{text}");
    assert!(!opens.to_lowercase().contains("not intercepted"), "{text}");
    assert!(!text.contains("zeros"), "{text}");
}
