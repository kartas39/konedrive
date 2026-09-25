//! The multiple-accounts daemon over a private test bus (design §10, tests 7–11): the
//! manager at `/org/konedrive/Accounts` — `Accounts1`, `Files1` and the `ObjectManager` —
//! and the accounts below it, started as `main` starts them (`accounts::start`). A fake
//! helper speaks the wire protocol; nothing is intercepted for real.

mod common;

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use futures_util::StreamExt;
use konedrive_dbus::accounts::{Account1Proxy, Accounts1Proxy, Files1Proxy, Sync1Proxy};
use konedrive_dbus::testing::TestBus;
use konedrive_dbus::{error_name, ACCOUNTS_INTERFACE_NAME, ACCOUNTS_PATH, ACCOUNT_INTERFACE_NAME, FILES_INTERFACE_NAME};
use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use konedrived::config::Paths;
use konedrived::oauth::Endpoints;
use konedrived::secret::{MemoryWallet, Slot, Wallet};
use konedrived::state::SignInState;
use konedrived::sync::helper::HelperLink;
use nix::sys::socket::{accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, UnixAddr};
use wiremock::MockServer;
use zbus::zvariant::OwnedObjectPath;

const ACCOUNTS_XML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dbus/org.konedrive.Accounts1.xml"));
const FILES_XML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dbus/org.konedrive.Files1.xml"));

/// A stand-in helper: greets, acknowledges `Hello` and every request after it, records
/// what it was asked, and sends hydration requests on the live connection when told to.
/// It accepts connection after connection.
struct FakeHelper {
    seen: Arc<Mutex<Vec<&'static str>>>,
    answered: Arc<Mutex<HashMap<u64, i32>>>,
    live: Arc<Mutex<Option<UnixStream>>>,
}

