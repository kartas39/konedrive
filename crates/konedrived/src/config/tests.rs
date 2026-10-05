use std::path::Path;

use super::*;

const CLIENT: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

async fn open(paths: &Paths) -> ConfigStore {
    ConfigStore::open(paths, async { false }).await
}

fn id(text: &str) -> AccountId {
    AccountId::new(text)
}

fn drive(text: &str) -> DriveId {
    DriveId::new(text).unwrap()
}

fn account(id: &str, label: &str) -> AccountConfig {
    AccountConfig::new(AccountId::new(id), label, Origin::Added)
}

/// Whether `TokenExport.ReadWrite` may hand a token out for the account's drive, as the file
/// says now.
fn export_allowed(store: &ConfigStore, id: &AccountId) -> bool {
    store.current().is_some_and(|config| {
        config.account(id).and_then(|account| account.drive_id.as_ref()).is_some_and(|drive| config.read_write_export_allowed(drive))
    })
}

#[test]
fn a_client_id_is_a_guid_in_any_case() {
    assert!(is_valid_client_id("0f8fad5b-d9cb-469f-a165-70867728950e"));
    assert!(is_valid_client_id("0F8FAD5B-D9CB-469F-A165-70867728950E"));
    for bad in [
        "",
        "not-a-guid",
        "0f8fad5b-d9cb-469f-a165-70867728950",
        "0f8fad5bd9cb469fa16570867728950e",
        "0f8fad5b-d9cb-469f-a165-70867728950g",
        "0f8fad5b-d9cb-469f-a165-70867728950e-1",
    ] {
        assert!(!is_valid_client_id(bad), "{bad:?} should be rejected");
    }
}

#[test]
fn in_dir_places_every_file_in_the_directory() {
    let paths = Paths::in_dir(Path::new("/tmp/x"));
    assert_eq!(paths.config_file, Path::new("/tmp/x/config.toml"));
    assert_eq!(paths.account_cache, Path::new("/tmp/x/account.json"));
    assert_eq!(paths.tree_db, Path::new("/tmp/x/tree.sqlite"));
    assert_eq!(paths.rescue_dir, Path::new("/tmp/x/rescued"));
    assert_eq!(paths.thumbnails, Path::new("/tmp/x/thumbnails"));
}

/// Each account's state in its own directory, its rescues in their own; and an id that
/// is not an account id names no path at all.
#[test]
fn each_account_has_its_own_files() {
    let paths = Paths::in_dir(Path::new("/tmp/x"));
    assert_eq!(
        paths.account(&id("3f9a1c0e5b7d")),
        Some(AccountPaths {
            dir: "/tmp/x/accounts/3f9a1c0e5b7d".into(),
            account_cache: "/tmp/x/accounts/3f9a1c0e5b7d/account.json".into(),
            tree_db: "/tmp/x/accounts/3f9a1c0e5b7d/tree.sqlite".into(),
            rescue_dir: "/tmp/x/rescued/3f9a1c0e5b7d".into(),
        })
    );
    for bad in ["", "..", "../../etc/xx", "3F9A1C0E5B7D", "3f9a1c0e5b7", "my-account"] {
        assert_eq!(paths.account(&id(bad)), None, "{bad:?}");
    }
}

#[test]
fn labels_follow_the_rules() {
    let config = Config { accounts: vec![account("3f9a1c0e5b7d", "Personal")], ..Config::default() };
    assert_eq!(check_label("  Family ", &config, None), Ok("Family".into()));
    assert!(check_label(&"é".repeat(40), &config, None).is_ok(), "40 characters, not bytes");
    let bad_labels = ["", "   ", &"x".repeat(41), "Home/Work", "tab\there", "PERSONAL", "8C21D07A44E1"];
    for bad in bad_labels {
        assert!(check_label(bad, &config, None).is_err(), "{bad:?} should be refused");
    }
    assert_eq!(
        check_label("PERSONAL", &config, Some(&id("3f9a1c0e5b7d"))),
        Ok("PERSONAL".into()),
        "an account may change the case of its own label"
    );
    // An account is commonly named by its email.
    assert_eq!(check_label("ann@outlook.com", &config, None), Ok("ann@outlook.com".into()));
}

