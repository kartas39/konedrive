//! The multiple-accounts daemon over a private test bus: the
//! manager at `/org/konedrive/Accounts` — `Accounts`, `Files` and the `ObjectManager` —
//! and the accounts below it, started as `main` starts them (`accounts::start`). A fake
//! helper speaks the wire protocol; nothing is intercepted for real.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use futures_util::StreamExt;
use konedrive_dbus::accounts::{AccountProxy, AccountsProxy, FilesProxy};
use konedrive_dbus::testing::TestBus;
use konedrive_dbus::{error_name, ACCOUNTS_INTERFACE_NAME, ACCOUNTS_PATH, ACCOUNT_INTERFACE_NAME, FILES_INTERFACE_NAME};
use konedrived::config::Paths;
use konedrive_graph::oauth::Endpoints;
use konedrived::account::testing::MemoryWallet;
use zbus::zvariant::OwnedObjectPath;

const ACCOUNTS_XML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dbus/org.konedrive.Accounts.xml"));
const FILES_XML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dbus/org.konedrive.Files.xml"));

/// The daemon, with no account yet, and a client of it.
struct Daemon {
    daemon: konedrived::daemon::startup::Daemon,
    client: zbus::Connection,
    manager: AccountsProxy<'static>,
    _files: FilesProxy<'static>,
    _wallet: Arc<MemoryWallet>,
    /// `config.toml` and the accounts' files.
    config: tempfile::TempDir,
    /// What a test puts beside it.
    _dir: tempfile::TempDir,
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
            AccountsProxy::builder(&client).cache_properties(zbus::proxy::CacheProperties::No).build().await.unwrap();
        let files = FilesProxy::new(&client).await.unwrap();
        Self { daemon, client, manager, _files: files, _wallet: wallet, config, _dir: dir, _bus: bus }
    }

    /// A signed-out account under `label`, as the manager adds one: nothing on the bus
    /// adds an account but `Accounts.SignIn`.
    async fn add(&self, label: &str) -> OwnedObjectPath {
        self.daemon.manager.add(label, &self.daemon.connection).await.unwrap().path.clone()
    }

    /// The daemon's own half of an account, for what no method reaches.
    fn account(&self, path: &OwnedObjectPath) -> Arc<konedrived::daemon::manager::Account> {
        self.daemon.manager.account(&path.as_ref()).unwrap()
    }
}

/// The D-Bus error name of a refused call.
#[track_caller]
fn refusal<T: std::fmt::Debug>(result: zbus::Result<T>) -> String {
    let error = result.expect_err("this call was supposed to be refused");
    error_name(&error).unwrap_or_else(|| panic!("not a D-Bus method error: {error}")).to_owned()
}

/// Design test 8: an account added and `Remove`, the ordered `List`, the `ObjectManager`'s
/// `InterfacesAdded` and `InterfacesRemoved`, and the checked-in XML of `Accounts` and
/// `Files` against the live object.
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
    assert!(d.manager.list().await.unwrap().is_empty());

    let family = d.add(" Family ").await;
    let announced = tokio::time::timeout(Duration::from_secs(5), added.next()).await.unwrap().unwrap();
    assert_eq!(announced.args().unwrap().object_path.as_str(), family.as_str());
    let account = AccountProxy::new(&d.client, family.clone()).await.unwrap();
    assert_eq!(account.label().await.unwrap(), "Family", "trimmed");
    assert_eq!(format!("{ACCOUNTS_PATH}/{}", account.id().await.unwrap()), family.as_str());
    for refused in ["family", "a/b"] {
        let result = d.daemon.manager.add(refused, &d.daemon.connection).await;
        assert!(matches!(result, Err(konedrived::daemon::manager::ManagerError::InvalidArgs(_))), "{refused}: a label used in another case, or with a slash");
    }

    let personal = d.add("Personal").await;
    assert_eq!(d.manager.list().await.unwrap(), vec![family.clone(), personal.clone()], "in the order added");
    let managed = objects.get_managed_objects().await.unwrap();
    for path in [&family, &personal] {
        let interfaces: Vec<String> = managed[path].keys().map(|name| name.to_string()).collect();
        assert!(interfaces.contains(&ACCOUNT_INTERFACE_NAME.to_owned()), "{interfaces:?}");
    }
    // The accounts as their folders see each other are the listed ones, in that order.
    let folders = || d.daemon.manager.registry().accounts().iter().map(|sync| format!("{ACCOUNTS_PATH}/{}", sync.id())).collect::<Vec<_>>();
    assert_eq!(folders(), [family.as_str(), personal.as_str()]);

    d.manager.remove(&family.as_ref()).await.unwrap();
    assert_eq!(d.manager.list().await.unwrap(), vec![personal.clone()]);
    // What the window's `AccountsModel` follows.
    let signal = tokio::time::timeout(Duration::from_secs(5), changed.next()).await.unwrap().unwrap();
    let args = signal.args().unwrap();
    assert_eq!(args.interface_name.as_str(), ACCOUNTS_INTERFACE_NAME);
    assert!(args.changed_properties.contains_key("List"), "{:?}", args.changed_properties.keys().collect::<Vec<_>>());
    assert_eq!(folders(), [personal.as_str()]);
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