impl FakeHelper {
    fn start(path: &Path) -> Self {
        let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None).unwrap();
        bind(fd.as_raw_fd(), &UnixAddr::new(path).unwrap()).unwrap();
        listen(&fd, Backlog::new(16).unwrap()).unwrap();
        let helper = Self { seen: Arc::default(), answered: Arc::default(), live: Arc::default() };
        let (seen, answered, live) = (Arc::clone(&helper.seen), Arc::clone(&helper.answered), Arc::clone(&helper.live));
        std::thread::spawn(move || {
            let listener: OwnedFd = fd;
            while let Ok(accepted) = accept(listener.as_raw_fd()) {
                // SAFETY: `accept` just returned a descriptor this process now solely owns.
                let stream = unsafe { UnixStream::from_raw_fd(accepted) };
                *live.lock().unwrap() = stream.try_clone().ok();
                let Ok(mut channel) = Channel::new(stream) else { continue };
                if channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).is_err() {
                    continue;
                }
                while let Ok((message, _fd)) = channel.recv::<ToHelper>() {
                    match message {
                        ToHelper::RegisterRoot { .. } => seen.lock().unwrap().push("RegisterRoot"),
                        ToHelper::UnregisterRoot { .. } => seen.lock().unwrap().push("UnregisterRoot"),
                        ToHelper::HydrateDone { req_id, errno } => {
                            answered.lock().unwrap().insert(req_id, errno);
                        }
                        _ => {}
                    }
                    if channel.send(&ToDaemon::Ack { errno: 0 }, None).is_err() {
                        break;
                    }
                }
            }
        });
        helper
    }

    fn seen(&self) -> Vec<&'static str> {
        self.seen.lock().unwrap().clone()
    }

    /// Sends the daemon a hydration request for `fd`, as the helper does for an
    /// intercepted open. A second `Channel` on the same socket is safe: every send is one
    /// datagram.
    fn send_request(&self, req_id: u64, fd: &OwnedFd) {
        let live = self.live.lock().unwrap();
        let stream = live.as_ref().expect("a live connection").try_clone().unwrap();
        Channel::new(stream).unwrap().send(&ToDaemon::HydrateRequest { req_id }, Some(fd.as_fd())).unwrap();
    }

    /// The errno the daemon answered request `req_id` with.
    async fn answer(&self, req_id: u64) -> i32 {
        for _ in 0..500 {
            if let Some(errno) = self.answered.lock().unwrap().get(&req_id) {
                return *errno;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("request {req_id} was never answered");
    }
}

/// The daemon, with no account yet, and a client of it.
struct Daemon {
    daemon: konedrived::accounts::Daemon,
    client: zbus::Connection,
    manager: Accounts1Proxy<'static>,
    files: Files1Proxy<'static>,
    wallet: Arc<MemoryWallet>,
    /// `config.toml` and the accounts' files.
    config: tempfile::TempDir,
    /// Folders, sources, and the helper's socket.
    dir: tempfile::TempDir,
    _bus: TestBus,
}

impl Daemon {
    async fn start() -> Self {
        Self::start_in(tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()).await
    }

    /// The daemon on the `config.toml` a test prepared in `config`, with folders in `dir`.
    async fn start_in(config: tempfile::TempDir, dir: tempfile::TempDir) -> Self {
        let bus = TestBus::start();
        let wallet = Arc::new(MemoryWallet::default());
        let daemon =
            start_daemon(&bus, config.path(), Endpoints::microsoft(), Arc::clone(&wallet), Duration::from_secs(5)).await;
        let client = bus.connect().await;
        let manager =
            Accounts1Proxy::builder(&client).cache_properties(zbus::proxy::CacheProperties::No).build().await.unwrap();
        let files = Files1Proxy::new(&client).await.unwrap();
        Self { daemon, client, manager, files, wallet, config, dir, _bus: bus }
    }

    async fn sync(&self, account: &OwnedObjectPath) -> Sync1Proxy<'static> {
        Sync1Proxy::builder(&self.client)
            .path(account.clone())
            .unwrap()
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .unwrap()
    }

    /// The daemon's own half of an account, for what no method reaches.
    fn account(&self, path: &OwnedObjectPath) -> Arc<konedrived::accounts::Account> {
        self.daemon.manager.account(&path.as_ref()).unwrap()
    }

    /// A fake helper at a socket of the test's own, and the hub linked to it by hand.
    async fn connect_helper(&self) -> (FakeHelper, PathBuf) {
        let socket = self.dir.path().join("helper.sock");
        let helper = FakeHelper::start(&socket);
        self.link(&socket).await;
        (helper, socket)
    }

    /// A fake helper, and the hub's supervisor connected to it — which serves hydration
    /// requests, routed to their accounts, as the daemon does.
    async fn supervise_helper(&self) -> FakeHelper {
        let socket = self.dir.path().join("helper.sock");
        let helper = FakeHelper::start(&socket);
        let hub = Arc::clone(self.daemon.manager.hub());
        tokio::spawn(konedrived::sync::hub::supervise(Arc::clone(&hub), socket, Duration::from_millis(50)));
        eventually("the supervisor connected", || {
            let hub = Arc::clone(&hub);
            async move { hub.link().is_some() }
        })
        .await;
        helper
    }

    /// What the hub's supervisor does the moment a helper answers: the link published, and
    /// every account brought up on it.
    async fn link(&self, socket: &Path) {
        let hub = self.daemon.manager.hub();
        hub.set_socket(socket);
        let (link, _requests) = HelperLink::connect(socket).await.unwrap();
        hub.set_link(Some(link));
        self.daemon.manager.resume_all().await;
    }

    /// A folder `name` with the files `(name, byte, size)` in a source directory beside it.
    fn source(&self, name: &str, files: &[(&str, u8, usize)]) -> PathBuf {
        let source = self.dir.path().join(format!("{name}-source"));
        std::fs::create_dir(&source).unwrap();
        for (file, byte, size) in files {
            std::fs::write(source.join(file), vec![*byte; *size]).unwrap();
        }
        source
    }
}

/// The D-Bus error name of a refused call.
#[track_caller]
fn refusal<T: std::fmt::Debug>(result: zbus::Result<T>) -> String {
    let error = result.expect_err("this call was supposed to be refused");
    error_name(&error).unwrap_or_else(|| panic!("not a D-Bus method error: {error}")).to_owned()
}

