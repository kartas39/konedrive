//! Exercises `konedrivectl`'s testable sync-side surface (`sync_status_text`,
//! and the `Sync1Proxy` it is built on) over a private test bus, the same
//! harness shape `tests/status.rs` uses for the account side.
//!
//! `register_root` needs a helper connection to mark the root (that is what
//! the helper is for), so — like `konedrived`'s own `tests/sync_dbus.rs` —
//! the harness here runs a fake helper thread that just acknowledges
//! everything, never a real fanotify group. `status` (see the first half of
//! the test below, before any root is registered) must also read sensibly
//! with no helper connected at all: on a machine without the helper it says
//! how to install or start it, and the developer's mode without
//! interception — a local folder whose files read as zeros until hydrated —
//! still works there.

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
use konedrived::state::SignInState;
use konedrived::sync::helper::HelperLink;
use konedrived::sync::SyncService;
use nix::sys::socket::{
    accept, bind, listen as sock_listen, socket, AddressFamily, Backlog, SockFlag, SockType, UnixAddr,
};

/// Polls `pred` every 20 ms for up to 5 s — the same shape
/// `konedrived`'s own tests use for a background listing to catch up.
async fn wait_for(mut pred: impl FnMut() -> bool) {
    for _ in 0..250 {
        if pred() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition was not met within 5s");
}

struct Harness {
    proxy: Sync1Proxy<'static>,
    dir: tempfile::TempDir,
    /// The daemon-side service itself, for the one test that has to take
    /// its helper away mid-run (`set_link(None)`), which nothing on the bus
    /// can do.
    service: Arc<SyncService>,
    /// The account service, so the token-export test can seed an access
    /// token directly rather than going through a real sign-in.
    account: Arc<AccountService>,
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
/// connects to it — the shape `register-without-interception`, the
/// developer's mode, exists for.
async fn harness_with_helper(with_helper: bool) -> Harness {
    build_harness(with_helper, false, false).await
}

/// As [`harness`], with a helper that refuses every `ClearIgnore` — how a
/// test makes startup recovery genuinely fail (see [`stuck_root`]).
async fn harness_refusing_clear_ignore() -> Harness {
    build_harness(true, false, true).await
}

/// As [`harness`], but signed in: the account's `StateHandle` is set to
/// `SignedIn` before the `SyncService` is made, and the sign-in gate is on
/// — the combination a folder registered while signed in needs,
/// which is what [`harness_onedrive`] builds on.
async fn build_harness_signed_in(with_helper: bool) -> Harness {
    build_harness_full(with_helper, true, false, true).await
}

/// As [`harness`], but with `RegisterRoot`'s sign-in gate wired to an
/// account nobody has signed in to — every other harness passes no account
/// at all, which `SyncService` treats as "nothing to check".
async fn harness_signed_out() -> Harness {
    build_harness(true, true, false).await
}

async fn build_harness(with_helper: bool, gate_on_sign_in: bool, refuse_clear_ignore: bool) -> Harness {
    build_harness_full(with_helper, gate_on_sign_in, refuse_clear_ignore, false).await
}

async fn build_harness_full(
    with_helper: bool,
    gate_on_sign_in: bool,
    refuse_clear_ignore: bool,
    signed_in: bool,
) -> Harness {
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
    if signed_in {
        account_service.state().update(|s| s.state = SignInState::SignedIn);
    }
    // A fresh account starts signed out, which is exactly what the one test
    // that asks for the gate needs.
    let account = gate_on_sign_in.then(|| account_service.state().clone());
    let sync_service = SyncService::new(link, account, None);
    // Without a helper, nothing is bound at this path: a punch with no link
    // goes ahead whatever this machine runs at the real one.
    sync_service.set_helper_socket(helper_dir.path().join("helper.sock"));

    let server =
        konedrived::dbus::serve(bus.builder(), Arc::clone(&account_service), None).await.unwrap();
    konedrived::sync::dbus::attach(&server, Arc::clone(&sync_service)).await.unwrap();

    let client = bus.connect().await;
    let proxy = Sync1Proxy::new(&client).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    Harness {
        proxy,
        dir,
        service: sync_service,
        account: account_service,
        _server: server,
        _account_dir: account_dir,
        _helper_dir: helper_dir,
        _bus: bus,
    }
}

/// A signed-in harness with a drive on a mocked Graph whose listing holds
/// one folder, one file inside it, and the Personal Vault (skipped) — the
/// shape `sync skipped`, `sync status`'s `Items:`/`Skipped:` lines, and
/// `sync refresh` all need a real listing to exercise.
async fn harness_onedrive() -> (Harness, wiremock::MockServer) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let graph = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me/drive"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "D1"})))
        .mount(&graph)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "value": [
                {"id": "R", "root": {}, "folder": {}},
                {"id": "D", "name": "docs", "folder": {}, "parentReference": {"id": "R"}},
                {"id": "F", "name": "f.txt", "size": 3, "cTag": "c1", "file": {}, "parentReference": {"id": "D"}},
                {"id": "V", "name": "Personal Vault", "folder": {}, "specialFolder": {"name": "vault"}, "parentReference": {"id": "R"}}
            ],
            "@odata.deltaLink": format!("{}/me/drive/root/delta?token=L1", graph.uri())
        })))
        .mount(&graph)
        .await;
    let f = build_harness_signed_in(true).await;
    let drive = konedrived::drive::DriveClient::new(
        url::Url::parse(&format!("{}/", graph.uri())).unwrap(),
        std::sync::Arc::new(konedrived::token::StaticToken::new("T")),
    )
    .unwrap();
    f.service.set_drive(drive);
    f.service.set_sync_paths(konedrived::sync::SyncPaths {
        tree_db: f.dir.path().join("tree.sqlite"),
        rescue_dir: f.dir.path().join("rescued"),
        thumbnails: Some(f.dir.path().join("thumbnails")),
    });
    (f, graph)
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

