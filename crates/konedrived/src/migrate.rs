//! Version 1 of `config.toml`, and its migration to version 2 (design §7).
//!
//! [`V1Config`] is the single-account file of before; only the migration reads it.

use std::future::Future;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{
    new_account_id, write_atomic, write_config, AccountConfig, Config, ConfigError, ConfigStore, Mode, OnBattery, Origin,
    Paths, RootConfig, CONFIG_VERSION, MIGRATED_LABEL,
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
            login_hint: String::new(),
            legacy_token: true,
            migrate_files: true,
            root,
            ignore: None,
            machine_name: String::new(),
            thumbnails: None,
            old_pause_on_metered: None,
            old_on_battery: None,
        }]
    } else {
        Vec::new()
    };
    Config { config_version: CONFIG_VERSION, client_id: v1.client_id, accounts, ..Config::default() }
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

/// Issue #95, once, at a start after [`ConfigStore::open`] and before the accounts are
/// loaded: an account's `pause_on_metered` and `on_battery` of before become the global keys,
/// and leave the accounts, the strictest value winning ([`Config::take_old_hold_settings`]).
/// Nothing is written when no account has either key, so a second start changes nothing, and
/// nothing is tried on an unreadable file (its trouble is already in
/// [`ConfigStore::last_error`]), as [`finish_file_moves`] does. A write that fails is logged
/// and shown in [`ConfigStore::last_error`]; the keys stay for the next start (or the next
/// `SetPauseOnMetered` / `SetOnBattery`), and the accounts run on the global keys meanwhile.
pub fn move_hold_settings(store: &ConfigStore) {
    if store.is_poisoned() {
        return;
    }
    let moved = store.update(|config| {
        // Nothing to move changes nothing, and `update` writes nothing then.
        Ok::<_, ConfigError>(config.take_old_hold_settings())
    });
    match moved {
        Ok(None) => {}
        Ok(Some((pause_on_metered, on_battery))) => tracing::info!(
            "the accounts' pause_on_metered and on_battery moved to one setting for every account, the strictest: \
             pause_on_metered = {pause_on_metered}, on_battery = {:?}",
            on_battery.as_str()
        ),
        Err(e) => {
            let message = format!("moving the accounts' pause_on_metered and on_battery failed: {e}");
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
mod tests;