/// Design test 8: `Add` and `Remove`, the ordered `Accounts`, the `ObjectManager`'s
/// `InterfacesAdded` and `InterfacesRemoved`, and the checked-in XML of `Accounts1` and
/// `Files1` against the live object.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accounts_are_added_listed_announced_and_removed() {
    let d = Daemon::start().await;
    let objects = zbus::fdo::ObjectManagerProxy::builder(&d.client)
        .destination(konedrive_dbus::SERVICE_NAME)
        .unwrap()
        .path(ACCOUNTS_PATH)
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut added = objects.receive_interfaces_added().await.unwrap();
    let mut removed = objects.receive_interfaces_removed().await.unwrap();
    let properties = zbus::fdo::PropertiesProxy::builder(&d.client)
        .destination(konedrive_dbus::SERVICE_NAME)
        .unwrap()
        .path(ACCOUNTS_PATH)
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut changed = properties.receive_properties_changed().await.unwrap();
    assert!(d.manager.accounts().await.unwrap().is_empty());

    let family = d.manager.add(" Family ").await.unwrap();
    let announced = tokio::time::timeout(Duration::from_secs(5), added.next()).await.unwrap().unwrap();
    assert_eq!(announced.args().unwrap().object_path.as_str(), family.as_str());
    // What the window's `AccountsModel` follows.
    let signal = tokio::time::timeout(Duration::from_secs(5), changed.next()).await.unwrap().unwrap();
    let args = signal.args().unwrap();
    assert_eq!(args.interface_name.as_str(), ACCOUNTS_INTERFACE_NAME);
    assert!(args.changed_properties.contains_key("Accounts"), "{:?}", args.changed_properties.keys().collect::<Vec<_>>());
    let account = Account1Proxy::new(&d.client, family.clone()).await.unwrap();
    assert_eq!(account.label().await.unwrap(), "Family", "trimmed");
    assert_eq!(format!("{ACCOUNTS_PATH}/{}", account.id().await.unwrap()), family.as_str());
    assert_eq!(refusal(d.manager.add("family").await), "org.freedesktop.DBus.Error.InvalidArgs", "a label used, in another case");
    assert_eq!(refusal(d.manager.add("a/b").await), "org.freedesktop.DBus.Error.InvalidArgs");

    let personal = d.manager.add("Personal").await.unwrap();
    assert_eq!(d.manager.accounts().await.unwrap(), vec![family.clone(), personal.clone()], "in the order added");
    let managed = objects.get_managed_objects().await.unwrap();
    for path in [&family, &personal] {
        let interfaces: Vec<String> = managed[path].keys().map(|name| name.to_string()).collect();
        assert!(interfaces.contains(&ACCOUNT_INTERFACE_NAME.to_owned()), "{interfaces:?}");
    }

    d.manager.remove(&family.as_ref()).await.unwrap();
    assert_eq!(d.manager.accounts().await.unwrap(), vec![personal.clone()]);
    let gone = tokio::time::timeout(Duration::from_secs(5), removed.next()).await.unwrap().unwrap();
    assert_eq!(gone.args().unwrap().object_path.as_str(), family.as_str());
    let managed = objects.get_managed_objects().await.unwrap();
    assert!(!managed.contains_key(&family) && managed.contains_key(&personal), "{:?}", managed.keys().collect::<Vec<_>>());
    assert_eq!(refusal(d.manager.remove(&family.as_ref()).await), "org.konedrive.Error.NoAccount");
    let config = std::fs::read_to_string(d.config.path().join("config.toml")).unwrap();
    assert!(!config.contains("Family") && config.contains("Personal"), "{config}");

    let live = introspect(&d.client, ACCOUNTS_PATH).await;
    assert_eq!(signature_lines(&live, ACCOUNTS_INTERFACE_NAME), signature_lines(ACCOUNTS_XML, ACCOUNTS_INTERFACE_NAME));
    assert_eq!(signature_lines(&live, FILES_INTERFACE_NAME), signature_lines(FILES_XML, FILES_INTERFACE_NAME));
}