/// Marks `root_dir` as an already-registered root — exempts a
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
/// `busy` now, not a failure —)
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

/// `absolute_str` canonicalised every
/// path, so `sync register <symlink>` resolved the link and registered its
/// target — silently, while says a symbolic link is refused as a
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

/// `NoHelper` from `register`: the refusal a user without the helper meets
/// first, so it has to name the way forward — starting the helper, in the
/// words `HelperState` gives (HS4). The mode without interception is named
/// only as the developer's, with its cost: it never shows OneDrive (HS2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_register_without_a_helper_says_how_to_start_it() {
    let f = harness_with_helper(false).await;
    let addr = f._bus.address();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();

    let told = refused(addr, &["sync", "register", root.to_str().unwrap()]);
    assert!(told.contains("was not registered"), "{told}");
    assert!(told.contains("The konedrive helper is not connected"), "the helper's advice closes it: {told}");
    assert!(told.contains("developer's mode"), "{told}");
    assert!(told.contains("zeros"), "the developer's mode's cost is said with it: {told}");

    let status = out_text(&run(addr, &["sync", "status"]));
    let helper = status.lines().find(|l| l.starts_with("Helper:")).unwrap_or_else(|| panic!("{status}"));
    assert_eq!(helper, "Helper:                 unknown — the konedrive helper is not connected", "{status}");
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
// On a machine without the helper, `no-interception` is the state a folder
// registered there stays in — and its cost (a file
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

// --- `sync skipped`, `sync refresh`, and OneDrive-folder `sync status` ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_skipped_lists_what_is_not_in_the_folder_and_why() {
    let (f, _graph) = harness_onedrive().await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.register_root(root.to_str().unwrap()).await.unwrap();
    wait_for(|| root.join("docs/f.txt").is_file()).await;

    let out = run(f._bus.address(), &["sync", "skipped"]);
    assert!(out.status.success(), "{out:?}");
    let text = out_text(&out);
    assert!(text.contains(&root.join("Personal Vault").display().to_string()), "{text}");
    assert!(text.contains("locked separately"), "says why: {text}");
}

/// `sync skipped` with no folder registered at all: there is nothing to be
/// signed in about, and nothing OneDrive-related to say either — a plain
/// statement of the actual reason, not the empty "Nothing is skipped."
/// that would otherwise print (§4's "an unrecognised state shows no
/// emblem" kind of silent-looking success).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_skipped_with_no_folder_registered_says_so() {
    let f = harness().await;
    let out = run(f._bus.address(), &["sync", "skipped"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out_text(&out), "No folder is registered.\n");
}