/// `PauseOnMetered` and `OnBattery` are the manager's, one pair for every
/// account. A start moves an account's keys of before to them (the strictest value); each
/// setter writes `config.toml`, announces the change, and every account runs on it at once;
/// `SetOnBattery` refuses what is not a choice, changing nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hold_settings_are_one_pair_for_every_account() {
    use konedrived::config::OnBattery;
    let config = tempfile::tempdir().unwrap();
    std::fs::write(
        config.path().join("config.toml"),
        "config_version = 2\n\
         [[accounts]]\nid = \"3f9a1c0e5b7d\"\nlabel = \"Personal\"\norigin = \"added\"\non_battery = \"sync\"\npause_on_metered = false\n\
         [[accounts]]\nid = \"8c21d07a44e1\"\nlabel = \"Family\"\norigin = \"added\"\non_battery = \"pause\"\npause_on_metered = false\n",
    )
    .unwrap();
    let d = Daemon::start_in(config, tempfile::tempdir().unwrap()).await;
    let file = d.config.path().join("config.toml");
    let accounts = d.manager.list().await.unwrap();
    assert_eq!(accounts.len(), 2);
    assert!(!d.manager.pause_on_metered().await.unwrap(), "moved: every account said off");
    assert_eq!(d.manager.on_battery().await.unwrap(), "pause", "moved: the strictest");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(!text.split("[[accounts]]").skip(1).any(|a| a.contains("on_battery") || a.contains("pause_on_metered")), "{text}");
    let hold = |path: &OwnedObjectPath| d.account(path).sync.hold_settings();
    assert!(accounts.iter().all(|a| hold(a).on_battery == OnBattery::Pause && !hold(a).pause_on_metered));

    let properties = zbus::fdo::PropertiesProxy::builder(&d.client)
        .destination(konedrive_dbus::SERVICE_NAME)
        .unwrap()
        .path(ACCOUNTS_PATH)
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut changed = properties.receive_properties_changed().await.unwrap();
    async fn announced(changed: &mut zbus::fdo::PropertiesChangedStream) -> Vec<String> {
        let signal = tokio::time::timeout(Duration::from_secs(5), changed.next()).await.unwrap().unwrap();
        signal.args().unwrap().changed_properties.keys().map(|k| k.to_string()).collect()
    }

    d.manager.set_pause_on_metered(true).await.unwrap();
    assert_eq!(announced(&mut changed).await, ["PauseOnMetered"]);
    assert!(d.manager.pause_on_metered().await.unwrap());
    d.manager.set_on_battery("sync").await.unwrap();
    assert_eq!(announced(&mut changed).await, ["OnBattery"]);
    assert_eq!(d.manager.on_battery().await.unwrap(), "sync");
    assert!(accounts.iter().all(|a| hold(a).on_battery == OnBattery::Sync && hold(a).pause_on_metered), "every account at once");
    let text = std::fs::read_to_string(&file).unwrap();
    let top = text.split("[[accounts]]").next().unwrap();
    assert!(top.contains("pause_on_metered = true") && top.contains("on_battery = \"sync\""), "{text}");

    assert_eq!(refusal(d.manager.set_on_battery("whenever").await), "org.freedesktop.DBus.Error.InvalidArgs");
    assert_eq!(d.manager.on_battery().await.unwrap(), "sync", "a refusal changes nothing");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
}

/// A hand-edited id that is not an account id: the account is not loaded, the others are,
/// and `Accounts.LastError` says which and why, with the id as the file has it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_account_whose_id_is_not_one_is_not_loaded_and_last_error_says_so() {
    let (config, dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(
        config.path().join("config.toml"),
        "config_version = 2\n\n[[accounts]]\nid = \"0123456789ab\"\nlabel = \"Personal\"\n\n\
         [[accounts]]\nid = \"bad\"\nlabel = \"Work\"\n",
    )
    .unwrap();
    let d = Daemon::start_in(config, dir).await;
    assert_eq!(d.manager.list().await.unwrap().len(), 1);
    assert_eq!(
        d.manager.last_error().await.unwrap(),
        "the account \"Work\" is not loaded: its id \"bad\" is not 12 lowercase hexadecimal characters"
    );
}