/// Design §8.3 (test 7): a folder that is, is inside, or contains another account's folder
/// is refused `Overlaps`, naming that account — both ways.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_that_nests_with_another_accounts_is_refused_naming_it() {
    let d = Daemon::start().await;
    let (a, b) = (d.manager.add("A").await.unwrap(), d.manager.add("B").await.unwrap());
    let (in_a, in_b) = (d.dir.path().join("A"), d.dir.path().join("B"));
    for folder in [&in_a, &in_b] {
        std::fs::create_dir_all(folder).unwrap();
    }
    d.sync(&a).await.register_root_without_interception(in_a.to_str().unwrap()).await.unwrap();
    std::fs::create_dir(in_a.join("inner")).unwrap();

    let b_sync = d.sync(&b).await;
    let inside = b_sync.register_root_without_interception(in_a.join("inner").to_str().unwrap()).await.unwrap_err();
    assert_eq!(error_name(&inside), Some("org.konedrive.Error.Overlaps"));
    assert!(inside.to_string().contains("'A'"), "{inside}");
    let around = b_sync.register_root_without_interception(d.dir.path().to_str().unwrap()).await;
    assert_eq!(refusal(around), "org.konedrive.Error.Overlaps", "a folder that contains A's");
    b_sync.register_root_without_interception(in_b.to_str().unwrap()).await.unwrap();
}

/// Design test 9: two accounts, two local folders filled from two sources. `Files1.Hydrate`
/// and a hydration request from the helper each fill from the right account's source, and
/// the downloads are the right account's activity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_account_fills_from_its_own_source() {
    let d = Daemon::start().await;
    let helper = d.supervise_helper().await;
    let (a, b) = (d.manager.add("A").await.unwrap(), d.manager.add("B").await.unwrap());
    let (a_sync, b_sync) = (d.sync(&a).await, d.sync(&b).await);
    let (in_a, in_b) = (d.dir.path().join("A"), d.dir.path().join("B"));
    for (sync, folder, source) in [
        (&a_sync, &in_a, d.source("A", &[("doc.bin", 1, 4096), ("opened.bin", 4, 1024)])),
        (&b_sync, &in_b, d.source("B", &[("doc.bin", 2, 4096), ("opened.bin", 3, 2048)])),
    ] {
        std::fs::create_dir(folder).unwrap();
        sync.register_root_without_interception(folder.to_str().unwrap()).await.unwrap();
        sync.populate_from_directory(source.to_str().unwrap()).await.unwrap();
    }

    d.files.hydrate(in_a.join("doc.bin").to_str().unwrap()).await.unwrap();
    d.files.hydrate(in_b.join("doc.bin").to_str().unwrap()).await.unwrap();
    assert_eq!(std::fs::read(in_a.join("doc.bin")).unwrap(), vec![1u8; 4096]);
    assert_eq!(std::fs::read(in_b.join("doc.bin")).unwrap(), vec![2u8; 4096]);

    // An open of B's file, as the helper hands it over; both folders are on one
    // filesystem, so it is B's by its name, proved by its inode.
    let opened = in_b.join("opened.bin");
    let open = |path: &Path| -> OwnedFd { std::fs::File::options().read(true).write(true).open(path).unwrap().into() };
    helper.send_request(7, &open(&opened));
    assert_eq!(helper.answer(7).await, 0);
    assert_eq!(std::fs::read(&opened).unwrap(), vec![3u8; 2048]);
    // And one of A's, which has a file of the same name: a router that picked by any
    // other rule than the file's own folder would fill one of the two wrong.
    helper.send_request(8, &open(&in_a.join("opened.bin")));
    assert_eq!(helper.answer(8).await, 0);
    assert_eq!(std::fs::read(in_a.join("opened.bin")).unwrap(), vec![4u8; 1024]);
    // A file in neither folder is no one's, and its open is answered EIO on the wire.
    let stray = d.dir.path().join("stray.bin");
    std::fs::write(&stray, b"").unwrap();
    helper.send_request(9, &open(&stray));
    assert_eq!(helper.answer(9).await, libc::EIO);

    let shown = |events: Vec<(i64, String, String, String)>| -> Vec<(String, String)> {
        events.into_iter().map(|(_, kind, path, _)| (kind, path)).collect()
    };
    let path = |p: PathBuf| std::fs::canonicalize(p).unwrap().display().to_string();
    eventually("B's download on open is recorded", || async {
        b_sync.recent_activity(10).await.unwrap().len() == 2
    })
    .await;
    let mut in_b_activity = shown(b_sync.recent_activity(10).await.unwrap());
    in_b_activity.sort();
    assert_eq!(
        in_b_activity,
        vec![("downloaded".to_owned(), path(in_b.join("doc.bin"))), ("downloaded".to_owned(), path(opened))]
    );
    eventually("A's download on open is recorded", || async {
        a_sync.recent_activity(10).await.unwrap().len() == 2
    })
    .await;
    let mut in_a_activity = shown(a_sync.recent_activity(10).await.unwrap());
    in_a_activity.sort();
    assert_eq!(
        in_a_activity,
        vec![("downloaded".to_owned(), path(in_a.join("doc.bin"))), ("downloaded".to_owned(), path(in_a.join("opened.bin")))]
    );
    assert_eq!(d.files.item_state(in_a.join("doc.bin").to_str().unwrap()).await.unwrap(), "hydrated");
    assert_eq!(
        refusal(d.files.hydrate(d.dir.path().join("A-source/doc.bin").to_str().unwrap()).await),
        "org.konedrive.Error.OutsideRoot",
        "a file in no account's folder"
    );
}