/// `sync skipped` of a folder filled with `PopulateFrom` (a local folder,
/// `RootSource = local`): there is no OneDrive listing behind it, so
/// nothing is "skipped" in the sense this command means, and that has to be
/// said plainly rather than as an empty list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_skipped_of_a_local_folder_says_it_is_not_connected_to_onedrive() {
    let f = harness().await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.register_root(root.to_str().unwrap()).await.unwrap();

    let out = run(f._bus.address(), &["sync", "skipped"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out_text(&out), "This folder is not connected to OneDrive.\n");
}

/// `sync skipped` asked for while the initial listing is still running: the
/// list `Skipped()` can return at that moment is not wrong, only
/// incomplete — pages not listed yet have not reported what they skip — so
/// the output has to say that rather than let the (possibly empty) list
/// read as final. The delta response is delayed well past the time the
/// subprocess needs to start and call `sync skipped`, so `RootState` is
/// still `listing` for the whole call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_skipped_of_a_onedrive_folder_still_listing_says_the_list_may_be_partial() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let graph = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me/drive"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "D1"})))
        .mount(&graph)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(2))
                .set_body_json(serde_json::json!({
                    "value": [{"id": "R", "root": {}, "folder": {}}],
                    "@odata.deltaLink": format!("{}/me/drive/root/delta?token=L1", graph.uri())
                })),
        )
        .mount(&graph)
        .await;
    let f = build_harness_signed_in(true).await;
    let drive = konedrived::drive::DriveClient::new(
        url::Url::parse(&format!("{}/", graph.uri())).unwrap(),
        std::sync::Arc::new(konedrived::token::StaticToken::new("T")),
    )
    .unwrap();
    f.service.set_drive(drive);
    f.service.set_sync_paths(konedrived::sync::SyncPaths {
        tree_db: f.dir.path().join("tree.sqlite"),
        rescue_dir: f.dir.path().join("rescued"),
        thumbnails: Some(f.dir.path().join("thumbnails")),
    });
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.register_root(root.to_str().unwrap()).await.unwrap();
    for _ in 0..250 {
        if f.proxy.root_state().await.unwrap() == "listing" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(f.proxy.root_state().await.unwrap(), "listing", "the delay must still be in effect");

    let out = run(f._bus.address(), &["sync", "skipped"]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).contains("may be partial"), "{}", out_text(&out));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_status_of_a_onedrive_folder_counts_its_items_and_says_it_is_read_only() {
    let (f, _graph) = harness_onedrive().await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.register_root(root.to_str().unwrap()).await.unwrap();
    wait_for(|| root.join("docs/f.txt").is_file()).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await; // counters are coalesced

    let text = out_text(&run(f._bus.address(), &["sync", "status"]));
    assert!(
        text.lines().any(|l| l.starts_with("Items:") && l.contains("3 in OneDrive") && l.contains("2 in the folder")),
        "{text}"
    );
    assert!(
        text.lines().any(|l| l == "Skipped:                1 (see `konedrivectl sync skipped`)"),
        "{text}"
    );
    assert!(text.lines().any(|l| l.starts_with("Editing:") && l.contains("read-only")), "{text}");
}

// --- Activity, transfers, conflicts, free-up, status lines -----

/// The binary, with `TZ` set so the times it prints are UTC.
fn run_utc(bus_addr: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_konedrivectl"))
        .args(args)
        .env("DBUS_SESSION_BUS_ADDRESS", bus_addr)
        .env("TZ", "UTC")
        .output()
        .expect("failed to run the konedrivectl binary")
}

/// A folder registered without interception, with no helper anywhere, and
/// `names` in it downloaded, 64 KiB each.
async fn downloaded_files(f: &Harness, names: &[&str]) -> PathBuf {
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    let source = f.dir.path().join("source");
    std::fs::create_dir(&source).unwrap();
    for name in names {
        std::fs::write(source.join(name), vec![6u8; 64 * 1024]).unwrap();
    }
    f.proxy.register_root_without_interception(root.to_str().unwrap()).await.unwrap();
    f.proxy.populate_from_directory(source.to_str().unwrap()).await.unwrap();
    for name in names {
        f.proxy.hydrate(root.join(name).to_str().unwrap()).await.unwrap();
    }
    root
}