/// `[transfers] max`: 64 when missing, clamped into 1–256.
#[test]
fn the_transfer_ceiling_is_read_and_clamped() {
    let read = |text: &str| toml::from_str::<Config>(&format!("config_version = 2\n{text}")).unwrap().transfer_ceiling();
    assert_eq!(read(""), konedrive_graph::pool::DEFAULT_CEILING);
    assert_eq!(read("[transfers]\nmax = 20"), 20);
    assert_eq!(read("[transfers]\nmax = 0"), 1);
    assert_eq!(read("[transfers]\nmax = 1000"), 256);
}

/// `[transfers] large`: 4 when missing, clamped into 1…`max`.
#[test]
fn the_large_file_limit_is_read_and_clamped() {
    let read = |text: &str| toml::from_str::<Config>(&format!("config_version = 2\n{text}")).unwrap().transfer_large();
    assert_eq!(read(""), 4);
    assert_eq!(read("[transfers]\nlarge = 2"), 2);
    assert_eq!(read("[transfers]\nlarge = 0"), 1);
    assert_eq!(read("[transfers]\nmax = 8\nlarge = 20"), 8);
    assert_eq!(read("[transfers]\nmax = 2"), 2, "the default never above the ceiling");
}

/// The hold's two settings are global keys, absent until set and read as
/// their defaults then (an unknown `on_battery` as `power-saver`); each setter writes its
/// own key at the top of the file and leaves the accounts alone.
#[tokio::test]
async fn the_hold_settings_are_global_keys() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let store = open(&paths).await;
    let id = store.add_account("Personal").unwrap().id;
    let config = store.snapshot();
    assert_eq!((config.pause_on_metered, config.on_battery.clone()), (None, None));
    assert_eq!((config.pauses_on_metered(), config.on_battery()), (true, OnBattery::PowerSaver));

    store.set_pause_on_metered(false).unwrap();
    store.set_on_battery(OnBattery::Pause).unwrap();
    let text = std::fs::read_to_string(&paths.config_file).unwrap();
    let top = text.split("[[accounts]]").next().unwrap();
    assert!(top.contains("pause_on_metered = false") && top.contains("on_battery = \"pause\""), "{text}");
    let on_disk: Config = toml::from_str(&text).unwrap();
    assert_eq!((on_disk.pauses_on_metered(), on_disk.on_battery()), (false, OnBattery::Pause));
    let account = on_disk.account(&id).unwrap();
    assert_eq!((account.old_pause_on_metered, account.old_on_battery.clone()), (None, None));

    std::fs::write(&paths.config_file, text.replace("\"pause\"", "\"whenever\"")).unwrap();
    assert_eq!(store.current().unwrap().on_battery(), OnBattery::PowerSaver, "an unknown value falls back");
}

/// §3.1: a hand-edited file whose accounts collide loads every account, holds each
/// later one that collides, and is not rewritten.
#[tokio::test]
async fn colliding_accounts_are_held_and_the_file_is_not_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let text = r#"config_version = 2
client_id = ""

[[accounts]]
id = "3f9a1c0e5b7d"
label = "Personal"
drive_id = "D1"
[accounts.root]
path = "/home/ann/OneDrive"
id = "R1"

[[accounts]]
id = "3f9a1c0e5b7d"
label = "Same id"

[[accounts]]
id = "000000000002"
label = "personal"

[[accounts]]
id = "000000000003"
label = "Same drive"
drive_id = "D1"

[[accounts]]
id = "000000000004"
label = "Same root id"
[accounts.root]
path = "/home/ann/Elsewhere"
id = "R1"

[[accounts]]
id = "000000000005"
label = "Inside"
[accounts.root]
path = "/home/ann/OneDrive/Family"
id = "R5"

[[accounts]]
id = "my-account"
label = "Not an id"