/// `Files1.Pin` and `FreeUp` over two accounts: every path is routed before anything
/// changes — one in no account's folder refuses the whole call — and then each account
/// does its own paths, the counts summed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_files1_call_over_two_accounts_is_refused_whole_or_done_whole() {
    let d = Daemon::start().await;
    let (a, b) = (d.manager.add("A").await.unwrap(), d.manager.add("B").await.unwrap());
    let (in_a, in_b) = (d.dir.path().join("A"), d.dir.path().join("B"));
    for (account, folder, source) in [
        (&a, &in_a, d.source("A", &[("a.bin", 1, 4096)])),
        (&b, &in_b, d.source("B", &[("b.bin", 2, 4096)])),
    ] {
        std::fs::create_dir(folder).unwrap();
        let sync = d.sync(account).await;
        sync.register_root_without_interception(folder.to_str().unwrap()).await.unwrap();
        sync.populate_from_directory(source.to_str().unwrap()).await.unwrap();
    }
    let (a_file, b_file) = (in_a.join("a.bin"), in_b.join("b.bin"));
    let outside = d.dir.path().join("A-source/a.bin");
    let both = [a_file.to_str().unwrap(), b_file.to_str().unwrap()];

    assert_eq!(refusal(d.files.pin(&[both[0], outside.to_str().unwrap()]).await), "org.konedrive.Error.OutsideRoot");
    assert_eq!(xattr::get(&a_file, "user.konedrive.pin").unwrap(), None, "nothing was pinned");
    // Review M4: a path B refuses (not one of ours) refuses A's pin too.
    let stray = in_b.join("stray.txt");
    std::fs::write(&stray, b"mine").unwrap();
    assert_eq!(refusal(d.files.pin(&[both[0], stray.to_str().unwrap()]).await), "org.konedrive.Error.NotManaged");
    assert_eq!(xattr::get(&a_file, "user.konedrive.pin").unwrap(), None, "nothing was pinned in A either");
    // Review M5: a path through `..` is routed where it leads.
    let through = in_a.join("..").join("B").join("b.bin");
    assert_eq!(d.files.item_state(through.to_str().unwrap()).await.unwrap(), "online-only", "B's file, not A's");

    assert_eq!(d.files.pin(&both).await.unwrap(), 2, "one queued in each account");
    for file in [&a_file, &b_file] {
        let files = &d.files;
        eventually("the pinned file is downloaded", || async move {
            files.item_state(file.to_str().unwrap()).await.unwrap() == "hydrated"
        })
        .await;
    }

    let (freed, bytes, busy, kept) = d.files.free_up(&both).await.unwrap();
    assert_eq!((freed, busy, kept), (2, 0, 0), "their own pins came off first");
    assert!(bytes >= 8192, "{bytes}");
    assert_eq!(d.files.item_state(both[1]).await.unwrap(), "online-only");
}