/// `sync activity`: time, kind, path and detail, newest first, at most
/// `--limit` of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_activity_lists_what_happened_newest_first() {
    let f = harness_with_helper(false).await;
    let addr = f._bus.address();
    let out = run_utc(addr, &["sync", "activity"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out_text(&out).trim(), "Nothing has happened yet.");

    let root = downloaded_files(&f, &["a.bin"]).await;
    let file = root.join("a.bin");
    f.proxy.dehydrate(file.to_str().unwrap()).await.unwrap();

    let out = run_utc(addr, &["sync", "activity", "--limit", "5"]);
    assert!(out.status.success(), "{out:?}");
    let text = out_text(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[0].contains("freed") && lines[0].contains(file.to_str().unwrap()), "newest first: {text}");
    assert!(lines[1].contains("downloaded") && lines[1].contains("64.0 KiB"), "{text}");
    let time = lines[1].split("  ").next().unwrap();
    assert_eq!(time.len(), "2026-09-24 10:00:00".len(), "a time first: {text}");

    let out = run_utc(addr, &["sync", "activity", "--limit", "1"]);
    assert_eq!(out_text(&out).lines().count(), 1, "{}", out_text(&out));
}

/// `sync transfers`: each download under way with how far it has got, or
/// that there is none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_transfers_lists_the_downloads_under_way() {
    let f = harness().await;
    let addr = f._bus.address();
    let out = run(addr, &["sync", "transfers"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out_text(&out).trim(), "Nothing is downloading.");

    let entry = f.service.report().transfers.start("/home/u/OneDrive/big.bin".into(), 4 << 20);
    entry.progress(1 << 20, 4 << 20);
    let out = run(addr, &["sync", "transfers"]);
    let text = out_text(&out);
    assert!(out.status.success(), "{out:?}");
    assert!(text.contains("/home/u/OneDrive/big.bin") && text.contains("25%") && text.contains("4.0 MiB"), "{text}");
    drop(entry);
    assert_eq!(out_text(&run(addr, &["sync", "transfers"])).trim(), "Nothing is downloading.");
}

/// `sync conflicts` lists each local version moved out of the way — where
/// it was, where it is, when — and `sync dismiss` takes one off the list,
/// leaving the file; a path that is not a conflict is refused by name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_conflicts_are_listed_and_dismissed() {
    let f = harness().await;
    let addr = f._bus.address();
    assert_eq!(out_text(&run_utc(addr, &["sync", "conflicts"])).trim(), "No conflicts.");

    let rescued = f.dir.path().join("rescued/2023-11-14T22-13-20Z/docs/f.txt");
    std::fs::create_dir_all(rescued.parent().unwrap()).unwrap();
    std::fs::write(&rescued, b"mine").unwrap();
    let activity = &f.service.report().activity;
    activity.attach(konedrived::tree::Store::new(konedrived::tree::TreeStore::in_memory().unwrap()), f.dir.path());
    activity.add_conflicts(vec![konedrived::tree::ConflictRow {
        at: 1_700_000_000,
        original: "/home/u/OneDrive/docs/f.txt".into(),
        rescued: rescued.display().to_string(),
    }]);

    let status = out_text(&run(addr, &["sync", "status"]));
    assert!(
        status.lines().any(|l| l == "Conflicts:              1 (see `konedrivectl sync conflicts`)"),
        "{status}"
    );
    let text = out_text(&run_utc(addr, &["sync", "conflicts"]));
    assert!(text.contains("/home/u/OneDrive/docs/f.txt"), "{text}");
    assert!(text.contains(rescued.to_str().unwrap()), "{text}");
    assert!(text.contains("2023-11-14 22:13:20"), "{text}");

    let out = run(addr, &["sync", "dismiss", rescued.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).contains("Dismissed"), "{out:?}");
    assert!(rescued.exists(), "the file itself is left where it is");
    assert_eq!(out_text(&run(addr, &["sync", "conflicts"])).trim(), "No conflicts.");

    let text = refused(addr, &["sync", "dismiss", "/nowhere/f.txt"]);
    assert!(text.contains("/nowhere/f.txt"), "{text}");
}

