//! The one owner of `config.toml`: [`ConfigStore`].

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::config::atomic::write_atomic;
use crate::config::migrate::V1Config;
use crate::config::{
    check_label, is_valid_client_id, AccountConfig, AccountId, Config, DriveId, Mode, OnBattery, Origin, Paths, RootConfig,
    DEFAULT_CLIENT_ID,
};

/// Why a [`ConfigStore`] call changed nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// `config.toml` cannot be read, was written by a newer version, or was replaced by a
    /// version-1 file while the daemon ran: nothing is written over it (`Failed`).
    #[error("{0}")]
    Unreadable(String),
    /// It could not be written (`Failed`).
    #[error("{0}")]
    Write(String),
    /// No account has this id (`NoAccount`).
    #[error("there is no account {:?}", .0.as_str())]
    NoAccount(AccountId),
    /// A label [`check_label`] refuses, with the reason (`InvalidArgs`).
    #[error("{0}")]
    InvalidLabel(String),
    #[error("invalid client ID: expected a GUID like 00000000-0000-0000-0000-000000000000")]
    InvalidClientId,
    /// [`ConfigStore::record_drive`] of a drive another account has; its label.
    #[error("this Microsoft account is already connected as '{0}'")]
    DriveTaken(String),
}

/// What `config.toml` says about one account's writes, from one reading of the file: the
/// mode and the list it is gated by are never read at different times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteStanding {
    /// The mode the user chose.
    pub mode: Mode,
    /// The account's drive, when the development gate lets it through
    /// ([`Config::writes_allowed`]); `None` for a drive not listed and for an account with no
    /// drive yet.
    pub writable_drive: Option<DriveId>,
}

impl WriteStanding {
    /// Whether the file lets the account write: read-write chosen, and the drive let through.
    pub fn allows_writes(&self) -> bool {
        self.mode == Mode::ReadWrite && self.writable_drive.is_some()
    }
}

impl From<ConfigError> for String {
    fn from(error: ConfigError) -> Self {
        error.to_string()
    }
}

/// The one owner of `config.toml`. Every write goes through [`update`](Self::update), which
/// re-reads the file, applies the change and writes it atomically, holding one lock across
/// all three: two writers can no longer save over each other.
///
/// An unreadable file is never overwritten: the store is then *poisoned* for the life of
/// the process — it has no accounts, refuses every write, and says why in
/// [`last_error`](Self::last_error) (`Accounts.LastError`). A later start with the file
/// fixed loads, or migrates, it then.
///
/// The calls do blocking file I/O on a small file, as the single-account code did. The one
/// caller that asks before every outbox row, the write gate, calls from a blocking thread
/// (`Engine::may_write` of `upload/engine.rs`); the others are in limitations log F231.
pub struct ConfigStore {
    file: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    /// As last read or written; empty while poisoned.
    config: Config,
    /// Why the file could not be loaded; `Some` means poisoned.
    poisoned: Option<String>,
    /// Trouble that belongs to no account: the poison, or a migration step that failed.
    last_error: String,
}

/// What `config.toml` holds.
enum FileRead {
    Missing,
    V1 { text: String, config: V1Config },
    V2(Config),
}

fn read(file: &Path) -> Result<FileRead, String> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FileRead::Missing),
        Err(e) => return Err(format!("{} cannot be read: {e}", file.display())),
    };
    let unreadable = |e: toml::de::Error| format!("{} cannot be read: {}", file.display(), e.message());
    let table: toml::Table = toml::from_str(&text).map_err(unreadable)?;
    match table.get("config_version") {
        None | Some(toml::Value::Integer(1)) => {
            let config = toml::from_str(&text).map_err(unreadable)?;
            Ok(FileRead::V1 { text, config })
        }
        Some(toml::Value::Integer(2)) => Ok(FileRead::V2(toml::from_str(&text).map_err(unreadable)?)),
        Some(toml::Value::Integer(n)) if *n > 2 => Err(format!(
            "{} was written by a newer version of konedrive (configuration version {n})",
            file.display()
        )),
        Some(other) => Err(format!("{} has a configuration version this konedrive does not know: {other}", file.display())),
    }
}

/// Writes `config` to `file` atomically.
pub(super) fn write_config(file: &Path, config: &Config) -> Result<(), ConfigError> {
    let failed = |e: &dyn std::fmt::Display| ConfigError::Write(format!("cannot save {}: {e}", file.display()));
    let text = toml::to_string(config).map_err(|e| failed(&e))?;
    write_atomic(file, text.as_bytes()).map_err(|e| failed(&e))
}

