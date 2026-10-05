//! `Files.Menu`: what the context menu may offer for a selection. The decision
//! (`AccountManager::menu`) is asked directly, one case per thing it answers about, and each
//! answer is held against what `Pin`, `Unpin` and `FreeUp` then say of the same paths; the
//! call on the bus is made once. Local folders in temporary directories, a private bus.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use konedrive_dbus::accounts::FilesProxy;
use konedrive_dbus::error_name;
use konedrive_dbus::testing::TestBus;
use konedrive_graph::oauth::Endpoints;
use konedrived::account::testing::MemoryWallet;
use konedrived::daemon::manager::Account;
use konedrived::sync::menu::{AlwaysKeep, Menu, Offer};
use konedrived::sync::SyncService;

/// The daemon and a client of it, with folders in `dir`.
struct World {
    daemon: konedrived::daemon::startup::Daemon,
    files: FilesProxy<'static>,
    dir: tempfile::TempDir,
    _config: tempfile::TempDir,
    _bus: TestBus,
}

impl World {
    async fn start() -> Self {
        let bus = TestBus::start();
        let config = tempfile::tempdir().unwrap();
        let wallet = Arc::new(MemoryWallet::default());
        let daemon = start_daemon(&bus, config.path(), Endpoints::microsoft(), wallet, Duration::from_secs(5)).await;
        let files = FilesProxy::new(&bus.connect().await).await.unwrap();
        Self { daemon, files, dir: tempfile::tempdir().unwrap(), _config: config, _bus: bus }
    }

    /// An account `label` whose folder, `<dir>/<label>`, shows `files` (4 KiB each, none
    /// downloaded), registered without interception.
    async fn account(&self, label: &str, files: &[&str]) -> (Arc<Account>, PathBuf) {
        let source = self.dir.path().join(format!("{label}-source"));
        for file in files {
            let file = source.join(file);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, vec![7u8; 4096]).unwrap();
        }
        let folder = self.dir.path().join(label);
        std::fs::create_dir(&folder).unwrap();
        let account = self.daemon.manager.add(label, &self.daemon.connection).await.unwrap();
        account.sync.register_root_without_interception(&folder).await.unwrap();
        account.sync.populate_from_directory(&source).await.unwrap();
        (account, folder)
    }

    async fn menu(&self, paths: &[&Path]) -> Menu {
        let paths: Vec<String> = paths.iter().map(|path| path.to_str().unwrap().to_owned()).collect();
        self.daemon.manager.menu(&paths).await
    }

    /// Pins `paths` and waits until `file`, which one of them covers, is downloaded.
    async fn pin_and_wait(&self, paths: &[&Path], file: &Path) {
        let paths: Vec<&str> = paths.iter().map(|path| path.to_str().unwrap()).collect();
        self.files.pin(&paths).await.unwrap();
        let files = &self.files;
        eventually("the pinned file is downloaded", || async move {
            files.item_state(file.to_str().unwrap()).await.unwrap() == "hydrated"
        })
        .await;
    }
}

/// Changes a mark on `path` by hand, whatever its mode.
fn with_write_bit(path: &Path, change: impl FnOnce()) {
    let mode = std::fs::metadata(path).unwrap().permissions().mode();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o200)).unwrap();
    change();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn names(menu: &Menu, folder: &Path) -> Vec<String> {
    menu.paths.iter().map(|path| Path::new(path).strip_prefix(folder).unwrap().display().to_string()).collect()
}

/// The answer, without its paths: (always-keep, free-up, blocked-by, open-online).
fn offered(menu: &Menu) -> (AlwaysKeep, Offer, &str, Offer) {
    (menu.always_keep, menu.free_up, menu.blocked_by.as_str(), menu.open_online)
}