/// Design test 10: `Remove` forgets the folder through the helper, deletes the refresh
/// token, the cached name and quota and the tree store, and keeps the rescued files. With
/// an intercepted folder and no helper, it is refused `NoHelper`, and nothing changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_an_account_forgets_its_folder_and_keeps_its_rescued_files() {
    let d = Daemon::start().await;
    let (helper, socket) = d.connect_helper().await;
    let path = d.manager.add("Personal").await.unwrap();
    let account = d.account(&path);
    let files = Paths::in_dir(d.config.path()).account(&account.id).unwrap();
    account.account.state().update(|s| s.state = SignInState::SignedIn);
    let item = Slot::Account(account.id.clone());
    d.wallet.store(&item, "KOneDrive: ann@outlook.com", "RT").await.unwrap();
    std::fs::write(&files.account_cache, "{}").unwrap();
    std::fs::write(&files.tree_db, "a tree store").unwrap();
    std::fs::create_dir_all(files.rescue_dir.join("2026-09-25")).unwrap();
    std::fs::write(files.rescue_dir.join("2026-09-25/mine.txt"), "rescued").unwrap();
    let folder = d.dir.path().join("OneDrive");
    std::fs::create_dir(&folder).unwrap();
    d.sync(&path).await.register_root(folder.to_str().unwrap()).await.unwrap();

    // The helper goes away: an intercepted folder is forgotten through it, or not at all.
    d.daemon.manager.hub().set_link(None);
    account.sync.report_helper_lost();
    assert_eq!(refusal(d.manager.remove(&path.as_ref()).await), "org.konedrive.Error.NoHelper");
    assert_eq!(d.manager.accounts().await.unwrap(), vec![path.clone()]);
    assert_eq!(d.wallet.current(&item).as_deref(), Some("RT"), "nothing changed");
    assert!(files.tree_db.exists() && files.account_cache.exists());
    let config = std::fs::read_to_string(d.config.path().join("config.toml")).unwrap();
    assert!(config.contains(&account.id), "{config}");
    assert!(config.contains("[accounts.root]") && config.contains(&folder.canonicalize().unwrap().display().to_string()), "{config}");

    d.link(&socket).await;
    d.manager.remove(&path.as_ref()).await.unwrap();

    assert_eq!(helper.seen().last(), Some(&"UnregisterRoot"), "{:?}", helper.seen());
    assert!(d.manager.accounts().await.unwrap().is_empty());
    assert_eq!(d.wallet.current(&item), None, "the refresh token is deleted");
    assert!(!files.dir.exists(), "the cached name and quota and the tree store are deleted");
    assert_eq!(std::fs::read_to_string(files.rescue_dir.join("2026-09-25/mine.txt")).unwrap(), "rescued");
    assert!(!std::fs::read_to_string(d.config.path().join("config.toml")).unwrap().contains(&account.id));
    assert!(folder.exists(), "the folder's files are kept");
}