[[accounts]]
id = "000000000007"
label = "Fine"
drive_id = "D7"
[accounts.root]
path = "/home/ann/OneDrive-Fine"
id = "R7"
"#;
    std::fs::write(&paths.config_file, text).unwrap();
    let store = open(&paths).await;
    let config = store.snapshot();
    assert_eq!(config.accounts.len(), 8, "every account is loaded");
    let holds = config.holds();
    let expected = [None, Some("id is also"), Some("label"), Some("same Microsoft account"), Some("root id"), Some("inside"), Some("not 12"), None];
    for ((account, held), expected) in config.accounts.iter().zip(&holds).zip(expected) {
        match (held, expected) {
            (None, None) => {}
            (Some(why), Some(words)) => assert!(why.contains(words), "{}: {why}", account.label),
            _ => panic!("{}: held {held:?}, expected {expected:?}", account.label),
        }
    }
    assert_eq!(
        holds[6].as_deref(),
        Some("its id \"my-account\" is not 12 lowercase hexadecimal characters"),
        "the sentence Accounts.LastError says, with the id as plain quoted text"
    );
    assert_eq!(std::fs::read_to_string(&paths.config_file).unwrap(), text);
    assert_eq!(store.last_error(), "");
}

/// Both modes load as written; one this version does not know loads as read-only, and the
/// next write says so. An unknown origin reads as the protected one.
#[tokio::test]
async fn an_unknown_mode_loads_as_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    std::fs::write(
        &paths.config_file,
        "config_version = 2\n\
         [[accounts]]\nid = \"3f9a1c0e5b7d\"\nlabel = \"Personal\"\nmode = \"read-write-all\"\norigin = \"imported\"\n\
         [[accounts]]\nid = \"8c21d07a44e1\"\nlabel = \"Test\"\nmode = \"read-write\"\norigin = \"added\"\n",
    )
    .unwrap();
    let store = open(&paths).await;
    let loaded = store.account(&id("3f9a1c0e5b7d")).unwrap();
    assert_eq!((loaded.mode, loaded.origin), (Mode::ReadOnly, Origin::Migrated));
    assert_eq!(store.account(&id("8c21d07a44e1")).unwrap().mode, Mode::ReadWrite);
    store.set_label(&id("3f9a1c0e5b7d"), "Home").unwrap();
    let text = std::fs::read_to_string(&paths.config_file).unwrap();
    assert!(text.contains("mode = \"read-only\"") && text.contains("origin = \"migrated\""), "{text}");
    assert!(text.contains("mode = \"read-write\""), "{text}");
    assert_eq!(Mode::parse("rw"), None);
}