/// `menu` agrees with what one account's `Pin`, `Unpin` and `FreeUp` check of its paths
/// before they change anything: `Pin` takes them all, `on-locked` is an `Unpin` that is
/// refused, and `disabled` a `FreeUp` that is.
async fn agrees(sync: &SyncService, menu: &Menu) {
    let paths: Vec<PathBuf> = menu.paths.iter().map(PathBuf::from).collect();
    if paths.is_empty() {
        assert_eq!((menu.always_keep, menu.free_up), (AlwaysKeep::Hidden, Offer::Hidden), "{menu:?}");
        return;
    }
    sync.check_pinnable(&paths).await.unwrap_or_else(|e| panic!("Pin refuses {menu:?}: {e}"));
    let unpin = sync.check_unpinnable(&paths).await;
    match menu.always_keep {
        AlwaysKeep::OnLocked => assert!(unpin.is_err(), "Unpin takes {menu:?}"),
        AlwaysKeep::On => assert!(unpin.is_ok(), "Unpin refuses {menu:?}: {unpin:?}"),
        AlwaysKeep::Off => {}
        AlwaysKeep::Hidden => panic!("hidden with paths: {menu:?}"),
    }
    let free = sync.check_free_up(&paths).await;
    match menu.free_up {
        Offer::Disabled => assert!(free.is_err(), "FreeUp takes {menu:?}"),
        Offer::Enabled => assert!(free.is_ok(), "FreeUp refuses {menu:?}: {free:?}"),
        Offer::Hidden => {}
    }
}

const HIDDEN: (AlwaysKeep, Offer, &str, Offer) = (AlwaysKeep::Hidden, Offer::Hidden, "", Offer::Hidden);

/// One item at a time: a file online-only, downloaded and pinned, a folder, and an item
/// OneDrive does not have yet.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_item_is_offered_what_its_state_allows() {
    let w = World::start().await;
    let (a, folder) = w.account("A", &["docs/a.bin", "c.bin", "d.bin"]).await;
    let (docs, c, d) = (folder.join("docs"), folder.join("c.bin"), folder.join("d.bin"));

    // Online-only: nothing to free up.
    let menu = w.menu(&[&c]).await;
    assert_eq!(names(&menu, &folder), ["c.bin"]);
    assert_eq!(offered(&menu), (AlwaysKeep::Off, Offer::Hidden, "", Offer::Enabled));
    assert_eq!(menu.open_online_path, c.to_str().unwrap());
    agrees(&a.sync, &menu).await;

    // Downloaded.
    w.files.hydrate(c.to_str().unwrap()).await.unwrap();
    let menu = w.menu(&[&c]).await;
    assert_eq!(offered(&menu), (AlwaysKeep::Off, Offer::Enabled, "", Offer::Enabled));
    agrees(&a.sync, &menu).await;

    // Pinned itself: checked, and it can be unchecked.
    w.pin_and_wait(&[&d], &d).await;
    let menu = w.menu(&[&d]).await;
    assert_eq!(offered(&menu), (AlwaysKeep::On, Offer::Enabled, "", Offer::Enabled));
    agrees(&a.sync, &menu).await;

    // A folder, downloaded or not: "Free up space" is offered. One filled from a
    // directory has no item id, so OneDrive has no page of it.
    let menu = w.menu(&[&docs]).await;
    assert_eq!(names(&menu, &folder), ["docs"]);
    assert_eq!(offered(&menu), (AlwaysKeep::Off, Offer::Enabled, "", Offer::Disabled));
    assert_eq!(menu.open_online_path, docs.to_str().unwrap());
    agrees(&a.sync, &menu).await;

    // A file OneDrive does not have yet: its page cannot be opened, and `WebUrl` says so.
    with_write_bit(&c, || xattr::remove(&c, "user.konedrive.item-id").unwrap());
    let menu = w.menu(&[&c]).await;
    assert_eq!(offered(&menu), (AlwaysKeep::Off, Offer::Enabled, "", Offer::Disabled));
    let refused = w.files.web_url(c.to_str().unwrap()).await.unwrap_err();
    assert_eq!(error_name(&refused), Some("org.konedrive.Error.NotUploaded"));

    // The answers hold: the calls go through.
    let both = [c.to_str().unwrap(), d.to_str().unwrap()];
    assert_eq!(w.files.unpin(&[both[1]]).await.unwrap(), 1);
    w.files.pin(&both).await.unwrap();
    assert_eq!(w.files.free_up(&both).await.unwrap().files, 2);
}

