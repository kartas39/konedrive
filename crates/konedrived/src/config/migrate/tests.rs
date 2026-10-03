use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::config::is_valid_account_id;

const CLIENT: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

/// The wallet's answer, or `None` when the migration must not need to ask it.
async fn open(paths: &Paths, wallet: Option<bool>) -> ConfigStore {
    ConfigStore::open(paths, async move { wallet.expect("the wallet was asked, though a file already said there is an account") }).await
}

fn write_v1(paths: &Paths, text: &str) {
    std::fs::write(&paths.config_file, text).unwrap();
}

/// A tree store in WAL mode holding `rows` rows.
fn tree_store(path: &Path, rows: i64) -> rusqlite::Connection {
    let db = rusqlite::Connection::open(path).unwrap();
    db.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(())).unwrap();
    db.execute_batch("PRAGMA wal_autocheckpoint = 0; CREATE TABLE items (n INTEGER);").unwrap();
    for n in 0..rows {
        db.execute("INSERT INTO items VALUES (?1)", [n]).unwrap();
    }
    db
}

fn rows(path: &Path) -> i64 {
    rusqlite::Connection::open(path).unwrap().query_row("SELECT count(*) FROM items", [], |r| r.get(0)).unwrap()
}

#[test]
fn missing_file_loads_default() {
    let dir = tempfile::tempdir().unwrap();
    let config = V1Config::load(&dir.path().join("config.toml")).unwrap();
    assert_eq!(config, V1Config::default());
}

#[test]
fn save_creates_directories_and_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("config.toml");
    let config = V1Config { client_id: CLIENT.into(), ..V1Config::default() };
    config.save(&path).unwrap();
    assert_eq!(V1Config::load(&path).unwrap(), config);
}

/// The sync root travels with the client id in the same file, so both
/// have to survive a round trip — and a config written before the sync
/// sub-project existed has to keep loading, defaulting to the
/// fail-closed intercepted mode rather than to the mode that serves
/// zeros.
#[test]
fn the_sync_root_round_trips_and_an_older_config_still_loads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let config = V1Config {
        client_id: CLIENT.into(),
        sync_root: "/home/someone/OneDrive".into(),
        sync_root_intercepted: false,
        sync_root_id: "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d".into(),
        sync_root_source: "onedrive".into(),
        sync_root_baloo_excluded: true,
        sync_root_upgrade_when_helper: Some(true),
        sync_root_drive_id: "D1".into(),
    };
    config.save(&path).unwrap();
    assert_eq!(V1Config::load(&path).unwrap(), config);

    std::fs::write(&path, "client_id = \"0f8fad5b-d9cb-469f-a165-70867728950e\"\n").unwrap();
    let older = V1Config::load(&path).unwrap();
    assert_eq!(older.sync_root, "", "no root was persisted by that version");
    assert_eq!(older.sync_root_id, "", "nor its id");
    assert!(
        older.sync_root_intercepted,
        "a missing mode must read as the fail-closed one, not as the one that serves zeros"
    );
    assert!(
        !older.sync_root_baloo_excluded,
        "a config from before this existed must not have Baloo settings taken off it"
    );

    // A folder recorded by part 1, before a folder could show OneDrive,
    // was filled from a directory: it is a local one.
    std::fs::write(&path, "sync_root = \"/home/someone/Offline\"\nsync_root_intercepted = false\n").unwrap();
    assert_eq!(V1Config::load(&path).unwrap().sync_root_source, "local");
    assert_eq!(V1Config::default().sync_root_source, "local");
}

/// Ruling 4: a config written before the switch flag existed
/// cannot say why its root is without interception, and is read as one
/// to switch when the helper connects — the user's own folder is that
/// case. An intercepted root has nothing to switch. A written flag is
/// what it says.
#[test]
fn a_missing_switch_flag_reads_as_switch_only_for_a_root_without_interception() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");

    std::fs::write(&path, "sync_root = \"/home/someone/tools/test\"\nsync_root_intercepted = false\n").unwrap();
    let unintercepted = V1Config::load(&path).unwrap();
    assert_eq!(unintercepted.sync_root_upgrade_when_helper, None);
    assert!(unintercepted.sync_root_upgrades_when_helper());

    std::fs::write(&path, "sync_root = \"/home/someone/OneDrive\"\n").unwrap();
    assert!(!V1Config::load(&path).unwrap().sync_root_upgrades_when_helper());

    let written = V1Config { sync_root_intercepted: false, sync_root_upgrade_when_helper: Some(false), ..V1Config::default() };
    written.save(&path).unwrap();
    assert!(!V1Config::load(&path).unwrap().sync_root_upgrades_when_helper(), "a choice is kept");
    assert!(V1Config::default().save(&path).is_ok());
    assert!(
        !std::fs::read_to_string(&path).unwrap().contains("sync_root_upgrade_when_helper"),
        "no root, no flag written"
    );
}