impl ConfigStore {
    /// Loads `paths.config_file` — first step of the daemon's start (design §2.2). A
    /// version-1 file is migrated (§7.2): `legacy_token` is the wallet's presence check for
    /// the version-1 refresh token (no unlock; a Secret Service that does not answer counts
    /// as present), and is awaited only when nothing else says there is an account to carry
    /// over. A missing file is an empty configuration and is not written.
    ///
    /// Next, before any account's services open a file: [`crate::config::migrate::finish_file_moves`].
    pub async fn open(paths: &Paths, legacy_token: impl Future<Output = bool>) -> Self {
        let file = paths.config_file.clone();
        let loaded = match read(&file) {
            Ok(FileRead::Missing) => Ok(Config::default()),
            Ok(FileRead::V2(config)) => Ok(config),
            Ok(FileRead::V1 { text, config }) => crate::config::migrate::migrate(paths, &text, config, legacy_token).await,
            Err(e) => Err(e),
        };
        let (config, poisoned) = match loaded {
            Ok(config) => (config, None),
            Err(e) => {
                tracing::error!("{e}; no account is loaded, and the file is not written");
                (Config::default(), Some(e))
            }
        };
        for (account, held) in config.accounts.iter().zip(config.holds()) {
            if let Some(why) = held {
                tracing::warn!("{}: account {:?} is held: {why}", file.display(), account.label);
            }
        }
        let last_error = poisoned.clone().unwrap_or_default();
        Self { file, inner: Mutex::new(Inner { config, poisoned, last_error }) }
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A change that panicked wrote nothing and left `config` as it was.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The configuration as last read or written; empty while poisoned. Held accounts are
    /// in it: see [`Config::holds`].
    pub fn snapshot(&self) -> Config {
        self.lock().config.clone()
    }

    pub fn account(&self, id: &AccountId) -> Option<AccountConfig> {
        self.lock().config.account(id).cloned()
    }

    /// The application's client ID: the one set in `config.toml`, or else konedrive's own
    /// ([`DEFAULT_CLIENT_ID`]), so that signing in needs nothing from the user.
    pub fn client_id(&self) -> String {
        let set = self.lock().config.client_id.clone();
        if set.is_empty() { DEFAULT_CLIENT_ID.to_owned() } else { set }
    }

    pub fn is_poisoned(&self) -> bool {
        self.lock().poisoned.is_some()
    }

    /// `Accounts.LastError`: why the file could not be loaded, or which migration step
    /// failed. Empty when there is nothing.
    pub fn last_error(&self) -> String {
        self.lock().last_error.clone()
    }

    pub(crate) fn note_error(&self, message: String) {
        let mut inner = self.lock();
        if !inner.last_error.is_empty() {
            inner.last_error.push_str("; ");
        }
        inner.last_error.push_str(&message);
    }

    /// The one way to change `config.toml`: under the store's lock, re-reads the file,
    /// applies `change`, and writes the result atomically — or nothing, when `change`
    /// returns `Err` or changes nothing. Refused while poisoned, and when the file can no
    /// longer be read or has become a version-1 file: what cannot be read is never
    /// overwritten. A missing file is an empty configuration. `change` runs under the lock,
    /// so a check and the write it allows are one step (the identity guard of §8.2).
    pub fn update<R, E: From<ConfigError>>(&self, change: impl FnOnce(&mut Config) -> Result<R, E>) -> Result<R, E> {
        let mut inner = self.lock();
        if let Some(why) = &inner.poisoned {
            return Err(ConfigError::Unreadable(format!("{why}; it is not written until konedrived starts with it readable")).into());
        }
        let mut config = match read(&self.file).map_err(ConfigError::Unreadable)? {
            FileRead::Missing => Config::default(),
            FileRead::V2(config) => config,
            FileRead::V1 { .. } => {
                return Err(ConfigError::Unreadable(format!(
                    "{} was replaced by a version-1 configuration; restart konedrived to migrate it",
                    self.file.display()
                ))
                .into())
            }
        };
        let before = config.clone();
        let result = change(&mut config)?;
        if config != before {
            write_config(&self.file, &config)?;
        }
        inner.config = config;
        Ok(result)
    }

    /// [`update`](Self::update) of one account; `NoAccount` when there is none with `id`.
    pub fn update_account<R, E: From<ConfigError>>(
        &self,
        id: &AccountId,
        change: impl FnOnce(&mut AccountConfig) -> Result<R, E>,
    ) -> Result<R, E> {
        self.update(|config| match config.account_mut(id) {
            Some(account) => change(account),
            None => Err(ConfigError::NoAccount(id.clone()).into()),
        })
    }

    /// `Accounts.SetClientId`'s write. The rule that no account may be signing in or
    /// signed in is the caller's.
    pub fn set_client_id(&self, id: &str) -> Result<(), ConfigError> {
        let id = id.trim();
        if !is_valid_client_id(id) {
            return Err(ConfigError::InvalidClientId);
        }
        self.update(|config| {
            config.client_id = id.to_owned();
            Ok(())
        })
    }

    /// `Accounts.SetPauseOnMetered`'s write: one setting for every account.
    /// Keys of an account still left (a move whose write failed) are moved first, in the
    /// same write, so that a later start's move cannot undo the user's choice.
    pub fn set_pause_on_metered(&self, on: bool) -> Result<(), ConfigError> {
        self.update(|config| {
            config.take_old_hold_settings();
            config.pause_on_metered = Some(on);
            Ok(())
        })
    }

    /// `Accounts.SetOnBattery`'s write: one setting for every account.
    pub fn set_on_battery(&self, choice: OnBattery) -> Result<(), ConfigError> {
        self.update(|config| {
            config.take_old_hold_settings();
            config.on_battery = Some(choice.as_str().to_owned());
            Ok(())
        })
    }

    /// `Accounts.Add`: a read-only account with no folder and no drive yet, after every
    /// other, under a fresh id.
    pub fn add_account(&self, label: &str) -> Result<AccountConfig, ConfigError> {
        self.update(|config| {
            let label = check_label(label, config, None).map_err(ConfigError::InvalidLabel)?;
            let id = AccountId::fresh(config.accounts.iter().map(|a| &a.id));
            let account = AccountConfig::new(id, label, Origin::Added);
            config.accounts.push(account.clone());
            Ok(account)
        })
    }

    /// `Account.SetLabel`: returns the label as stored (trimmed).
    pub fn set_label(&self, id: &AccountId, label: &str) -> Result<String, ConfigError> {
        self.update(|config| {
            let label = check_label(label, config, Some(id)).map_err(ConfigError::InvalidLabel)?;
            let account = config.account_mut(id).ok_or_else(|| ConfigError::NoAccount(id.clone()))?;
            account.label = label.clone();
            Ok(label)
        })
    }

    /// Takes the account's section out of the file and returns it. Its files, token and
    /// folder are the caller's to deal with (`Accounts.Remove`).
    pub fn remove_account(&self, id: &AccountId) -> Result<AccountConfig, ConfigError> {
        self.update(|config| {
            let at = config.accounts.iter().position(|a| a.id == *id).ok_or_else(|| ConfigError::NoAccount(id.clone()))?;
            Ok(config.accounts.remove(at))
        })
    }

    /// The registration's record of the account's folder (`None`: forgotten). The drive
    /// stays: it is the account's, not the folder's.
    pub fn set_root(&self, id: &AccountId, root: Option<RootConfig>) -> Result<(), ConfigError> {
        self.update_account(id, |account| {
            account.root = root;
            Ok(())
        })
    }

    /// `config.toml` as it is *now*, read again: what a hand edit made since the daemon
    /// started (a drive added to `write_test_drive_ids`) counts. `None` for a store that is
    /// poisoned and a file that cannot be read now, which callers take as refusing writes.
    ///
    /// The file is read without the store's lock: every write replaces it in one step
    /// (`write_atomic`), so a reading is a whole file, the one before a write or the one after,
    /// and nobody who wants the last reading waits for the disk.
    pub fn current(&self) -> Option<Config> {
        if self.is_poisoned() {
            return None;
        }
        match read(&self.file) {
            Ok(FileRead::V2(config)) => Some(config),
            _ => None,
        }
    }

    /// What `config.toml` says *now* about account `id`'s writes: the file is read again, so
    /// an edit of the gate's list — a drive taken off it — counts at once, not at the next
    /// write. `None` for a store that is poisoned, a file that cannot be read now, and an
    /// account that is not there: callers take that as read-only, and the gate fails closed.
    pub fn write_standing(&self, id: &AccountId) -> Option<WriteStanding> {
        let config = self.current()?;
        let account = config.account(id)?;
        let writable_drive = account.drive_id.clone().filter(|drive| config.writes_allowed(drive));
        Some(WriteStanding { mode: account.mode, writable_drive })
    }

    /// Records `drive` as the account's drive when it has none yet, and returns the
    /// account's drive, which differs from `drive` when another was recorded before: the
    /// caller's same-account check (§8.1) compares them. Writes nothing when a drive is
    /// already recorded. Refused `DriveTaken` when another account has `drive`: a drive is
    /// one account (§8.2), whichever way it comes to be recorded.
    pub fn record_drive(&self, id: &AccountId, drive: &DriveId) -> Result<DriveId, ConfigError> {
        self.update(|config| {
            let mine = config.account(id).ok_or_else(|| ConfigError::NoAccount(id.clone()))?;
            if let Some(recorded) = &mine.drive_id {
                return Ok(recorded.clone());
            }
            if let Some(other) = config.accounts.iter().find(|a| a.id != *id && a.drive_id.as_ref() == Some(drive)) {
                return Err(ConfigError::DriveTaken(other.label.clone()));
            }
            config.account_mut(id).expect("found above").drive_id = Some(drive.clone());
            Ok(drive.clone())
        })
    }
}
