use super::*;
use konedrive_tree::{Change, Kind, Placement, Row, Store, TreeStore};

/// An account on `hub` whose folder `dir` is registered without interception, with no
/// helper anywhere.
async fn account_at(hub: &Arc<HelperHub>, dir: &Path) -> Arc<SyncService> {
    let account = SyncService::on_hub(hub, None, None);
    hub.set_socket(dir.join("no-helper.sock"));
    std::fs::create_dir_all(dir).unwrap();
    account.register_root_without_interception(dir).await.unwrap();
    account
}

/// A file `name` in `dir` carrying item id `id`, opened as the helper hands one over.
fn opened(dir: &Path, name: &str, id: &str) -> OwnedFd {
    std::fs::write(dir.join(name), b"").unwrap();
    xattr::set(dir.join(name), XATTR_ITEM_ID, id.as_bytes()).unwrap();
    File::open(dir.join(name)).unwrap().into()
}

fn same(a: &Option<Arc<SyncService>>, b: &Arc<SyncService>) -> bool {
    a.as_ref().is_some_and(|a| Arc::ptr_eq(a, b))
}

/// Design §2.4, step 1 (test 5): the one account whose folder is on the file's
/// filesystem is the answer — a file moved out of its folder included. Needs a second
/// filesystem (`/dev/shm`) beside the temporary directory's.
#[tokio::test]
async fn the_one_folder_on_the_files_filesystem_is_the_answer() {
    let (Ok(here), Ok(there)) = (tempfile::tempdir(), tempfile::tempdir_in("/dev/shm")) else {
        return eprintln!("no /dev/shm: nothing to test");
    };
    if device_of(here.path()) == device_of(there.path()) {
        return eprintln!("/dev/shm is on the temporary directory's filesystem: nothing to test");
    }
    let hub = HelperHub::new();
    let a = account_at(&hub, &here.path().join("A")).await;
    let b = account_at(&hub, &there.path().join("B")).await;

    assert!(same(&hub.route(&opened(&here.path().join("A"), "f", "1")).await, &a));
    assert!(same(&hub.route(&opened(&there.path().join("B"), "f", "2")).await, &b));
    assert!(same(&hub.route(&opened(here.path(), "moved-out", "3")).await, &a), "by device alone");
}

/// Steps 2 and 3: two folders on one filesystem are told apart by the name the kernel
/// has for the file, verified by its inode — a file renamed while its open waits
/// included — and, for a file unlinked meanwhile, by its item id in a tree store. A
/// file in neither folder, whose id no store knows, is no one's.
#[tokio::test]
async fn two_folders_on_one_filesystem_are_told_apart_by_path_then_by_item_id() {
    let dir = tempfile::tempdir().unwrap();
    let (in_a, in_b) = (dir.path().join("A"), dir.path().join("B"));
    let hub = HelperHub::new();
    let a = account_at(&hub, &in_a).await;
    let b = account_at(&hub, &in_b).await;

    assert!(same(&hub.route(&opened(&in_a, "f", "1")).await, &a));
    let renamed = opened(&in_b, "f", "2");
    std::fs::rename(in_b.join("f"), in_b.join("g")).unwrap();
    assert!(same(&hub.route(&renamed).await, &b), "renamed while its open waited");

    // Both have a tree store; only B's knows the id.
    let store = |id: &str| {
        let row = Row {
            id: id.into(),
            parent_id: Some("ROOT".into()),
            name: "h".into(),
            kind: Kind::File,
            size: 0,
            mtime: 0,
            etag: None,
            ctag: None,
            quickxor: None,
            mime: None,
            placement: Placement::Placed,
        };
        let store = Store::new(TreeStore::in_memory().unwrap());
        konedrive_tree::off_runtime(|| store.call_blocking(move |s| s.commit_page(&[Change::Upsert(row)], "next"))).unwrap();
        store
    };
    *a.store.lock().unwrap() = Some(store("ITEM-A"));
    *b.store.lock().unwrap() = Some(store("ITEM-B"));
    let unlinked = opened(&in_b, "h", "ITEM-B");
    std::fs::remove_file(in_b.join("h")).unwrap();
    assert!(same(&hub.route(&unlinked).await, &b), "found by its item id");

    let nowhere = opened(dir.path(), "stray", "ITEM-X");
    assert!(hub.route(&nowhere).await.is_none(), "routing never guesses");
}