/// An item a folder above keeps pinned, with and without a pin of its own: checked and
/// locked, "Free up space" disabled, the folder named — unless the folder is selected too,
/// whose pin the same call takes off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_a_folder_above_keeps_pinned_is_locked_naming_the_folder() {
    let w = World::start().await;
    let (a, folder) = w.account("A", &["docs/a.bin", "docs/b.bin", "c.bin"]).await;
    let (docs, in_docs, b, c) = (folder.join("docs"), folder.join("docs/a.bin"), folder.join("docs/b.bin"), folder.join("c.bin"));
    w.pin_and_wait(&[&docs], &in_docs).await;
    w.pin_and_wait(&[&docs], &b).await;
    let locked = (AlwaysKeep::OnLocked, Offer::Disabled, "docs", Offer::Enabled);

    let menu = w.menu(&[&in_docs]).await;
    assert_eq!(offered(&menu), locked);
    agrees(&a.sync, &menu).await;
    let paths = [in_docs.to_str().unwrap()];
    assert_eq!(error_name(&w.files.unpin(&paths).await.unwrap_err()), Some("org.konedrive.Error.NotAllowed"));
    assert_eq!(error_name(&w.files.free_up(&paths).await.unwrap_err()), Some("org.konedrive.Error.NotAllowed"));

    // With a pin of its own as well: it would stay pinned by the folder either way.
    with_write_bit(&in_docs, || xattr::set(&in_docs, "user.konedrive.pin", b"1").unwrap());
    let menu = w.menu(&[&in_docs]).await;
    assert_eq!(offered(&menu), locked);
    agrees(&a.sync, &menu).await;

    // Several, one of them not pinned: unchecked, and checking it is a `Pin` that goes
    // through; "Free up space" would still be refused for the one the folder keeps.
    let menu = w.menu(&[&c, &b]).await;
    assert_eq!(names(&menu, &folder), ["c.bin", "docs/b.bin"]);
    assert_eq!(offered(&menu), (AlwaysKeep::Off, Offer::Disabled, "docs", Offer::Hidden));
    agrees(&a.sync, &menu).await;

    // The folder selected with what it keeps: its pin comes off in the same call, so
    // nothing is locked.
    let menu = w.menu(&[&docs, &b]).await;
    assert_eq!(offered(&menu), (AlwaysKeep::On, Offer::Enabled, "", Offer::Hidden));
    agrees(&a.sync, &menu).await;
    assert_eq!(w.files.unpin(&[docs.to_str().unwrap(), b.to_str().unwrap()]).await.unwrap(), 1);
}

/// What `Pin` does not take is not in `paths`, and alone is offered nothing: a path in no
/// folder, a symbolic link, a file of the user's own, a reserved name. An account's folder
/// itself is offered "Open in OneDrive" alone, and only by itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_pin_does_not_take_is_left_out() {
    let w = World::start().await;
    let (a, folder) = w.account("A", &["docs/a.bin", "c.bin"]).await;
    let (docs, c) = (folder.join("docs"), folder.join("c.bin"));
    let outside = w.dir.path().join("A-source/c.bin");
    let (link, mine, reserved) = (folder.join("link.bin"), folder.join("mine.txt"), folder.join(".konedrive-tmp"));
    std::os::unix::fs::symlink(&c, &link).unwrap();
    std::fs::write(&mine, b"mine").unwrap();
    std::fs::write(&reserved, b"").unwrap();

    for alone in [&outside, &link, &mine, &reserved] {
        let menu = w.menu(&[alone]).await;
        assert_eq!((names(&menu, &folder), offered(&menu)), (vec![], HIDDEN), "{}", alone.display());
        assert_eq!(menu.open_online_path, "");
        let refused = w.files.pin(&[alone.to_str().unwrap()]).await;
        assert!(refused.is_err(), "Pin takes {}", alone.display());
    }

    // Several of mixed kinds: the answer is about those `Pin` takes, in the order given.
    let menu = w.menu(&[&mine, &docs, &link, &outside, &c, &reserved]).await;
    assert_eq!(names(&menu, &folder), ["docs", "c.bin"]);
    assert_eq!(offered(&menu), (AlwaysKeep::Off, Offer::Enabled, "", Offer::Hidden));
    agrees(&a.sync, &menu).await;

    // The account's folder itself.
    let menu = w.menu(&[&folder]).await;
    assert_eq!((names(&menu, &folder), offered(&menu)), (vec![], (AlwaysKeep::Hidden, Offer::Hidden, "", Offer::Enabled)));
    assert_eq!(menu.open_online_path, folder.to_str().unwrap());
    let menu = w.menu(&[&folder, &c]).await;
    assert_eq!(names(&menu, &folder), ["c.bin"]);
    assert_eq!(offered(&menu), (AlwaysKeep::Off, Offer::Hidden, "", Offer::Hidden));
    // The calls themselves take that path; the menu does not offer them for it.
    assert_eq!(w.files.unpin(&[folder.to_str().unwrap()]).await.unwrap(), 0);
}