/// §7.2: today's file, with its folder, becomes account #1; version 1 is kept, private
/// and byte for byte, in `config.toml.v1`; a restart does not migrate again.
#[tokio::test]
async fn a_version_1_folder_becomes_account_1() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let v1 = format!(
        "client_id = \"{CLIENT}\"\nsync_root = \"/home/ann/OneDrive\"\nsync_root_intercepted = true\n\
         sync_root_id = \"1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d\"\nsync_root_source = \"onedrive\"\n\
         sync_root_baloo_excluded = true\nsync_root_drive_id = \"D1A2B3C4\"\n"
    );
    write_v1(&paths, &v1);

    let store = open(&paths, None).await;
    let config = store.snapshot();
    assert_eq!(config.client_id, CLIENT);
    let [account] = config.accounts.as_slice() else { panic!("one account: {config:?}") };
    assert!(is_valid_account_id(&account.id), "{}", account.id);
    assert_eq!(
        account,
        &AccountConfig {
            id: account.id.clone(),
            label: "Personal".into(),
            mode: Mode::ReadOnly,
            origin: Origin::Migrated,
            drive_id: "D1A2B3C4".into(),
            login_hint: String::new(),
            legacy_token: true,
            migrate_files: true,
            root: Some(RootConfig {
                path: "/home/ann/OneDrive".into(),
                id: "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d".into(),
                intercepted: true,
                source: "onedrive".into(),
                baloo_excluded: true,
                upgrade_when_helper: None,
            }),
            ignore: None,
            machine_name: String::new(),
            thumbnails: None,
            old_pause_on_metered: None,
            old_on_battery: None,
        }
    );
    assert_eq!(store.last_error(), "");

    let copy = v1_copy(&paths.config_file);
    assert_eq!(std::fs::read_to_string(&copy).unwrap(), v1);
    assert_eq!(std::fs::metadata(&copy).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(std::fs::read_to_string(&paths.config_file).unwrap().starts_with("config_version = 2\n"));
    assert_eq!(open(&paths, None).await.snapshot(), config, "a restart loads version 2 as it is");
}

/// §7.2 step 2: what makes version 1 carry an account over, and what it carries.
#[tokio::test]
async fn what_version_1_carries_over() {
    let client = format!("client_id = \"{CLIENT}\"\n");
    let local = format!("{client}sync_root = \"/home/ann/Offline\"\nsync_root_intercepted = false\n");
    let local_root = RootConfig {
        path: "/home/ann/Offline".into(),
        id: String::new(),
        intercepted: false,
        source: "local".into(),
        baloo_excluded: false,
        upgrade_when_helper: None,
    };
    /// The case, the file, the files beside it, the wallet's answer, and the account's
    /// folder when an account is carried over.
    type Case<'a> = (&'a str, &'a str, &'a [&'a str], Option<bool>, Option<Option<RootConfig>>);
    let cases: [Case; 5] = [
        ("a client id alone", &client, &[], Some(false), None),
        ("a refresh token, or a wallet that does not answer", &client, &[], Some(true), Some(None)),
        ("the cached account", &client, &["account.json"], None, Some(None)),
        ("the tree store", &client, &["tree.sqlite"], None, Some(None)),
        ("a local folder from before the defaults", &local, &[], None, Some(Some(local_root.clone()))),
    ];
    for (case, text, files, wallet, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::in_dir(dir.path());
        write_v1(&paths, text);
        for file in files {
            std::fs::write(dir.path().join(file), "").unwrap();
        }
        let config = open(&paths, wallet).await.snapshot();
        assert_eq!(config.client_id, CLIENT, "{case}");
        assert_eq!(config.accounts.first().map(|a| a.root.clone()), expected, "{case}");
        assert!(config.accounts.len() <= 1, "{case}");
        assert!(v1_copy(&paths.config_file).exists(), "{case}");
    }
    assert!(local_root.upgrades_when_helper(), "Ruling 4 carries over");

    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let store = open(&paths, None).await;
    assert_eq!(store.snapshot(), Config::default(), "no file: a fresh start");
    assert!(!paths.config_file.exists() && !v1_copy(&paths.config_file).exists());
}

/// §7.3's crash table: the moves are finished by whichever start comes next — after a
/// crash right after the commit, between the two moves, or before the flag was cleared.
#[tokio::test]
async fn the_file_moves_finish_whatever_step_a_crash_stopped_them_at() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    write_v1(&paths, &format!("client_id = \"{CLIENT}\"\n"));
    std::fs::write(&paths.account_cache, "{\"cached\":1}").unwrap();
    drop(tree_store(&paths.tree_db, 3));

    // A crash right after the commit: version 2, and the files where version 1 kept them.
    let id = open(&paths, None).await.snapshot().accounts[0].id.clone();
    let to = paths.account(&id).unwrap();
    assert!(paths.account_cache.exists() && paths.tree_db.exists());
    // A crash between the two moves: the cache moved, the store not.
    std::fs::create_dir_all(&to.dir).unwrap();
    std::fs::rename(&paths.account_cache, &to.account_cache).unwrap();

    let store = open(&paths, None).await;
    finish_file_moves(&store, &paths);
    assert!(!paths.account_cache.exists() && !paths.tree_db.exists());
    assert_eq!(std::fs::read_to_string(&to.account_cache).unwrap(), "{\"cached\":1}");
    assert_eq!(rows(&to.tree_db), 3);
    assert!(!store.account(&id).unwrap().migrate_files);
    assert!(!open(&paths, None).await.account(&id).unwrap().migrate_files, "cleared on disk");

    // A crash before the flag was cleared: nothing left to move, and the flag goes.
    store
        .update_account(&id, |a| {
            a.migrate_files = true;
            Ok::<_, ConfigError>(())
        })
        .unwrap();
    finish_file_moves(&store, &paths);
    assert!(!store.account(&id).unwrap().migrate_files);
    assert_eq!(rows(&to.tree_db), 3);
    assert_eq!(store.last_error(), "");
}