/// Write design §4.6, §8.5: the fill of an object that left an account's folder goes to that
/// account, by its item id — even from inside another account's folder, which its path says.
#[tokio::test]
async fn a_moved_out_object_is_routed_by_its_item_id() {
    let dir = tempfile::tempdir().unwrap();
    let (in_a, in_b) = (dir.path().join("A"), dir.path().join("B"));
    let hub = HelperHub::new();
    let a = account_at(&hub, &in_a).await;
    let b = account_at(&hub, &in_b).await;
    let moved = opened(&in_b, "came-from-a", "ITEM-A");
    assert!(same(&hub.route(&moved).await, &b), "by its path while nothing says otherwise");
    hub.set_moved_out(&Arc::downgrade(&a), HashSet::from(["ITEM-A".to_owned()]));
    assert!(same(&hub.route(&moved).await, &a), "by its item id");
    assert!(same(&hub.route(&opened(&in_b, "theirs", "ITEM-B")).await, &b));
    hub.set_moved_out(&Arc::downgrade(&a), HashSet::new());
    assert!(same(&hub.route(&moved).await, &b), "the row went");
}

/// An item id is another account's while that account's outbox waits to
/// fetch it, its tree store knows it, or the id names its drive; never an account's own.
#[tokio::test]
async fn another_accounts_item_ids_are_claimed() {
    let dir = tempfile::tempdir().unwrap();
    let hub = HelperHub::new();
    let a = account_at(&hub, &dir.path().join("A")).await;
    let b = account_at(&hub, &dir.path().join("B")).await;
    let (of_a, of_b) = (Arc::downgrade(&a), Arc::downgrade(&b));
    assert!(!hub.claimed_elsewhere(&of_b, "ITEM-A"), "nothing says so yet");
    hub.set_moved_out(&of_a, HashSet::from(["ITEM-A".to_owned()]));
    assert!(hub.claimed_elsewhere(&of_b, "ITEM-A"), "A's move out waits for it");
    assert!(!hub.claimed_elsewhere(&of_a, "ITEM-A"), "never one's own");

    let row = Row {
        id: "ITEM-S".into(),
        parent_id: Some("ROOT".into()),
        name: "s".into(),
        kind: Kind::File,
        size: 0,
        mtime: 0,
        etag: None,
        ctag: None,
        quickxor: None,
        mime: None,
        placement: Placement::Placed,
    };
    let store = Store::new(TreeStore::in_memory().unwrap());
    store
        .call(move |s| {
            s.commit_page(&[Change::Upsert(row)], "next")?;
            s.set_meta("drive_id", Some("abc123"))
        }).await
        .unwrap();
    *a.store.lock().unwrap() = Some(store);
    let claimed = |of: &Weak<SyncService>, id: &str| konedrive_tree::off_runtime(|| hub.claimed_elsewhere(of, id));
    assert!(claimed(&of_b, "ITEM-S"), "A's tree knows it");
    assert!(claimed(&of_b, "ABC123!42"), "the id names A's drive");
    assert!(!claimed(&of_b, "DEF456!42"));
    assert!(!claimed(&of_a, "ITEM-S"));
}

/// Review M3: one candidate by device is the answer only while every other account's
/// folder is placed. With another account's folder held back — its device unknown, and
/// the file could be in it — the one candidate is verified like two, and a file outside
/// its folder is no one's.
#[tokio::test]
async fn one_candidate_is_verified_while_another_folder_cannot_be_placed() {
    let dir = tempfile::tempdir().unwrap();
    let hub = HelperHub::new();
    let a = account_at(&hub, &dir.path().join("A")).await;
    assert!(same(&hub.route(&opened(dir.path(), "moved-out", "1")).await, &a), "unverified while all is placed");

    let config = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::config::ConfigStore::open(&crate::config::Paths::in_dir(config.path()), async { false }).await);
    let id = store.add_account("B").unwrap().id;
    let folder = crate::config::RootConfig {
        path: dir.path().join("B"),
        id: String::new(),
        intercepted: true,
        source: "local".into(),
        baloo_excluded: false,
        upgrade_when_helper: None,
    };
    store.set_root(&id, Some(folder)).unwrap();
    let b = SyncService::on_hub(&hub, None, Some(super::super::Persist { store, account: id }));
    b.hold_back("a test");

    assert!(same(&hub.route(&opened(&dir.path().join("A"), "f", "2")).await, &a), "verified by path");
    assert!(hub.route(&opened(dir.path(), "moved-out-too", "3")).await.is_none(), "not taken on trust");
}