/// One daemon per configuration. A second one started on the same files while
/// the first runs is refused before it reads them, so two daemons never migrate at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_daemon_on_the_same_configuration_is_refused() {
    let d = Daemon::start().await;
    let other_bus = TestBus::start();
    let options = konedrived::daemon::manager::Options {
        endpoints: Endpoints::microsoft(),
        wallet: Arc::new(MemoryWallet::default()),
        sign_in_timeout: Duration::from_secs(5),
        baloo: konedrived::desktop::baloo::Baloo::disabled,
        thumbnails: None,
        drive: konedrived::daemon::manager::no_drive(),
        bus: Arc::new(konedrived::dbus::export::OnBus),
    };
    let second = konedrived::daemon::startup::start(other_bus.builder(), Paths::in_dir(d.config.path()), options).await;
    let error = second.err().expect("a second daemon was started").to_string();
    assert!(error.contains("another konedrived is running"), "{error}");
}

/// A bus on which an account's folder cannot be put while it is told so.
struct FailingExports {
    failing: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl konedrived::daemon::manager::Bus for FailingExports {
    async fn serve(&self, connection: &zbus::Connection, manager: &Arc<konedrived::daemon::manager::AccountManager>) -> zbus::Result<()> {
        konedrived::dbus::export::OnBus.serve(connection, manager).await
    }

    async fn export(
        &self,
        connection: &zbus::Connection,
        path: &zbus::zvariant::ObjectPath<'_>,
        account: Arc<konedrived::account::AccountService>,
        sync: Arc<konedrived::sync::SyncService>,
    ) -> zbus::Result<Vec<tokio::task::JoinHandle<()>>> {
        let signals = konedrived::dbus::export::OnBus.export(connection, path, account, sync).await?;
        if self.failing.load(std::sync::atomic::Ordering::SeqCst) {
            // As an export that failed part of the way leaves it: some of the account's
            // interfaces on the bus, one in the middle not.
            signals.iter().for_each(|task| task.abort());
            let transfers = zbus::names::InterfaceName::from_static_str(konedrive_dbus::TRANSFERS_INTERFACE_NAME)?;
            connection.object_server().remove_named(path, transfers).await?;
            return Err(zbus::Error::Failure("no folder on this bus".into()));
        }
        Ok(signals)
    }

    async fn unexport(&self, connection: &zbus::Connection, path: &zbus::zvariant::ObjectPath<'_>, partly: bool) -> zbus::Result<()> {
        konedrived::dbus::export::OnBus.unexport(connection, path, partly).await
    }

    async fn sign_in_finished(&self, connection: &zbus::Connection, sign_in: u32, outcome: &str, message: &str, account: &zbus::zvariant::ObjectPath<'_>, listed: bool) -> zbus::Result<()> {
        konedrived::dbus::export::OnBus.sign_in_finished(connection, sign_in, outcome, message, account, listed).await
    }
}

/// An adding whose account cannot be put on the bus leaves nothing: no account in
/// `config.toml` to come up at the next start, no object, no directory, and the label free.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_add_that_cannot_be_put_on_the_bus_leaves_nothing_behind() {
    let config = tempfile::tempdir().unwrap();
    let bus = TestBus::start();
    let exports = Arc::new(FailingExports { failing: true.into() });
    let options = konedrived::daemon::manager::Options {
        endpoints: Endpoints::microsoft(),
        wallet: Arc::new(MemoryWallet::default()),
        sign_in_timeout: Duration::from_secs(5),
        baloo: konedrived::desktop::baloo::Baloo::disabled,
        thumbnails: None,
        drive: konedrived::daemon::manager::no_drive(),
        bus: exports.clone(),
    };
    let daemon = start_daemon_with(&bus, config.path(), options).await;
    let client = bus.connect().await;
    let manager = AccountsProxy::builder(&client).cache_properties(zbus::proxy::CacheProperties::No).build().await.unwrap();

    assert!(daemon.manager.add("Personal", &daemon.connection).await.is_err(), "the account's folder cannot be put on the bus");
    assert!(manager.list().await.unwrap().is_empty());
    assert!(daemon.manager.registry().accounts().is_empty(), "nor is its folder one of the daemon's");
    let written = std::fs::read_to_string(Paths::in_dir(config.path()).config_file).unwrap_or_default();
    assert!(!written.contains("Personal"), "config.toml keeps no account: {written}");
    let accounts = Paths::in_dir(config.path()).state_dir.join("accounts");
    let left: Vec<_> = std::fs::read_dir(&accounts).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    assert!(left.is_empty(), "no directory of the account is left: {left:?}");
    let objects = introspect(&client, ACCOUNTS_PATH).await;
    assert!(!objects.contains("<node name="), "no object of the account is left on the bus: {objects}");