/// A store its daemon never closed — rows committed to the write-ahead log and never
/// checkpointed — keeps every row through the move.
#[tokio::test]
async fn the_store_move_keeps_rows_committed_to_the_write_ahead_log() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    write_v1(&paths, &format!("client_id = \"{CLIENT}\"\n"));
    let live = tempfile::tempdir().unwrap();
    let writer = tree_store(&live.path().join("tree.sqlite"), 5);
    // What a crash leaves: the database and its log, as they are on disk mid-run.
    for suffix in ["", "-wal"] {
        std::fs::copy(live.path().join(format!("tree.sqlite{suffix}")), with_suffix(&paths.tree_db, suffix)).unwrap();
    }
    drop(writer);
    assert!(std::fs::metadata(with_suffix(&paths.tree_db, "-wal")).unwrap().len() > 0, "the rows are in the log");

    let store = open(&paths, None).await;
    finish_file_moves(&store, &paths);
    let to = paths.account(&store.snapshot().accounts[0].id).unwrap();
    for suffix in ["", "-wal", "-shm"] {
        assert!(!with_suffix(&paths.tree_db, suffix).exists(), "tree.sqlite{suffix} is gone from the old place");
    }
    assert_eq!(rows(&to.tree_db), 5);
    assert_eq!(std::fs::metadata(&to.dir).unwrap().permissions().mode() & 0o777, 0o700, "private, like account.json");
}

/// A store that cannot be moved safely — the target exists, or another process still
/// has it open — is left where it is; the flag is cleared all the same, and the account
/// starts a store of its own.
#[tokio::test]
async fn a_store_that_cannot_be_moved_is_left_where_it_is() {
    for case in ["the target exists", "another process has it open"] {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::in_dir(dir.path());
        write_v1(&paths, &format!("client_id = \"{CLIENT}\"\n"));
        let holder = tree_store(&paths.tree_db, 2);
        let store = open(&paths, None).await;
        let id = store.snapshot().accounts[0].id.clone();
        let to = paths.account(&id).unwrap();
        if case == "the target exists" {
            drop(holder);
            std::fs::create_dir_all(&to.dir).unwrap();
            std::fs::write(&to.tree_db, "newer").unwrap();
            finish_file_moves(&store, &paths);
            assert_eq!(std::fs::read_to_string(&to.tree_db).unwrap(), "newer", "{case}");
        } else {
            finish_file_moves(&store, &paths);
            assert!(!to.tree_db.exists(), "{case}");
            drop(holder);
        }
        assert_eq!(rows(&paths.tree_db), 2, "{case}: left where it is, whole");
        assert!(!store.account(&id).unwrap().migrate_files, "{case}");
        assert_eq!(store.last_error(), "", "{case}: expected, not a failure");
    }
}