/// `sync free-up-space`: what it freed, and what it kept because it was in
/// use.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_free_up_space_says_what_it_freed_and_what_was_in_use() {
    let f = harness_with_helper(false).await;
    let root = downloaded_files(&f, &["a.bin", "b.bin"]).await;
    use std::os::unix::fs::MetadataExt;
    let freed = std::fs::metadata(root.join("a.bin")).unwrap().blocks() * 512;
    let _in_use = std::fs::File::open(root.join("b.bin")).unwrap();

    let out = run(f._bus.address(), &["sync", "free-up-space"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        out_text(&out).trim(),
        format!("Freed 1 file ({}). 1 file was in use and kept.", konedrivectl::human_bytes(freed))
    );
}

/// `sync pin` keeps a folder on this device, and everything in it
/// downloads; `sync status` counts it; `sync free` of a file the folder keeps
/// is refused, naming the folder; `sync free` of the folder stops keeping it
/// and frees up what is in it.
/// A folder registered without interception, with no helper, holding
/// `docs/a.bin` and `docs/b.bin`, 64 KiB each, neither downloaded: the paths
/// of `docs`, `a.bin` and `b.bin`.
async fn docs_to_pin(f: &Harness) -> (PathBuf, PathBuf, PathBuf) {
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let source = f.dir.path().join("source");
    std::fs::create_dir_all(source.join("docs")).unwrap();
    for name in ["docs/a.bin", "docs/b.bin"] {
        std::fs::write(source.join(name), vec![4u8; 64 * 1024]).unwrap();
    }
    f.proxy.register_root_without_interception(root.to_str().unwrap()).await.unwrap();
    f.proxy.populate_from_directory(source.to_str().unwrap()).await.unwrap();
    (root.join("docs"), root.join("docs/a.bin"), root.join("docs/b.bin"))
}

/// A file's `user.konedrive.state`, read by name.
fn state(path: &PathBuf) -> Vec<u8> {
    xattr::get(path, "user.konedrive.state").unwrap().unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_pin_keeps_a_folder_here_and_free_lets_it_go() {
    let f = harness_with_helper(false).await;
    let addr = f._bus.address();
    let (docs, a, b) = docs_to_pin(&f).await;

    let out = run(addr, &["sync", "pin", docs.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out_text(&out).trim(), "Kept on this device. 2 files are downloading (`konedrivectl sync transfers`).");
    wait_for(|| state(&a) == b"hydrated" && state(&b) == b"hydrated").await;
    let status = out_text(&run(addr, &["sync", "status"]));
    assert!(status.lines().any(|l| l == "Always on this device:  1"), "{status}");

    let told = refused(addr, &["sync", "free", a.to_str().unwrap()]);
    assert!(told.contains(&format!("because the folder {} is", docs.display())), "{told}");
    assert_eq!(state(&a), b"hydrated");

    let out = run(addr, &["sync", "free", docs.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).starts_with("Freed 2 files ("), "{}", out_text(&out));
    assert_eq!((state(&a), state(&b)), (b"online-only".to_vec(), b"online-only".to_vec()));
    let status = out_text(&run(addr, &["sync", "status"]));
    assert!(status.lines().any(|l| l == "Always on this device:  0"), "{status}");
}

/// `sync unpin` takes a folder's pin off and leaves its files downloaded.
/// Asked of files the folder keeps, it is refused, naming the first such
/// file alone and the folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_unpin_stops_keeping_a_folder_and_leaves_its_files() {
    let f = harness_with_helper(false).await;
    let addr = f._bus.address();
    let (docs, a, b) = docs_to_pin(&f).await;
    f.proxy.pin(&[docs.to_str().unwrap()]).await.unwrap();
    wait_for(|| state(&a) == b"hydrated" && state(&b) == b"hydrated").await;

    let told = refused(addr, &["sync", "unpin", b.to_str().unwrap(), a.to_str().unwrap()]);
    let expected = format!("{} is kept on this device because the folder {} is", b.display(), docs.display());
    assert!(told.contains(&expected), "{told}");
    assert!(told.contains(&format!("`konedrivectl sync unpin {}`", docs.display())), "{told}");

    let out = run(addr, &["sync", "unpin", docs.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).starts_with("No longer kept on this device."), "{}", out_text(&out));
    assert_eq!(xattr::get(&docs, "user.konedrive.pin").unwrap(), None);
    assert_eq!((state(&a), state(&b)), (b"hydrated".to_vec(), b"hydrated".to_vec()), "the files stay");
    assert_eq!(f.proxy.pinned_count().await.unwrap(), 0);
}

/// `sync status` says when the folder was last checked with OneDrive, and
/// how much of this computer's disk it takes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_status_says_when_it_last_checked_and_what_the_folder_takes() {
    let (f, _graph) = harness_onedrive().await;
    let addr = f._bus.address();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.register_root(root.to_str().unwrap()).await.unwrap();
    wait_for(|| f.service.status().0 > 0).await;

    let text = out_text(&run(addr, &["sync", "status"]));
    let checked = text.lines().find(|l| l.starts_with("Last checked:")).unwrap_or_else(|| panic!("{text}"));
    assert!(checked.ends_with(" s ago"), "{text}");
    let space = text.lines().find(|l| l.starts_with("On this computer:")).unwrap_or_else(|| panic!("{text}"));
    assert!(space.ends_with(" B") || space.ends_with("iB"), "{text}");
}

