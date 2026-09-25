//! Version 1 of `config.toml`, and its migration to version 2 (design §7).
//!
//! [`V1Config`] is the single-account file of before; only the migration reads it.

use std::future::Future;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{
    new_account_id, write_atomic, write_config, AccountConfig, Config, ConfigError, ConfigStore, Mode, Origin, Paths,
    RootConfig, CONFIG_VERSION, MIGRATED_LABEL,
};

/// `config.toml`, version 1: no `config_version`, one account, one folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct V1Config {
    #[serde(default)]
    pub client_id: String,
    /// The registered sync root, empty when none. It has to be
    /// "persisted, so it survives a restart" — and
    /// without it the startup recovery walk never runs at a startup at
    /// all, since nothing else re-registers the folder.
    #[serde(default)]
    pub sync_root: String,
    /// Whether that root is the ordinary intercepted kind. Defaults to
    /// `true` when the key is missing, so the fail-closed mode is what an
    /// older or hand-edited config restores: a root wrongly restored as
    /// intercepted refuses to come up without a helper, while one wrongly
    /// restored as un-intercepted would come up silently serving zeros.
    #[serde(default = "intercepted_by_default")]
    pub sync_root_intercepted: bool,
    /// The root id the folder carried when it was registered — the name the
    /// helper holds an intercepted root under. Recorded so that a root
    /// restored at startup can be held, forgotten and told apart before the
    /// helper is back, without reading anything from the folder. Empty in a
    /// config written before it existed; the folder's own
    /// `user.konedrive.root` stands in for it then.
    #[serde(default)]
    pub sync_root_id: String,
    /// What the folder shows: `"onedrive"` — listed from the
    /// signed-in drive, locked, kept in step — or `"local"`, filled with
    /// `PopulateFromDirectory` as in part 1. A config written before this
    /// existed describes a local folder.
    #[serde(default = "local_source")]
    pub sync_root_source: String,
    /// Whether *this daemon* excluded the root from KDE's Baloo indexer
    /// — `false` when the folder was already excluded (the
    /// user's own doing, or a parent directory's), since a Forget must never
    /// take off an exclusion it did not add. Defaults to `false`, the safe
    /// side for a config written before this field existed: nothing is
    /// removed from Baloo's settings that this daemon cannot be sure it put
    /// there.
    #[serde(default)]
    pub sync_root_baloo_excluded: bool,
    /// Whether a root registered without interception switches to
    /// interception when the helper connects: `true` when it was
    /// registered that way because no helper was connected, `false` when a
    /// helper was and the mode was a choice. Missing in a config written
    /// before this existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_root_upgrade_when_helper: Option<bool>,
    /// The drive a OneDrive folder was listed from, recorded
    /// when its sync first learns it, so the check that the account signed
    /// in is still that drive's survives a tree store rebuilt empty.
    /// Empty until then, for a local folder, and in a
    /// config written before it existed. It goes with `sync_root_id`: a
    /// different root never inherits it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sync_root_drive_id: String,
}

fn intercepted_by_default() -> bool {
    true
}

fn local_source() -> String {
    "local".into()
}

impl Default for V1Config {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            sync_root: String::new(),
            sync_root_intercepted: true,
            sync_root_id: String::new(),
            sync_root_source: local_source(),
            sync_root_baloo_excluded: false,
            sync_root_upgrade_when_helper: None,
            sync_root_drive_id: String::new(),
        }
    }
}

impl V1Config {
    /// A missing file yields the default configuration.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        write_atomic(path, toml::to_string(self)?.as_bytes())
    }

    /// `sync_root_upgrade_when_helper`, with a config written before it
    /// existed read as Ruling 4 says: a root without interception
    /// switches — such a config cannot tell a folder registered that way on
    /// purpose from one registered before the helper was installed, and the
    /// second reads as zeros until it switches — and an intercepted root has
    /// nothing to switch.
    pub fn sync_root_upgrades_when_helper(&self) -> bool {
        self.sync_root_upgrade_when_helper.unwrap_or(!self.sync_root_intercepted)
    }
}