/// Issue #95: the accounts' own `pause_on_metered` and `on_battery` become the global
/// keys, the strictest value winning — an account without a key counting as its
/// default — and leave the accounts; the file is written once, and a second start
/// changes nothing.
#[tokio::test]
async fn the_accounts_hold_settings_move_to_the_global_keys_once() {
    let account = |id: &str, keys: &str| format!("[[accounts]]\nid = \"{id}\"\nlabel = \"L{id}\"\n{keys}");
    for (keys, pause_on_metered, on_battery) in [
        (["pause_on_metered = false\non_battery = \"sync\"\n", "pause_on_metered = false\non_battery = \"pause\"\n"], false, "pause"),
        (["pause_on_metered = false\non_battery = \"sync\"\n", ""], true, "power-saver"),
        (["on_battery = \"sync\"\n", "on_battery = \"whenever\"\n"], true, "power-saver"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::in_dir(dir.path());
        let text = format!("config_version = 2\n{}{}", account("3f9a1c0e5b7d", keys[0]), account("8c21d07a44e1", keys[1]));
        std::fs::write(&paths.config_file, &text).unwrap();
        let store = open(&paths, None).await;
        move_hold_settings(&store);
        let config = store.snapshot();
        assert_eq!((config.pause_on_metered, config.on_battery.as_deref()), (Some(pause_on_metered), Some(on_battery)), "{keys:?}");
        let written = std::fs::read_to_string(&paths.config_file).unwrap();
        assert_eq!(toml::from_str::<Config>(&written).unwrap(), config, "{keys:?}");
        for entry in written.split("[[accounts]]").skip(1) {
            assert!(!entry.contains("pause_on_metered") && !entry.contains("on_battery"), "{keys:?}: {written}");
        }
        assert_eq!(store.last_error(), "");

        let again = open(&paths, None).await;
        move_hold_settings(&again);
        assert_eq!(std::fs::read_to_string(&paths.config_file).unwrap(), written, "{keys:?}: the second start changes nothing");
    }
}

/// An unreadable file is left to its own error: the move neither writes nor adds one.
#[tokio::test]
async fn an_unreadable_file_is_not_moved() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let text = "config_version = 2\n[[accounts]]\nid = \"3f9a1c0e5b7d\"\non_battery = \"pause\"\nthis is not [toml\n";
    std::fs::write(&paths.config_file, text).unwrap();
    let store = open(&paths, None).await;
    assert!(store.is_poisoned());
    let parse_error = store.last_error();
    move_hold_settings(&store);
    assert_eq!(store.last_error(), parse_error);
    assert_eq!(std::fs::read_to_string(&paths.config_file).unwrap(), text);
}

/// Keys left in an account by a move whose write failed are taken by a later
/// `SetPauseOnMetered` / `SetOnBattery`, so the next start's move keeps the user's choice.
#[tokio::test]
async fn a_global_set_takes_the_keys_left_in_the_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let text = "config_version = 2\n[[accounts]]\nid = \"3f9a1c0e5b7d\"\nlabel = \"Personal\"\npause_on_metered = true\non_battery = \"pause\"\n";
    std::fs::write(&paths.config_file, text).unwrap();
    let store = open(&paths, None).await;
    store.set_on_battery(OnBattery::Sync).unwrap();
    store.set_pause_on_metered(false).unwrap();

    let again = open(&paths, None).await;
    move_hold_settings(&again);
    let config = again.snapshot();
    assert_eq!((config.pause_on_metered, config.on_battery.as_deref()), (Some(false), Some("sync")));
    let written = std::fs::read_to_string(&paths.config_file).unwrap();
    for entry in written.split("[[accounts]]").skip(1) {
        assert!(!entry.contains("pause_on_metered") && !entry.contains("on_battery"), "{written}");
    }
}

/// A file whose accounts have neither key is not written.
#[tokio::test]
async fn nothing_to_move_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let text = "config_version = 2\n\n[[accounts]]\nid = \"3f9a1c0e5b7d\"\nlabel = \"Personal\"\n";
    std::fs::write(&paths.config_file, text).unwrap();
    let store = open(&paths, None).await;
    move_hold_settings(&store);
    assert_eq!(std::fs::read_to_string(&paths.config_file).unwrap(), text);
    assert_eq!((store.snapshot().pause_on_metered, store.snapshot().on_battery), (None, None));
}