/// Review I1: an account held back (§3.1) never brings its folder up, but a folder it
/// registered with interception in an earlier session is still the helper's. `Remove`
/// forgets it through the helper — refused `NoHelper` without one, changing nothing — before
/// the account goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_a_held_account_forgets_its_folder_through_the_helper() {
    let (config, dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let folder = std::fs::canonicalize(dir.path()).unwrap().join("Family");
    std::fs::create_dir(&folder).unwrap();
    let root_id = "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d";
    xattr::set(&folder, "user.konedrive.root", root_id.as_bytes()).unwrap();
    // Hand-edited: the second account's label repeats the first's.
    std::fs::write(
        config.path().join("config.toml"),
        format!(
            "config_version = 2\n\n[[accounts]]\nid = \"0123456789ab\"\nlabel = \"Personal\"\n\n\
             [[accounts]]\nid = \"ba9876543210\"\nlabel = \"personal\"\n\n[accounts.root]\npath = \"{}\"\n\
             id = \"{root_id}\"\nintercepted = true\nsource = \"local\"\n",
            folder.display()
        ),
    )
    .unwrap();
    let d = Daemon::start_in(config, dir).await;
    let held = konedrive_dbus::account_path("ba9876543210").unwrap();
    let sync = d.sync(&held).await;
    assert_eq!(sync.root_state().await.unwrap(), "error");
    assert!(sync.last_error().await.unwrap().contains("held back"), "{}", sync.last_error().await.unwrap());
    let recorded = || std::fs::read_to_string(d.config.path().join("config.toml")).unwrap();

    assert_eq!(refusal(d.manager.remove(&held.as_ref()).await), "org.konedrive.Error.NoHelper");
    assert_eq!(d.manager.accounts().await.unwrap().len(), 2, "nothing changed");
    assert!(recorded().contains(root_id), "the folder's record stays: {}", recorded());

    let (helper, _socket) = d.connect_helper().await;
    d.manager.remove(&held.as_ref()).await.unwrap();
    assert_eq!(helper.seen(), vec!["UnregisterRoot"], "forgotten through the helper");
    assert_eq!(d.manager.accounts().await.unwrap().len(), 1);
    assert!(!recorded().contains(root_id), "{}", recorded());
}

/// Review M7: one daemon per configuration. A second one started on the same files while
/// the first runs is refused before it reads them, so two daemons never migrate at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_daemon_on_the_same_configuration_is_refused() {
    let d = Daemon::start().await;
    let other_bus = TestBus::start();
    let options = konedrived::accounts::Options {
        endpoints: Endpoints::microsoft(),
        wallet: Arc::new(MemoryWallet::default()),
        sign_in_timeout: Duration::from_secs(5),
        baloo: konedrived::sync::baloo::Baloo::disabled,
        thumbnails: None,
        onedrive: false,
    };
    let second = konedrived::accounts::start(other_bus.builder(), Paths::in_dir(d.config.path()), options).await;
    let error = second.err().expect("a second daemon was started").to_string();
    assert!(error.contains("another konedrived is running"), "{error}");
}

/// Design test 11: a daemon started on a version-1 configuration — a folder, the cached
/// name and quota, a tree store and the wallet's refresh token of before — has one account,
/// `Personal`, with that folder, signed in; its files moved into its own directory, and its
/// refresh token into its own wallet item at the first refresh.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_version_1_configuration_starts_as_personal_with_its_folder() {
    let bus = TestBus::start();
    let server = MockServer::start().await;
    mock_microsoft(&server).await;
    let config = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(config.path());
    let folders = tempfile::tempdir().unwrap();
    let folder = std::fs::canonicalize(folders.path()).unwrap().join("OneDrive");
    std::fs::create_dir(&folder).unwrap();
    let root_id = "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d";
    xattr::set(&folder, "user.konedrive.root", root_id.as_bytes()).unwrap();
    std::fs::write(
        &paths.config_file,
        format!(
            "client_id = \"{CLIENT_ID}\"\nsync_root = \"{}\"\nsync_root_intercepted = false\nsync_root_id = \"{root_id}\"\n\
             sync_root_source = \"local\"\nsync_root_upgrade_when_helper = false\n",
            folder.display()
        ),
    )
    .unwrap();
    std::fs::write(
        &paths.account_cache,
        r#"{"display_name":"Ann","email":"ann@outlook.com","quota_used":1,"quota_total":2,"fetched_at":0}"#,
    )
    .unwrap();
    drop(konedrived::tree::TreeStore::open(&paths.tree_db).unwrap());
    let wallet = Arc::new(MemoryWallet::with_v1("RT0"));

    let daemon = start_daemon(&bus, config.path(), endpoints(&server), Arc::clone(&wallet), Duration::from_secs(5)).await;

    let client = bus.connect().await;
    let manager = Accounts1Proxy::new(&client).await.unwrap();
    let accounts = manager.accounts().await.unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(manager.client_id().await.unwrap(), CLIENT_ID);
    let account = Account1Proxy::builder(&client)
        .path(accounts[0].clone())
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap();
    let sync = Sync1Proxy::new(&client, accounts[0].clone()).await.unwrap();
    assert_eq!(account.label().await.unwrap(), "Personal");
    assert_eq!(account.state().await.unwrap(), "signed-in", "the wallet's token of before counts");
    assert_eq!(sync.root_path().await.unwrap(), folder.display().to_string());
    assert_eq!(sync.root_state().await.unwrap(), "no-interception");

    let id = account.id().await.unwrap();
    let moved = paths.account(&id).unwrap();
    assert!(moved.tree_db.exists() && !paths.tree_db.exists(), "the tree store is the account's");
    assert!(paths.config_file.with_file_name("config.toml.v1").exists(), "version 1 is kept");
    eventually("the first refresh", || async { account.display_name().await.unwrap() == "Test User" }).await;
    assert_eq!(wallet.current(&Slot::V1), None, "version 1's item is moved");
    assert_eq!(wallet.current(&Slot::Account(id.clone())).as_deref(), Some("RT1"));
    assert!(moved.account_cache.exists());
    assert!(!daemon.manager.config().account(&id).unwrap().legacy_token);
}