/// `write_test_drive_ids` is still read, and decides one thing: which drives the read-write
/// token export is handed out for. Empty, as it is by default, it refuses that export for
/// every drive and nothing else: what the file says of an account's writes — its mode and
/// its recorded drive — is the same with the list empty, and with the drive on it. An
/// account never signed in (no drive) is exported for by no list, whatever a hand wrote
/// into it.
#[tokio::test]
async fn the_list_refuses_the_read_write_token_export_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&Paths::in_dir(dir.path())).await;
    let real = store.add_account("Personal").unwrap().id;
    let test = store.add_account("Test").unwrap().id;
    let fresh = store.add_account("New").unwrap().id;
    store.record_drive(&real, &drive("REAL")).unwrap();
    store.record_drive(&test, &drive("TEST")).unwrap();
    store
        .update(|c| {
            c.account_mut(&real).unwrap().mode = Mode::ReadWrite;
            c.account_mut(&fresh).unwrap().mode = Mode::ReadWrite;
            Ok::<_, ConfigError>(())
        })
        .unwrap();
    assert!(store.snapshot().write_test_drive_ids.is_empty(), "empty by default");
    for id in [&real, &test, &fresh] {
        assert!(!export_allowed(&store, id), "{id}: no token is exported while the list is empty");
    }
    // The mode is the user's choice, whatever the list says: an unlisted drive may be written to.
    let standing = |id: &AccountId| store.write_standing(id);
    assert_eq!(standing(&real), Some(WriteStanding { mode: Mode::ReadWrite, recorded_drive: Some(drive("REAL")) }));
    assert!(standing(&real).unwrap().allows_writes());
    assert_eq!(standing(&test), Some(WriteStanding { mode: Mode::ReadOnly, recorded_drive: Some(drive("TEST")) }));
    assert!(!standing(&test).unwrap().allows_writes(), "read-only is read-only");
    assert_eq!(standing(&fresh), Some(WriteStanding { mode: Mode::ReadWrite, recorded_drive: None }));
    assert!(!standing(&fresh).unwrap().allows_writes(), "an account with no recorded drive is read-only");
    assert_eq!(standing(&id("000000000000")), None, "no such account");

    // A hand edit of the list counts at once, for the export alone.
    let empty = std::fs::read_to_string(store.file()).unwrap();
    let listed = format!("write_test_drive_ids = [\"TEST\", \"\"]\n{empty}");
    std::fs::write(store.file(), &listed).unwrap();
    assert!(export_allowed(&store, &test));
    assert!(!export_allowed(&store, &real), "a drive not listed is not exported for");
    assert!(!export_allowed(&store, &fresh), "an empty entry lets no account without a drive through");
    assert!(standing(&real).unwrap().allows_writes() && !standing(&test).unwrap().allows_writes(), "the list changes no mode");
    // The empty entry names no drive: it is not kept, and is gone at the next write.
    store.set_label(&fresh, "Newer").unwrap();
    let written = std::fs::read_to_string(store.file()).unwrap();
    assert!(written.contains("write_test_drive_ids = [\"TEST\"]\n"), "{written}");
    assert!(export_allowed(&store, &test), "the listed drive stays");

    std::fs::write(store.file(), &empty).unwrap();
    assert!(!export_allowed(&store, &test), "the drive taken off the list by hand");
    std::fs::write(store.file(), "config_version = 2\nthis is not [toml\n").unwrap();
    assert_eq!(store.write_standing(&real), None, "an unreadable file fails closed");
    assert!(!export_allowed(&store, &test));
}

/// A list that is not a list makes the whole file unreadable: the store is poisoned, no
/// account loads, and nothing is writable.
#[tokio::test]
async fn a_malformed_write_list_lets_nothing_through() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    std::fs::write(
        &paths.config_file,
        "config_version = 2\nwrite_test_drive_ids = \"D1\"\n\n[[accounts]]\nid = \"3f9a1c0e5b7d\"\nlabel = \"Test\"\nmode = \"read-write\"\ndrive_id = \"D1\"\n",
    )
    .unwrap();
    let store = open(&paths).await;
    assert!(store.is_poisoned());
    assert!(store.snapshot().accounts.is_empty(), "no account loads");
    assert_eq!(store.write_standing(&id("3f9a1c0e5b7d")), None);
}

/// A drive is one account, however it comes to be recorded (design §8.2): a
/// drive another account has is refused, one of the account's own is kept.
#[tokio::test]
async fn a_drive_another_account_has_is_not_recorded_again() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&Paths::in_dir(dir.path())).await;
    let (a, b) = (store.add_account("A").unwrap().id, store.add_account("B").unwrap().id);
    assert_eq!(store.record_drive(&a, &drive("DA")).unwrap(), "DA");
    assert_eq!(store.record_drive(&b, &drive("DA")), Err(ConfigError::DriveTaken("A".into())));
    assert_eq!(store.account(&b).unwrap().drive_id, None, "nothing recorded");
    assert_eq!(store.record_drive(&a, &drive("DB")).unwrap(), "DA", "the drive recorded first stays");
}

/// F37: every write goes through one lock, so writers from many threads each keep
/// their change; a fresh account gets its own valid id.
#[tokio::test]
async fn writers_never_save_over_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let store = open(&paths).await;
    assert!(!paths.config_file.exists(), "a missing file is not written at load");
    std::thread::scope(|s| {
        for i in 0..8 {
            let store = &store;
            s.spawn(move || store.add_account(&format!("Account {i}")).unwrap());
        }
    });
    let on_disk: Config = toml::from_str(&std::fs::read_to_string(&paths.config_file).unwrap()).unwrap();
    assert_eq!(on_disk, store.snapshot());
    let mut ids: Vec<&AccountId> = on_disk.accounts.iter().map(|a| &a.id).collect();
    assert!(ids.iter().all(|id| id.is_valid()), "{ids:?}");
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 8);
    assert!(on_disk.accounts.iter().all(|a| a.origin == Origin::Added && a.mode == Mode::ReadOnly));
    assert_eq!(store.add_account("account 0"), Err(ConfigError::InvalidLabel("the label \"Account 0\" is already used".into())));
}