/// Where the version-1 file is kept once migrated: `config.toml.v1`, the way back by hand.
pub fn v1_copy(config_file: &Path) -> PathBuf {
    with_suffix(config_file, ".v1")
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

/// Whether `path` may exist: an error in finding out counts as yes.
fn present(path: &Path) -> bool {
    !matches!(path.try_exists(), Ok(false))
}

/// Version 1 as version 2 (§7.2 steps 2 and 3). Account #1 — `Personal`, read-only,
/// migrated, the folder's drive as its identity, both migration flags set, the folder when
/// there is one — when there is anything to carry over: a folder, the cached account or the
/// tree store at version 1's paths, or the version-1 refresh token in the wallet
/// (`legacy_token`, awaited only when nothing else says so). Otherwise the client id alone.
async fn to_v2(v1: V1Config, paths: &Paths, legacy_token: impl Future<Output = bool>) -> Config {
    let carry = !v1.sync_root.is_empty() || present(&paths.account_cache) || present(&paths.tree_db) || legacy_token.await;
    let root = (!v1.sync_root.is_empty()).then(|| RootConfig {
        path: PathBuf::from(&v1.sync_root),
        id: v1.sync_root_id,
        intercepted: v1.sync_root_intercepted,
        source: v1.sync_root_source,
        baloo_excluded: v1.sync_root_baloo_excluded,
        upgrade_when_helper: v1.sync_root_upgrade_when_helper,
    });
    let accounts = if carry {
        vec![AccountConfig {
            id: new_account_id([]),
            label: MIGRATED_LABEL.into(),
            mode: Mode::ReadOnly,
            origin: Origin::Migrated,
            drive_id: v1.sync_root_drive_id,
            legacy_token: true,
            migrate_files: true,
            root,
        }]
    } else {
        Vec::new()
    };
    Config { config_version: CONFIG_VERSION, client_id: v1.client_id, accounts }
}

/// §7.2 step 4: `text` (the version-1 file) is copied to `config.toml.v1`, then version 2 is
/// written atomically — the commit point. Before it the old layout is untouched; after it,
/// [`finish_file_moves`] finishes the job. `Err` when either write failed.
pub(crate) async fn migrate(
    paths: &Paths,
    text: &str,
    v1: V1Config,
    legacy_token: impl Future<Output = bool>,
) -> Result<Config, String> {
    let file = &paths.config_file;
    let config = to_v2(v1, paths, legacy_token).await;
    let copy = v1_copy(file);
    write_atomic(&copy, text.as_bytes())
        .map_err(|e| format!("{} cannot be migrated: {} cannot be written: {e}", file.display(), copy.display()))?;
    write_config(file, &config).map_err(|e| format!("{} cannot be migrated: {e}", file.display()))?;
    match config.accounts.first() {
        Some(account) => tracing::info!(
            "{} migrated to version 2: its account is {:?} ({}); version 1 is kept in {}",
            file.display(),
            account.label,
            account.id,
            copy.display()
        ),
        None => tracing::info!("{} migrated to version 2, with no account to carry over", file.display()),
    }
    Ok(config)
}

/// How one file move went.
enum Moved {
    /// Moved, or nothing left to move.
    Done,
    /// Left where it is, as the design allows: what it holds is fetched again (the cached
    /// account) or rebuilt with one listing (the tree store).
    Left(String),
    /// An error nothing planned for: the step is tried again at the next start.
    Failed(String),
}

/// §7.3, at every start, after [`ConfigStore::open`] and before any account's services open
/// a file: for each account whose `migrate_files` is set, moves version 1's `account.json`
/// and `tree.sqlite` into the account's own directory, then clears the flag. Every step is
/// idempotent — a source that is gone means the step is done — so a crash anywhere is
/// finished by the next start. A file that cannot be moved safely is left where it is and
/// logged. An unexpected error keeps the flag, for the next start to try again, and is
/// shown in [`ConfigStore::last_error`].
pub fn finish_file_moves(store: &ConfigStore, paths: &Paths) {
    for account in store.snapshot().accounts.into_iter().filter(|a| a.migrate_files) {
        // A hand-edited id names no directory; such an account is held anyway.
        let Some(to) = paths.account(&account.id) else { continue };
        let steps = match std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&to.dir) {
            Ok(()) => vec![move_file(&paths.account_cache, &to.account_cache), move_tree_store(&paths.tree_db, &to.tree_db)],
            Err(e) => vec![Moved::Failed(format!("{} cannot be created: {e}", to.dir.display()))],
        };
        let mut failed = Vec::new();
        for step in steps {
            match step {
                Moved::Done => {}
                Moved::Left(why) => tracing::warn!("{why}"),
                Moved::Failed(why) => failed.push(why),
            }
        }
        let done = if failed.is_empty() {
            store.update_account(&account.id, |a| {
                a.migrate_files = false;
                Ok::<_, ConfigError>(())
            })
        } else {
            Err(ConfigError::Write(failed.join("; ")))
        };
        if let Err(e) = done {
            let message = format!("moving the files of account {:?} failed: {e}", account.label);
            tracing::error!("{message}");
            store.note_error(message);
        }
    }
}

fn move_file(from: &Path, to: &Path) -> Moved {
    if !present(from) {
        return Moved::Done;
    }
    if present(to) {
        return Moved::Left(format!("{} is left where it is: {} exists already", from.display(), to.display()));
    }
    match std::fs::rename(from, to) {
        Ok(()) => Moved::Done,
        Err(e) => Moved::Failed(format!("{} cannot be moved to {}: {e}", from.display(), to.display())),
    }
}

/// Moves the tree store only as one file: opened and closed once, so the last connection's
/// close checkpoints the write-ahead log into the database and removes it. A database moved
/// without its log would lose what was committed to the log alone, so one whose log survives
/// the close — another process has it open — is left where it is.
fn move_tree_store(from: &Path, to: &Path) -> Moved {
    if !present(from) {
        return Moved::Done;
    }
    if present(to) {
        return Moved::Left(format!(
            "{} is left where it is: {} exists already; the account rebuilds its tree with a listing",
            from.display(),
            to.display()
        ));
    }
    let closed = rusqlite::Connection::open_with_flags(from, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).and_then(|db| {
        db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        db.close().map_err(|(_, e)| e)
    });
    let left = |why: String| {
        Moved::Left(format!("{} is left where it is: {why}; the account rebuilds its tree with a listing", from.display()))
    };
    if let Err(e) = closed {
        return left(format!("it cannot be opened ({e})"));
    }
    if present(&with_suffix(from, "-wal")) {
        return left("its write-ahead log is still there after closing it; another process may have it open".into());
    }
    if let Err(e) = std::fs::rename(from, to) {
        return Moved::Failed(format!("{} cannot be moved to {}: {e}", from.display(), to.display()));
    }
    let shm = with_suffix(from, "-shm");
    if let Err(e) = std::fs::remove_file(&shm) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("cannot remove {}: {e}", shm.display());
        }
    }
    Moved::Done
}

#[cfg(test)]
mod tests {
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
}