/// Design test 11, the common case (review M8): version 1's OneDrive folder, registered
/// with interception, its drive recorded. The account comes up holding the folder until the
/// helper is back, with the drive carried over as the account's identity; at the hub's first
/// connect the folder is registered with the helper again, brought up, and given its drive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_version_1_onedrive_folder_is_held_then_brought_up_at_the_first_connect() {
    let bus = TestBus::start();
    let server = MockServer::start().await;
    mock_microsoft(&server).await;
    let (config, folders) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths::in_dir(config.path());
    let folder = std::fs::canonicalize(folders.path()).unwrap().join("OneDrive");
    std::fs::create_dir(&folder).unwrap();
    let root_id = "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d";
    xattr::set(&folder, "user.konedrive.root", root_id.as_bytes()).unwrap();
    std::fs::write(
        &paths.config_file,
        format!(
            "client_id = \"{CLIENT_ID}\"\nsync_root = \"{}\"\nsync_root_id = \"{root_id}\"\n\
             sync_root_source = \"onedrive\"\nsync_root_drive_id = \"D1\"\n",
            folder.display()
        ),
    )
    .unwrap();
    let options = konedrived::accounts::Options {
        endpoints: endpoints(&server),
        wallet: Arc::new(MemoryWallet::with_v1("RT0")),
        sign_in_timeout: Duration::from_secs(5),
        baloo: konedrived::sync::baloo::Baloo::disabled,
        thumbnails: None,
        onedrive: true,
    };
    let daemon = start_daemon_with(&bus, config.path(), options).await;

    let client = bus.connect().await;
    let path = Accounts1Proxy::new(&client).await.unwrap().accounts().await.unwrap().remove(0);
    let id = path.as_str().rsplit('/').next().unwrap().to_owned();
    let sync =
        Sync1Proxy::builder(&client).path(path).unwrap().cache_properties(zbus::proxy::CacheProperties::No).build().await.unwrap();
    assert_eq!(sync.root_path().await.unwrap(), folder.display().to_string());
    assert_eq!(sync.root_state().await.unwrap(), "error", "held until the helper is back");
    assert!(sync.last_error().await.unwrap().contains("helper is not connected"), "{}", sync.last_error().await.unwrap());
    assert_eq!(daemon.manager.config().account(&id).unwrap().drive_id, "D1", "the folder's drive is the account's");

    let socket = folders.path().join("helper.sock");
    let helper = FakeHelper::start(&socket);
    tokio::spawn(konedrived::sync::hub::supervise(Arc::clone(daemon.manager.hub()), socket, Duration::from_millis(50)));
    eventually("registered with the helper again", || {
        let seen = helper.seen();
        async move { seen.contains(&"RegisterRoot") }
    })
    .await;
    eventually("the folder carries its drive", || {
        let drive = xattr::get(&folder, "user.konedrive.drive").unwrap();
        async move { drive.as_deref() == Some(&b"D1"[..]) }
    })
    .await;
    assert_ne!(sync.last_error().await.unwrap(), "", "OneDrive's listing is not mocked here, so the sync says so");
}