    exports.failing.store(false, std::sync::atomic::Ordering::SeqCst);
    let path = daemon.manager.add("Personal", &daemon.connection).await.expect("the label is free, and the account's object path too").path.clone();
    let account = AccountProxy::builder(&client).path(path.clone()).unwrap().build().await.unwrap();
    assert_eq!(account.label().await.unwrap(), "Personal");
}

/// A bus on which the daemon's own objects are put only when the test lets them: the
/// daemon's start stands still with its connection made and nothing of its own exported.
struct HeldServe {
    /// Given the daemon's unique name when its start reaches `serve`.
    reached: Mutex<Option<tokio::sync::oneshot::Sender<String>>>,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl konedrived::daemon::manager::Bus for HeldServe {
    async fn serve(&self, connection: &zbus::Connection, manager: &Arc<konedrived::daemon::manager::AccountManager>) -> zbus::Result<()> {
        let name = connection.unique_name().expect("a connection to a bus has a unique name").to_string();
        if let Some(reached) = self.reached.lock().unwrap().take() {
            let _ = reached.send(name);
        }
        self.release.notified().await;
        konedrived::dbus::export::OnBus.serve(connection, manager).await
    }

    async fn export(
        &self,
        connection: &zbus::Connection,
        path: &zbus::zvariant::ObjectPath<'_>,
        account: Arc<konedrived::account::AccountService>,
        sync: Arc<konedrived::sync::SyncService>,
    ) -> zbus::Result<Vec<tokio::task::JoinHandle<()>>> {
        konedrived::dbus::export::OnBus.export(connection, path, account, sync).await
    }

    async fn unexport(&self, connection: &zbus::Connection, path: &zbus::zvariant::ObjectPath<'_>, partly: bool) -> zbus::Result<()> {
        konedrived::dbus::export::OnBus.unexport(connection, path, partly).await
    }

    async fn sign_in_finished(&self, connection: &zbus::Connection, sign_in: u32, outcome: &str, message: &str, account: &zbus::zvariant::ObjectPath<'_>, listed: bool) -> zbus::Result<()> {
        konedrived::dbus::export::OnBus.sign_in_finished(connection, sign_in, outcome, message, account, listed).await
    }
}

/// SY13: the daemon's connection answers calls before the daemon's first export. A call
/// sent to its unique name while its start stands at `Bus::serve` — the bus name being
/// claimed last — gets a reply; before the fix it was read from the socket and dropped,
/// because the object server started only at that export, and its caller waited for ever.
///
/// What this does not prove: that the server listens before the socket is read at all. The
/// test's call comes long after the connection is made, so a server started any time before
/// `Bus::serve` would pass it too. That stronger order rests on zbus: a connection built
/// with an interface (`Builder::serve_at`) has its object server subscribed before `build`
/// starts the socket's reader (zbus 5.19, `connection/builder.rs`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_that_reaches_the_daemon_before_its_objects_is_answered() {
    let config = tempfile::tempdir().unwrap();
    let bus = TestBus::start();
    let (reached, at_serve) = tokio::sync::oneshot::channel();
    let held = Arc::new(HeldServe { reached: Mutex::new(Some(reached)), release: tokio::sync::Notify::new() });
    let options = konedrived::daemon::manager::Options {
        endpoints: Endpoints::microsoft(),
        wallet: Arc::new(MemoryWallet::default()),
        sign_in_timeout: Duration::from_secs(5),
        baloo: konedrived::desktop::baloo::Baloo::disabled,
        thumbnails: None,
        drive: konedrived::daemon::manager::no_drive(),
        bus: held.clone(),
    };
    let client = bus.connect().await;

    let early = async {
        let daemon = at_serve.await.expect("the daemon's start reaches its first export");
        let named = zbus::fdo::DBusProxy::new(&client).await.unwrap().name_has_owner(konedrive_dbus::SERVICE_NAME.try_into().unwrap()).await.unwrap();
        assert!(!named, "the bus name is claimed last");
        // A call that needs nothing of the daemon's own: answered.
        client
            .call_method(Some(daemon.as_str()), ACCOUNTS_PATH, Some("org.freedesktop.DBus.Peer"), "Ping", &())
            .await
            .expect("a call sent before the daemon's objects are exported is answered");
        // A call for what is not there yet: refused in a reply, not dropped.
        let refused = client.call_method(Some(daemon.as_str()), ACCOUNTS_PATH, Some(ACCOUNTS_INTERFACE_NAME), "List", &()).await;
        assert!(matches!(refused, Err(zbus::Error::MethodError(..))), "{refused:?}");
        held.release.notify_one();
    };
    let (_daemon, ()) = tokio::join!(start_daemon_with(&bus, config.path(), options), early);

    let manager = AccountsProxy::builder(&client).cache_properties(zbus::proxy::CacheProperties::No).build().await.unwrap();
    assert!(manager.list().await.unwrap().is_empty(), "the daemon is on the bus under its name, with its objects");
}