/// A selection over two accounts' folders is one answer: each account says its share, as
/// `Pin`, `Unpin` and `FreeUp` ask each account before anything changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_selection_over_two_accounts_is_one_answer() {
    let w = World::start().await;
    let (_a, in_a) = w.account("A", &["a.bin"]).await;
    let (_b, in_b) = w.account("B", &["docs/b.bin"]).await;
    let (a_file, b_docs, b_file) = (in_a.join("a.bin"), in_b.join("docs"), in_b.join("docs/b.bin"));
    let both = [b_file.to_str().unwrap(), a_file.to_str().unwrap()];

    let menu = w.menu(&[&b_file, &a_file]).await;
    assert_eq!(menu.paths, both);
    assert_eq!(offered(&menu), (AlwaysKeep::Off, Offer::Hidden, "", Offer::Hidden));

    // Both pinned, B's by its folder: locked for the whole selection, as the calls are
    // refused for the whole selection.
    w.pin_and_wait(&[&a_file], &a_file).await;
    w.pin_and_wait(&[&b_docs], &b_file).await;
    let menu = w.menu(&[&b_file, &a_file]).await;
    assert_eq!(menu.paths, both);
    assert_eq!(offered(&menu), (AlwaysKeep::OnLocked, Offer::Disabled, "docs", Offer::Hidden));
    assert_eq!(error_name(&w.files.unpin(&both).await.unwrap_err()), Some("org.konedrive.Error.NotAllowed"));
    assert_eq!(error_name(&w.files.free_up(&both).await.unwrap_err()), Some("org.konedrive.Error.NotAllowed"));
    assert_eq!(xattr::get(&a_file, "user.konedrive.pin").unwrap(), Some(b"1".to_vec()), "A's pin stayed");
}

/// `Files.Menu` on the bus: one call, every key, under its type.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_menu_is_one_call_on_the_bus() {
    let w = World::start().await;
    let (_a, folder) = w.account("A", &["c.bin"]).await;
    let (c, outside) = (folder.join("c.bin"), w.dir.path().join("A-source/c.bin"));

    let answer = w.files.menu(&[c.to_str().unwrap(), outside.to_str().unwrap()]).await.unwrap();
    assert_eq!(answer.len(), 6, "{answer:?}");
    assert_eq!(<Vec<String>>::try_from(answer["paths"].try_clone().unwrap()).unwrap(), [c.to_str().unwrap()]);
    assert_eq!(
        ["always-keep", "free-up", "blocked-by", "open-online", "open-online-path"].map(|key| text_of(&answer, key)),
        ["off", "hidden", "", "hidden", ""].map(str::to_owned)
    );
    // Never refused for a path it does not take, or for none.
    let answer = w.files.menu(&[]).await.unwrap();
    assert_eq!(text_of(&answer, "always-keep"), "hidden");
}

fn text_of(answer: &std::collections::HashMap<String, zbus::zvariant::OwnedValue>, key: &str) -> String {
    String::try_from(answer[key].try_clone().unwrap()).unwrap()
}