/// A folder that shows OneDrive but was never checked says so, rather than
/// a time.
#[test]
fn a_folder_never_checked_reads_never() {
    assert_eq!(konedrivectl::checked_text(0, 1_000), "never");
    assert_eq!(konedrivectl::checked_text(980, 1_000), "20 s ago");
    assert_eq!(konedrivectl::checked_text(1_000 - 5 * 60, 1_000), "5 min ago");
    assert_eq!(konedrivectl::checked_text(100_000 - 3 * 3600, 100_000), "3 h ago");
    assert_eq!(konedrivectl::checked_text(1_000_000 - 2 * 86_400, 1_000_000), "2 d ago");
    assert_eq!(konedrivectl::checked_text(1_010, 1_000), "just now", "a clock that went back");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_refresh_asks_onedrive_now() {
    let (f, graph) = harness_onedrive().await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.register_root(root.to_str().unwrap()).await.unwrap();
    wait_for(|| root.join("docs/f.txt").is_file()).await;
    let before = graph.received_requests().await.unwrap().len();

    let out = run(f._bus.address(), &["sync", "refresh"]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).contains("Asked OneDrive for changes"), "{out:?}");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(graph.received_requests().await.unwrap().len() > before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_refresh_of_a_local_folder_says_it_is_not_connected_to_onedrive() {
    let f = harness().await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.register_root(root.to_str().unwrap()).await.unwrap();

    let text = refused(f._bus.address(), &["sync", "refresh"]);
    assert!(text.contains("not connected to OneDrive"), "{text}");
}

// --- `dev export-access-token` --------------------------------------------

/// I1: an existing file at `--out` is replaced by a new inode, not
/// truncated in place. An fd opened before the export — the shape the
/// used to reproduce the bug — proves it: it must keep reading the
/// *old* content, byte for byte, forever, because `rename(2)` never touches
/// the inode a still-open fd already holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_exports_the_access_token_and_nothing_else_readable_only_by_the_user() {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    let f = harness().await;
    f.account
        .tokens()
        .seed(&konedrived::oauth::TokenResponse {
            access_token: "AT-EXPORT".into(),
            expires_in: 3600,
            refresh_token: Some("RT-NEVER".into()),
        })
        .await;
    let out_file = f.dir.path().join("token");
    std::fs::write(&out_file, b"old").unwrap();
    std::fs::set_permissions(&out_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut held_open = std::fs::File::open(&out_file).unwrap();

    let out = run(f._bus.address(), &["dev", "export-access-token", "--out", out_file.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(std::fs::read_to_string(&out_file).unwrap(), "AT-EXPORT");
    assert_eq!(std::fs::metadata(&out_file).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(!out_text(&out).contains("AT-EXPORT"), "the token must never reach stdout: {}", out_text(&out));
    assert!(!err_text(&out).contains("AT-EXPORT"), "the token must never reach stderr: {}", err_text(&out));
    let mut still_reads = String::new();
    held_open.read_to_string(&mut still_reads).unwrap();
    assert_eq!(still_reads, "old", "an fd opened before the export must keep reading the old inode");
}

/// I1: `--out` naming a symlink — the 's exact reproduction — must
/// have the link itself replaced by `rename(2)`, never the file it points
/// to opened and truncated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_export_access_token_replaces_a_symlink_without_touching_its_target() {
    use std::os::unix::fs::PermissionsExt;
    let f = harness().await;
    f.account
        .tokens()
        .seed(&konedrived::oauth::TokenResponse {
            access_token: "AT-EXPORT".into(),
            expires_in: 3600,
            refresh_token: Some("RT-NEVER".into()),
        })
        .await;
    let target = f.dir.path().join("someone-elses-file");
    std::fs::write(&target, b"do not touch").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
    let link = f.dir.path().join("token-link");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let out = run(f._bus.address(), &["dev", "export-access-token", "--out", link.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        !std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(),
        "the link must be replaced by a regular file, not written through"
    );
    assert_eq!(std::fs::read_to_string(&link).unwrap(), "AT-EXPORT");
    assert_eq!(std::fs::metadata(&link).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "do not touch", "the old target must be untouched");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_export_access_token_refused_while_signed_out_names_the_reason() {
    let f = harness().await;
    let out_file = f.dir.path().join("token");

    let out = run(f._bus.address(), &["dev", "export-access-token", "--out", out_file.to_str().unwrap()]);
    assert!(!out.status.success(), "{out:?}");
    assert!(!out_file.exists(), "nothing must be written on a refusal: {out:?}");
    assert!(err_text(&out).to_lowercase().contains("signed in"), "{}", err_text(&out));
}

// --- The skip-reason wording is one sentence, shared with the window -----

/// Parses `whyText`'s `if (reason == QLatin1String("<reason>")) { return
/// i18n("<sentence>"); }` branches out of `app/synccontroller.cpp`'s source,
/// in order, as `(reason, sentence)` pairs — plus the function's final,
/// unconditional `return i18n("<sentence>");` (the fallback for anything not
/// named above) as the pair `("unsupported", <that sentence>)`, matching the
/// name `skip_reason_text`'s own fallback answers to.
fn parse_why_text_branches(cpp: &str) -> Vec<(String, String)> {
    let start = cpp.find("QString whyText").expect("whyText(...) not found in synccontroller.cpp");
    let end = start + cpp[start..].find("\n}\n").expect("no closing brace found for whyText");
    let mut rest = &cpp[start..end];
    let mut pairs = Vec::new();
    while let Some(reason_at) = rest.find("QLatin1String(\"") {
        let after_reason_open = &rest[reason_at + "QLatin1String(\"".len()..];
        let reason_end = after_reason_open.find('"').expect("unterminated QLatin1String");
        let reason = after_reason_open[..reason_end].to_owned();

        let after_reason = &after_reason_open[reason_end..];
        let sentence_at = after_reason.find("i18n(\"").expect("no i18n(...) after this QLatin1String");
        let after_sentence_open = &after_reason[sentence_at + "i18n(\"".len()..];
        let sentence_end = after_sentence_open.find("\");").expect("unterminated i18n(...)");
        let sentence = after_sentence_open[..sentence_end].to_owned();

        pairs.push((reason, sentence));
        rest = &after_sentence_open[sentence_end..];
    }
    // What is left is the tail after the last named branch: the function's
    // final, unconditional return — the fallback.
    let fallback_at = rest.find("i18n(\"").expect("no fallback return i18n(...) after the named branches");
    let after_fallback_open = &rest[fallback_at + "i18n(\"".len()..];
    let fallback_end = after_fallback_open.find("\");").expect("unterminated fallback i18n(...)");
    pairs.push(("unsupported".to_owned(), after_fallback_open[..fallback_end].to_owned()));
    pairs
}

/// `konedrivectl::skip_reason_text` and the window's `whyText`
/// (`app/synccontroller.cpp`) are meant to say exactly the same thing for
/// each reason (see `crates/konedrivectl/src/lib.rs`'s doc comment on
/// `skip_reason_text`), so a person reading `konedrivectl sync skipped` and
/// a person reading the window see one explanation, not two that happen to
/// agree today. Checking only "does the Rust sentence appear somewhere in
/// the C++ file" (the guard's first cut) would still pass if two branches'
/// bodies were swapped — every sentence would still be *present*, just
/// answering the wrong reason. Parsing each branch's own (reason, sentence)
/// pair out of the C++ source and comparing it against
/// `skip_reason_text(reason)` directly closes that gap: it fails if a
/// reason's C++ sentence and its Rust sentence disagree, in either
/// direction, including a swap between two reasons that both still have
/// *a* sentence, just not the *right* one.
#[test]
fn skip_reason_text_matches_every_branch_of_the_windows_whytext() {
    let cpp = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../app/synccontroller.cpp"
    ))
    .unwrap();
    let branches = parse_why_text_branches(&cpp);
    assert_eq!(
        branches.iter().map(|(reason, _)| reason.as_str()).collect::<Vec<_>>(),
        vec!["name-too-long", "personal-vault", "shared", "onenote", "reserved-name", "unsupported"],
        "whyText's branches changed shape; update this parser or the reason list"
    );
    for (reason, sentence) in &branches {
        assert_eq!(
            konedrivectl::skip_reason_text(reason),
            sentence,
            "app/synccontroller.cpp's whyText(\"{reason}\") and \
             konedrivectl::skip_reason_text(\"{reason}\") must say exactly the same thing"
        );
    }
}

// --- The activity's words are one contract with the window --------------

/// One of the window's source files: from `app/`, or from the directory
/// `KONEDRIVE_APP_SOURCE` names — how the guard below is shown to fail on a
/// changed copy without touching `app/` itself.
fn app_source(name: &str) -> String {
    let dir = std::env::var("KONEDRIVE_APP_SOURCE")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../app").to_owned());
    std::fs::read_to_string(format!("{dir}/{name}")).unwrap_or_else(|e| panic!("{dir}/{name}: {e}"))
}

/// Every activity kind the window branches on: each `kind ==
/// QLatin1String("…")` in `app/activitymodel.cpp`, once.
fn kinds_the_window_branches_on(cpp: &str) -> Vec<String> {
    const MARK: &str = "kind == QLatin1String(\"";
    let mut kinds = Vec::new();
    let mut rest = cpp;
    while let Some(at) = rest.find(MARK) {
        let after = &rest[at + MARK.len()..];
        let end = after.find('"').expect("unterminated QLatin1String");
        kinds.push(after[..end].to_owned());
        rest = &after[end..];
    }
    kinds.sort();
    kinds.dedup();
    kinds
}

/// The window turns the daemon's events into
/// notifications by their kind and by one exact detail (A2 in the
/// limitations log), and nothing else ties the two sides together. Every
/// kind `app/activitymodel.cpp` branches on must be one the daemon sends
/// (`Kind::as_str`), the ones its notifications hang on must be among them,
/// and "not enough disk space" must be `activity::NO_DISK_SPACE` word for
/// word — so a rename on either side fails here, not in a user's tray.
#[test]
fn the_window_branches_on_the_daemons_own_activity_words() {
    use konedrived::sync::activity::{Kind, NO_DISK_SPACE};
    let cpp = app_source("activitymodel.cpp");
    let sent: Vec<&str> = Kind::ALL.iter().map(|kind| kind.as_str()).collect();
    let window = kinds_the_window_branches_on(&cpp);
    for kind in &window {
        assert!(sent.contains(&kind.as_str()), "the window branches on {kind:?}, which the daemon never sends ({sent:?})");
    }
    for kind in [Kind::UpdateFailed, Kind::Failed, Kind::Conflict] {
        assert!(
            window.iter().any(|k| k == kind.as_str()),
            "the window no longer branches on {:?}, which the daemon sends: {window:?}",
            kind.as_str()
        );
    }
    assert!(
        cpp.contains(&format!("QLatin1String(\"{NO_DISK_SPACE}\")")),
        "the window does not recognise the daemon's words for a full disk, {NO_DISK_SPACE:?}"
    );
}
