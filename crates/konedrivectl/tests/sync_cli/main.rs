//! Exercises `konedrivectl`'s sync side — the binary's `sync` and `dev`
//! commands — against the daemon over a private test bus,
//! with one account, `Personal`: the case where no command needs `--account`.
//! The daemon is started as `konedrived` starts it (`tests/common`); the
//! harness reaches into the account's services only for what nothing on the
//! bus can do (taking the helper away, seeding a token, a mocked drive).
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

#[path = "../common/mod.rs"]
mod common;
mod activity;
mod refusals;
mod registration;
mod settings;
mod status;
#[cfg(feature = "dev-tools")]
mod token_export;
mod wording;

use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::{err_text, out_text, run};
use konedrive_dbus::accounts::{FilesProxy, FolderProxies};
use konedrive_dbus::testing::TestBus;
use konedrive_fs::placeholder::{write_state, State};
use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use konedrived::account::AccountService;
use konedrived::account::state::SignInState;
use konedrived::helper::HelperLink;
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
    /// The account's folder.
    proxy: FolderProxies<'static>,
    /// `Files`: the calls on a path.
    files: FilesProxy<'static>,
    dir: tempfile::TempDir,
    /// The daemon-side service itself, for the one test that has to take
    /// its helper away mid-run (`set_link(None)`), which nothing on the bus
    /// can do.
    service: Arc<SyncService>,
    /// The account service, so the token-export test can seed an access
    /// token directly rather than going through a real sign-in.
    account: Arc<AccountService>,
    _daemon: konedrived::daemon::startup::Daemon,
    _config_dir: tempfile::TempDir,
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

/// The daemon with one account, `Personal`, signed in, and the fake helper
/// connected.
async fn harness() -> Harness {
    harness_with_helper(true).await
}

/// As [`harness`], but `with_helper: false` never starts the fake helper or
/// connects to it — the shape `register-without-interception`, the
/// developer's mode, exists for.
async fn harness_with_helper(with_helper: bool) -> Harness {
    build_harness(with_helper, true, false).await
}

/// As [`harness`], with a helper that refuses every `ClearIgnore` — how a
/// test makes startup recovery genuinely fail (see [`stuck_root`]).
async fn harness_refusing_clear_ignore() -> Harness {
    build_harness(true, true, true).await
}

/// As [`harness`], but nobody has signed in to the account: `RegisterRoot`
/// is refused for that.
async fn harness_signed_out() -> Harness {
    build_harness(true, false, false).await
}

/// The account is marked signed in (its `StateHandle` set to `SignedIn`, with
/// no token behind it) unless `signed_in` is false: `RegisterRoot` binds a
/// folder only to a signed-in account. The daemon's folders are local, so it
/// shows OneDrive only where a test gives it a drive ([`harness_onedrive`]).
async fn build_harness(with_helper: bool, signed_in: bool, refuse_clear_ignore: bool) -> Harness {
    build_harness_showing(with_helper, signed_in, refuse_clear_ignore, konedrived::daemon::manager::no_drive()).await
}

/// [`build_harness`], whose account's folder shows what `drive` gives.
async fn build_harness_showing(
    with_helper: bool,
    signed_in: bool,
    refuse_clear_ignore: bool,
    drive: konedrived::daemon::manager::DriveOf,
) -> Harness {
    let bus = TestBus::start();
    let config_dir = tempfile::tempdir().unwrap();
    let daemon = common::start_daemon_showing(&bus, config_dir.path(), drive).await;

    let helper_dir = tempfile::tempdir().unwrap();
    let hub = daemon.manager.hub();
    // Without a helper, nothing is bound at this path: a punch with no link
    // goes ahead whatever this machine runs at the real one.
    hub.set_socket(helper_dir.path().join("helper.sock"));
    if with_helper {
        let socket_path = helper_dir.path().join("helper.sock");
        fake_helper(socket_path.clone(), refuse_clear_ignore);
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        hub.set_link(Some(link));
    }
    let account = daemon.manager.add("Personal", &daemon.connection).await.unwrap();
    if signed_in {
        account.account.state().update(|s| s.state = SignInState::SignedIn);
    }

    let client = bus.connect().await;
    let proxy = FolderProxies::new(&client, account.path.clone()).await.unwrap();
    let files = FilesProxy::new(&client).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    Harness {
        proxy,
        files,
        dir,
        service: Arc::clone(&account.sync),
        account: Arc::clone(&account.account),
        _daemon: daemon,
        _config_dir: config_dir,
        _helper_dir: helper_dir,
        _bus: bus,
    }
}

/// A signed-in harness, with the fake helper connected, whose account's folder shows the
/// drive of the mocked Graph at `graph`.
async fn harness_showing(graph: &wiremock::MockServer) -> Harness {
    let drive = konedrive_graph::drive::DriveClient::new(
        url::Url::parse(&format!("{}/", graph.uri())).unwrap(),
        std::sync::Arc::new(konedrive_graph::token::StaticToken::new("T")),
    )
    .unwrap();
    build_harness_showing(true, true, false, Arc::new(move |_| Ok(Some(drive.clone())))).await
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
    let f = harness_showing(&graph).await;
    (f, graph)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_status_reports_no_folder_then_the_registered_one() {
    let f = harness().await;
    let addr = f._bus.address();

    let text = out_text(&run(addr, &["sync", "status"]));
    assert!(text.lines().any(|l| l == "Folder:                 (none)"), "{text}");

    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();

    let text = out_text(&run(addr, &["sync", "status"]));
    assert!(text.contains(root.to_str().unwrap()), "{text}");
    assert!(text.lines().any(|l| l == "State:                  ready"), "{text}");
}

/// Marks `root_dir` as an already-registered root and leaves one file in it `dehydrating`. A
/// folder that carries `user.konedrive.root` is exempt from the "must be empty" check, so the
/// file can be planted before the registration, whose recovery then finds it. Registered
/// through [`harness_refusing_clear_ignore`], whose helper refuses the `ClearIgnore` recovery
/// must have before it may punch that file, recovery fails for it: the registration still
/// answers `Ok`, and the folder needs attention.
fn stuck_root(root_dir: &std::path::Path) -> PathBuf {
    xattr::set(root_dir, "user.konedrive.root", b"1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d").unwrap();
    let path = root_dir.join("stuck.bin");
    std::fs::write(&path, vec![1u8; 4096]).unwrap();
    let file = std::fs::File::options().read(true).write(true).open(&path).unwrap();
    write_state(&file, State::Dehydrating).unwrap();
    path
}

/// A folder whose recovery left a file behind reads `error` in `sync status`, with the
/// detail: the fake helper of the other tests acknowledges everything, so recovery never
/// fails there; `stuck_root`, with a helper that refuses `ClearIgnore`, makes it fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_status_shows_a_recovery_failure_as_error_not_a_success() {
    let f = harness_refusing_clear_ignore().await;
    let root = f.dir.path().join("Stuck");
    std::fs::create_dir(&root).unwrap();
    let _stuck_path = stuck_root(&root);

    // A file recovery could not fix does not fail the registration.
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();

    let text = out_text(&run(f._bus.address(), &["sync", "status"]));
    assert!(text.lines().any(|l| l == "State:                  error"), "the state must read error: {text}");
    let detail = text.lines().find(|l| l.starts_with("Last error:")).unwrap_or_else(|| panic!("the detail must be shown: {text}"));
    assert!(detail.contains('1'), "the failure count belongs in the detail, not just the state: {text}");
}

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