/// Each write starts from the file as it is — a hand edit made meanwhile is kept — and
/// writes nothing when the change is refused, or when the file can no longer be read.
#[tokio::test]
async fn every_write_starts_from_the_file_and_never_overwrites_what_it_cannot_read() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    let store = open(&paths).await;
    let id = store.add_account("Personal").unwrap().id;

    let mut edited = store.snapshot();
    edited.client_id = CLIENT.into();
    std::fs::write(&paths.config_file, toml::to_string(&edited).unwrap()).unwrap();
    assert_eq!(store.record_drive(&id, &drive("D1")), Ok(drive("D1")));
    assert_eq!(store.client_id(), CLIENT, "the hand edit is kept");
    assert_eq!(store.record_drive(&id, &drive("D2")), Ok(drive("D1")), "a drive, once recorded, stays");
    let root = RootConfig {
        path: "/home/ann/OneDrive".into(),
        id: "R1".into(),
        intercepted: true,
        source: "onedrive".into(),
        baloo_excluded: false,
        upgrade_when_helper: Some(false),
    };
    store.set_root(&id, Some(root.clone())).unwrap();
    let on_disk: Config = toml::from_str(&std::fs::read_to_string(&paths.config_file).unwrap()).unwrap();
    assert_eq!(on_disk.account(&id).map(|a| (a.drive_id.clone(), a.root.clone())), Some((Some(drive("D1")), Some(root))));

    let before = std::fs::read_to_string(&paths.config_file).unwrap();
    let refused: Result<(), ConfigError> = store.update(|config| {
        config.client_id.clear();
        Err(ConfigError::NoAccount(AccountId::new("x")))
    });
    assert!(refused.is_err());
    let nobody = AccountId::new("000000000000");
    assert_eq!(store.record_drive(&nobody, &drive("D9")), Err(ConfigError::NoAccount(nobody.clone())));
    assert_eq!(std::fs::read_to_string(&paths.config_file).unwrap(), before, "nothing written");

    std::fs::write(&paths.config_file, "config_version = 2\nthis is not [toml\n").unwrap();
    assert!(matches!(store.set_client_id(CLIENT), Err(ConfigError::Unreadable(_))));
    std::fs::write(&paths.config_file, "config_version = 3\n").unwrap();
    assert!(matches!(store.remove_account(&id), Err(ConfigError::Unreadable(_))));
    assert_eq!(std::fs::read_to_string(&paths.config_file).unwrap(), "config_version = 3\n");
}

/// §7.2 step 1: a file that cannot be read — or that a newer version wrote — loads no
/// account, is neither migrated nor written, and `LastError` names it.
#[tokio::test]
async fn an_unreadable_or_newer_file_poisons_the_store() {
    for text in [
        "sync_root = \"/home/u/OneDrive\"\nthis is not [toml\n",
        "sync_root = 5\n",
        "config_version = 3\n[[accounts]]\nid = \"3f9a1c0e5b7d\"\nlabel = \"Personal\"\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::in_dir(dir.path());
        std::fs::write(&paths.config_file, text).unwrap();
        let store = ConfigStore::open(&paths, async { true }).await;
        assert!(store.is_poisoned(), "{text:?}");
        assert_eq!(store.snapshot(), Config::default());
        assert!(store.last_error().contains(&paths.config_file.display().to_string()), "{}", store.last_error());
        assert!(matches!(store.add_account("Personal"), Err(ConfigError::Unreadable(_))));
        assert_eq!(std::fs::read_to_string(&paths.config_file).unwrap(), text, "never written");
        assert!(!crate::config::migrate::v1_copy(&paths.config_file).exists(), "never migrated");
    }
}
