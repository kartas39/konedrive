//! Daemon configuration (`~/.config/konedrive/config.toml`, version 2) and file locations.
//!
//! One [`ConfigStore`] owns the file: it loads it, migrates version 1 into account #1
//! ([`crate::config::migrate`]), holds back accounts that collide ([`Config::holds`]), and runs every
//! read-modify-write under one lock ([`ConfigStore::update`]).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use crate::config::migrate::V1Config;

/// The version of `config.toml` this build reads and writes.
pub const CONFIG_VERSION: u32 = 2;

/// The label of the account a version-1 configuration becomes.
pub const MIGRATED_LABEL: &str = "Personal";

/// Locations of the daemon's files. Tests point these into a temp dir.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_file: PathBuf,
    /// `$XDG_STATE_HOME/konedrive`: each account's state is in `accounts/<id>/` below it.
    pub state_dir: PathBuf,
    /// The cached name and quota of version 1, where the migration finds them. Per account:
    /// [`Paths::account`].
    pub account_cache: PathBuf,
    /// The tree store of version 1, where the migration finds it. Per account:
    /// [`Paths::account`].
    pub tree_db: PathBuf,
    /// Where files are rescued to: version 1 directly below it, each account in `<id>/`.
    pub rescue_dir: PathBuf,
    /// The freedesktop thumbnail cache, shared by every account (keyed by file URI).
    pub thumbnails: PathBuf,
}

/// Where one account's files are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountPaths {
    /// `accounts/<id>/` under the state directory: everything in it goes with the account.
    pub dir: PathBuf,
    /// Its cached name and quota.
    pub account_cache: PathBuf,
    /// Its tree store, activity log and conflicts.
    pub tree_db: PathBuf,
    /// `rescued/<id>/`: kept when the account is removed.
    pub rescue_dir: PathBuf,
}

impl Paths {
    /// Standard XDG locations for the current user.
    pub fn from_xdg() -> anyhow::Result<Self> {
        let config = dirs::config_dir().ok_or_else(|| anyhow::anyhow!("no XDG config directory"))?;
        let state = dirs::state_dir().ok_or_else(|| anyhow::anyhow!("no XDG state directory"))?;
        let data = dirs::data_dir().ok_or_else(|| anyhow::anyhow!("no XDG data directory"))?;
        let cache = dirs::cache_dir().ok_or_else(|| anyhow::anyhow!("no XDG cache directory"))?;
        let state = state.join("konedrive");
        Ok(Self {
            config_file: config.join("konedrive").join("config.toml"),
            account_cache: state.join("account.json"),
            tree_db: state.join("tree.sqlite"),
            state_dir: state,
            rescue_dir: data.join("konedrive").join("rescued"),
            thumbnails: cache.join("thumbnails"),
        })
    }

    /// Every file directly under `dir`.
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            config_file: dir.join("config.toml"),
            state_dir: dir.to_owned(),
            account_cache: dir.join("account.json"),
            tree_db: dir.join("tree.sqlite"),
            rescue_dir: dir.join("rescued"),
            thumbnails: dir.join("thumbnails"),
        }
    }

    /// The files of account `id`. `None` unless `id` is an account id
    /// ([`is_valid_account_id`]), so a hand-edited id never names a path outside
    /// `accounts/`.
    pub fn account(&self, id: &str) -> Option<AccountPaths> {
        if !is_valid_account_id(id) {
            return None;
        }
        let dir = self.state_dir.join("accounts").join(id);
        Some(AccountPaths {
            account_cache: dir.join("account.json"),
            tree_db: dir.join("tree.sqlite"),
            dir,
            rescue_dir: self.rescue_dir.join(id),
        })
    }
}

/// `config.toml`, version 2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub config_version: u32,
    /// The Entra application every account signs in with.
    #[serde(default)]
    pub client_id: String,
    /// The development gate of the write phase (`docs/design/writes.md` §2.3): the drive ids of the test
    /// accounts that may be read-write. Every other account stays read-only, whatever its
    /// `mode` says. Empty — the default — lets no account through: the developer install
    /// sets it to the test account's drive by hand, and nothing in the daemon writes it. The
    /// release removes the gate in a commit of its own (limitations log F60).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_test_drive_ids: Vec<String>,
    /// Whether every account holds its background work back on a metered connection
    /// (issues #57, #95, `docs/design/writes.md` §11); `None` for yes. One setting for the
    /// machine, not per account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_on_metered: Option<bool>,
    /// What every account does on battery (issues #57, #95): `sync`, `power-saver` or
    /// `pause`; `None` for `power-saver`. Kept as written, so that a value this version does
    /// not know cannot make the whole file unreadable ([`Config::on_battery`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_battery: Option<String>,
    /// Every account, in the order it was added.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<AccountConfig>,
    /// `[transfers]`: the transfer pools' emergency ceiling and large-stream limit. Not in the
    /// window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfers: Option<TransfersConfig>,
}

/// `[transfers]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransfersConfig {
    /// The most requests one account's transfer pool has in flight (`konedrive_graph::pool`), each
    /// account's separately; [`konedrive_graph::pool::DEFAULT_CEILING`] when missing. Clamped into
    /// 1–256.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
    /// The streams of large sync transfers (files of [`konedrive_graph::pool::LARGE_FROM`] and up; a
    /// download in parts runs several) one account runs at once; a file being opened is outside
    /// the limit and its count (issue #50). [`konedrive_graph::pool::DEFAULT_LARGE`] when missing.
    /// Clamped into 1…`max`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub large: Option<i64>,
}

impl Config {
    /// Each account's transfer pool ceiling: `[transfers] max`, clamped into
    /// [`konedrive_graph::pool::CEILING_MIN`]–[`konedrive_graph::pool::CEILING_MAX`] with a warning when it is
    /// outside, or [`konedrive_graph::pool::DEFAULT_CEILING`].
    pub fn transfer_ceiling(&self) -> usize {
        use konedrive_graph::pool::{CEILING_MAX, CEILING_MIN, DEFAULT_CEILING};
        let Some(max) = self.transfers.as_ref().and_then(|t| t.max) else { return DEFAULT_CEILING };
        let clamped = max.clamp(CEILING_MIN as i64, CEILING_MAX as i64) as usize;
        if clamped as i64 != max {
            tracing::warn!("[transfers] max = {max} in config.toml is outside {CEILING_MIN}-{CEILING_MAX}; using {clamped}");
        }
        clamped
    }

    /// Each account's large-stream limit (the streams of large sync transfers, never an
    /// open): `[transfers] large`, clamped into 1…the ceiling
    /// ([`transfer_ceiling`](Self::transfer_ceiling)) with a warning when it is outside, or
    /// [`konedrive_graph::pool::DEFAULT_LARGE`] (never above the ceiling).
    pub fn transfer_large(&self) -> usize {
        let ceiling = self.transfer_ceiling();
        let Some(large) = self.transfers.as_ref().and_then(|t| t.large) else {
            return konedrive_graph::pool::DEFAULT_LARGE.min(ceiling);
        };
        let clamped = large.clamp(1, ceiling as i64) as usize;
        if clamped as i64 != large {
            tracing::warn!("[transfers] large = {large} in config.toml is outside 1-{ceiling}; using {clamped}");
        }
        clamped
    }
}

impl Config {
    /// `pause_on_metered`, absent meaning on.
    pub fn pauses_on_metered(&self) -> bool {
        self.pause_on_metered.unwrap_or(true)
    }

    /// `on_battery`, absent meaning `power-saver`; any other value is `power-saver` too,
    /// with a warning in the log.
    pub fn on_battery(&self) -> OnBattery {
        OnBattery::read(self.on_battery.as_deref())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_version: CONFIG_VERSION,
            client_id: String::new(),
            write_test_drive_ids: Vec::new(),
            pause_on_metered: None,
            on_battery: None,
            accounts: Vec::new(),
            transfers: None,
        }
    }
}

/// One account, `[[accounts]]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountConfig {
    /// 12 random lowercase hex characters, never reused; in object paths and file paths.
    pub id: String,
    /// What people see and type; see [`check_label`]. Nothing on disk is named after it.
    pub label: String,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub origin: Origin,
    /// The Graph drive id: the account's identity, recorded at its first sign-in or its
    /// first `GET /me/drive`, and never changed. Empty until then.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub drive_id: String,
    /// The Microsoft account's email as its last sign-in found it: the `login_hint` of a
    /// sign-in that asks for `Files.ReadWrite`. Kept across a sign-out, which
    /// forgets the cached name and email, so a read-write account signing in again is still
    /// pinned to its own account.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub login_hint: String,
    /// Set only until the refresh token of version 1 is moved to this account's own
    /// Secret Service item (design §7.4).
    #[serde(default, skip_serializing_if = "is_false")]
    pub legacy_token: bool,
    /// Set only until `account.json` and `tree.sqlite` of version 1 are moved into this
    /// account's directory ([`crate::config::migrate::finish_file_moves`]).
    #[serde(default, skip_serializing_if = "is_false")]
    pub migrate_files: bool,
    /// The account's registered folder; `None` when it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<RootConfig>,
    /// Names of local files that are never uploaded (`docs/design/writes.md` §4.4), shell globs;
    /// `None` for the defaults (`local::ignore::DEFAULT_PATTERNS`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore: Option<Vec<String>>,
    /// The name a conflict copy carries (`docs/design/writes.md` §7); empty for the host's
    /// (`local::names::default_machine_name`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub machine_name: String,
    /// Whether Graph's thumbnails of the account's images and videos are fetched
    /// (issue #80); `None` for yes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnails: Option<bool>,
    /// `pause_on_metered` as an account had it before it became one setting for the whole
    /// app (issue #95): read only to be moved to [`Config::pause_on_metered`]
    /// ([`crate::config::migrate::move_hold_settings`]), and gone from the file once moved.
    #[serde(default, rename = "pause_on_metered", skip_serializing_if = "Option::is_none")]
    pub old_pause_on_metered: Option<bool>,
    /// `on_battery` as an account had it before issue #95; see
    /// [`old_pause_on_metered`](Self::old_pause_on_metered).
    #[serde(default, rename = "on_battery", skip_serializing_if = "Option::is_none")]
    pub old_on_battery: Option<String>,
}

impl AccountConfig {
    /// `thumbnails`, absent meaning on.
    pub fn thumbnails_on(&self) -> bool {
        self.thumbnails.unwrap_or(true)
    }

}

/// What every account does on battery (issues #57, #95, `docs/design/writes.md` §11).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OnBattery {
    /// The battery changes nothing.
    Sync,
    /// Holds back while on battery and the power profile is `power-saver`.
    #[default]
    PowerSaver,
    /// Holds back whenever on battery.
    Pause,
}

impl OnBattery {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "sync" => Some(Self::Sync),
            "power-saver" => Some(Self::PowerSaver),
            "pause" => Some(Self::Pause),
            _ => None,
        }
    }

    /// `on_battery` as `config.toml` has it: absent is `power-saver`, and so is any value
    /// this version does not know, with a warning in the log.
    pub fn read(text: Option<&str>) -> Self {
        match text {
            None => Self::default(),
            Some(text) => Self::parse(text).unwrap_or_else(|| {
                tracing::warn!("on_battery = {text:?} in config.toml is not sync, power-saver or pause; using power-saver");
                Self::default()
            }),
        }
    }

    /// How strict the choice is: `pause` over `power-saver` over `sync`.
    pub fn strictness(self) -> u8 {
        match self {
            Self::Sync => 0,
            Self::PowerSaver => 1,
            Self::Pause => 2,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sync => "sync",
            Self::PowerSaver => "power-saver",
            Self::Pause => "pause",
        }
    }
}

fn is_false(value: &bool) -> bool {
    !value
}

/// `[accounts.root]`: the registered folder. The fields and their defaults are version 1's
/// `sync_root_*`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootConfig {
    pub path: PathBuf,
    /// The root id the folder carried when it was registered (`user.konedrive.root`), the
    /// name the helper holds an intercepted root under. Empty when carried over from a
    /// configuration older than it; the folder's own attribute stands in for it then.
    #[serde(default)]
    pub id: String,
    /// Whether the root is the ordinary intercepted kind. A missing key reads as `true`,
    /// the fail-closed mode: a root wrongly restored as intercepted refuses to come up
    /// without a helper, one wrongly restored as un-intercepted would serve zeros.
    #[serde(default = "yes")]
    pub intercepted: bool,
    /// `"onedrive"` (listed from the account's drive) or `"local"` (filled with
    /// `PopulateFromDirectory`). A missing key reads as `"local"`; any other value is kept
    /// as it is, and the folder is not brought up (`SyncService`, limitations log F211).
    #[serde(default = "local_source")]
    pub source: String,
    /// Whether this daemon excluded the folder from Baloo, so a Forget takes off only an
    /// exclusion it added.
    #[serde(default)]
    pub baloo_excluded: bool,
    /// Whether a root registered without interception switches to interception when the
    /// helper connects; see [`RootConfig::upgrades_when_helper`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade_when_helper: Option<bool>,
}

fn yes() -> bool {
    true
}

fn local_source() -> String {
    "local".into()
}

impl RootConfig {
    /// `upgrade_when_helper`, with a missing value read as Ruling 4 says: a root without
    /// interception switches, and an intercepted root has nothing to switch.
    pub fn upgrades_when_helper(&self) -> bool {
        self.upgrade_when_helper.unwrap_or(!self.intercepted)
    }
}

/// An account's mode (`docs/design/writes.md` §2): read-only, the default, or read-write. The mode in
/// `config.toml` is the one the user chose; the account runs read-write only while the gate
/// lets its drive through ([`Config::writes_allowed`]) and its token carries
/// `Files.ReadWrite` (`AccountService::mode`). A value this version does not know loads as
/// read-only, is logged, and is written back as read-only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    ReadOnly,
    ReadWrite,
}

impl Mode {
    /// As `config.toml`, `Account.Mode` and `Account.SetMode` spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::ReadOnly => "read-only",
            Mode::ReadWrite => "read-write",
        }
    }

    /// The mode `text` spells, if it spells one.
    pub fn parse(text: &str) -> Option<Mode> {
        [Mode::ReadOnly, Mode::ReadWrite].into_iter().find(|mode| mode.as_str() == text)
    }
}

impl Serialize for Mode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Mode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Ok(Mode::parse(&text).unwrap_or_else(|| {
            tracing::warn!("config.toml: mode {text:?} is not one this version knows; the account is read-only");
            Mode::ReadOnly
        }))
    }
}

/// Where an account came from. `Migrated` marks the account that existed before multiple
/// accounts — the user's real one, which write tests must never use — and is what a missing
/// or unknown value reads as.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Origin {
    Added,
    #[default]
    #[serde(other)]
    Migrated,
}

/// Whether `id` is an account id: 12 lowercase hex characters.
pub fn is_valid_account_id(id: &str) -> bool {
    id.len() == 12 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A fresh account id: 12 random lowercase hex characters (48 bits), none of `taken`.
pub fn new_account_id<'a>(taken: impl IntoIterator<Item = &'a str> + Clone) -> String {
    loop {
        let mut bytes = [0u8; 6];
        getrandom::getrandom(&mut bytes).expect("the OS random number generator failed");
        let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        if !taken.clone().into_iter().any(|t| t == id) {
            return id;
        }
    }
}

/// Checks a label against the rules of `Accounts.Add` and `Account.SetLabel`
/// ([`konedrive_dbus::LABEL_RULE`], the sentence a person is told; whoever changes a rule
/// here changes it there), and returns it trimmed. 12 hexadecimal digits are refused in any case,
/// so that a label is never taken for an id in `--account`; `except` is the account being
/// renamed. `@` is allowed: an account's label is commonly its email. `Err` says why, for
/// `InvalidArgs`.
pub fn check_label(label: &str, config: &Config, except: Option<&str>) -> Result<String, String> {
    let label = label.trim();
    let length = label.chars().count();
    if length == 0 {
        return Err("the label is empty".into());
    }
    if length > 40 {
        return Err(format!("the label is {length} characters long; at most 40 are allowed"));
    }
    if let Some(c) = label.chars().find(|&c| c == '/' || c.is_control()) {
        return Err(format!("a label may not contain {c:?}"));
    }
    if is_valid_account_id(&label.to_ascii_lowercase()) {
        return Err("a label may not be 12 hexadecimal digits, which is what an account id looks like".into());
    }
    let lower = label.to_lowercase();
    match config.accounts.iter().find(|a| Some(a.id.as_str()) != except && a.label.to_lowercase() == lower) {
        Some(other) => Err(format!("the label {:?} is already used", other.label)),
        None => Ok(label.to_owned()),
    }
}

impl Config {
    /// Issue #95: the accounts' `pause_on_metered` and `on_battery` of before, folded into
    /// the global keys and taken out of the accounts. The strictest value wins, over every
    /// account and a global key already there — an account without the key counting as its
    /// default: `on_battery` takes `pause` over `power-saver` over `sync` (a value it does not
    /// know reads `power-saver`); `pause_on_metered` is off only when every account says off.
    /// `None`, changing nothing, when no account has either key.
    pub fn take_old_hold_settings(&mut self) -> Option<(bool, OnBattery)> {
        if !self.accounts.iter().any(|a| a.old_pause_on_metered.is_some() || a.old_on_battery.is_some()) {
            return None;
        }
        let pause_on_metered =
            self.accounts.iter().map(|a| a.old_pause_on_metered.unwrap_or(true)).chain(self.pause_on_metered).any(|on| on);
        let on_battery = self
            .accounts
            .iter()
            .map(|a| a.old_on_battery.as_deref())
            .chain(self.on_battery.as_deref().map(Some))
            .map(OnBattery::read)
            .max_by_key(|choice| choice.strictness())
            .unwrap_or_default();
        for account in &mut self.accounts {
            account.old_pause_on_metered = None;
            account.old_on_battery = None;
        }
        self.pause_on_metered = Some(pause_on_metered);
        self.on_battery = Some(on_battery.as_str().to_owned());
        Some((pause_on_metered, on_battery))
    }

    pub fn account(&self, id: &str) -> Option<&AccountConfig> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn account_mut(&mut self, id: &str) -> Option<&mut AccountConfig> {
        self.accounts.iter_mut().find(|a| a.id == id)
    }

    /// The development gate (`docs/design/writes.md` §2.3): whether the account of `drive_id` may be
    /// read-write. Only a drive listed in `write_test_drive_ids` may. An empty drive id — an
    /// account never signed in — never may, and nothing may while the list is empty, as it is
    /// by default.
    pub fn writes_allowed(&self, drive_id: &str) -> bool {
        !drive_id.is_empty() && self.write_test_drive_ids.iter().any(|allowed| allowed == drive_id)
    }

    /// The validation at load (design §3.1): one entry per account, in file order, `Some`
    /// with the reason when the account is *held* — loaded, but its folder is not brought
    /// up. An account is held when its id is not an account id (it would not fit an object
    /// path or a file path), or when it repeats an earlier account's id, label (whatever
    /// the case) or drive, or its folder has an earlier folder's root id, or is, is inside
    /// or contains an earlier folder. Nothing is rewritten.
    pub fn holds(&self) -> Vec<Option<String>> {
        self.accounts
            .iter()
            .enumerate()
            .map(|(i, account)| {
                if !is_valid_account_id(&account.id) {
                    return Some(format!("its id {:?} is not 12 lowercase hexadecimal characters", account.id));
                }
                self.accounts[..i].iter().find_map(|earlier| conflict(earlier, account))
            })
            .collect()
    }
}

/// Why `later` cannot be brought up beside `earlier`, if it cannot.
fn conflict(earlier: &AccountConfig, later: &AccountConfig) -> Option<String> {
    let other = &earlier.label;
    if earlier.id == later.id {
        return Some(format!("its id is also the id of {other:?}"));
    }
    if earlier.label.to_lowercase() == later.label.to_lowercase() {
        return Some(format!("its label is also the label of {other:?}"));
    }
    if !later.drive_id.is_empty() && earlier.drive_id == later.drive_id {
        return Some(format!("it is the same Microsoft account as {other:?}"));
    }
    let (Some(a), Some(b)) = (&earlier.root, &later.root) else {
        return None;
    };
    if !b.id.is_empty() && a.id == b.id {
        return Some(format!("its folder has the root id of the folder of {other:?}"));
    }
    if a.path.starts_with(&b.path) || b.path.starts_with(&a.path) {
        return Some(format!(
            "its folder {} is, is inside, or contains the folder of {other:?}, {}",
            b.path.display(),
            a.path.display()
        ));
    }
    None
}

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
    #[error("there is no account {0:?}")]
    NoAccount(String),
    /// A label [`check_label`] refuses, with the reason (`InvalidArgs`).
    #[error("{0}")]
    InvalidLabel(String),
    #[error("invalid client ID: expected a GUID like 00000000-0000-0000-0000-000000000000")]
    InvalidClientId,
    /// [`ConfigStore::record_drive`] of a drive another account has; its label.
    #[error("this Microsoft account is already connected as '{0}'")]
    DriveTaken(String),
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
/// The calls do blocking file I/O on a small file, as the single-account code did.
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
enum Read {
    Missing,
    V1 { text: String, config: V1Config },
    V2(Config),
}

fn read(file: &Path) -> Result<Read, String> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Read::Missing),
        Err(e) => return Err(format!("{} cannot be read: {e}", file.display())),
    };
    let unreadable = |e: toml::de::Error| format!("{} cannot be read: {}", file.display(), e.message());
    let table: toml::Table = toml::from_str(&text).map_err(unreadable)?;
    match table.get("config_version") {
        None | Some(toml::Value::Integer(1)) => {
            let config = toml::from_str(&text).map_err(unreadable)?;
            Ok(Read::V1 { text, config })
        }
        Some(toml::Value::Integer(2)) => Ok(Read::V2(toml::from_str(&text).map_err(unreadable)?)),
        Some(toml::Value::Integer(n)) if *n > 2 => Err(format!(
            "{} was written by a newer version of konedrive (configuration version {n})",
            file.display()
        )),
        Some(other) => Err(format!("{} has a configuration version this konedrive does not know: {other}", file.display())),
    }
}

/// Writes `config` to `file` atomically.
pub(crate) fn write_config(file: &Path, config: &Config) -> Result<(), ConfigError> {
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
            Ok(Read::Missing) => Ok(Config::default()),
            Ok(Read::V2(config)) => Ok(config),
            Ok(Read::V1 { text, config }) => crate::config::migrate::migrate(paths, &text, config, legacy_token).await,
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

    pub fn account(&self, id: &str) -> Option<AccountConfig> {
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
            Read::Missing => Config::default(),
            Read::V2(config) => config,
            Read::V1 { .. } => {
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
        id: &str,
        change: impl FnOnce(&mut AccountConfig) -> Result<R, E>,
    ) -> Result<R, E> {
        self.update(|config| match config.account_mut(id) {
            Some(account) => change(account),
            None => Err(ConfigError::NoAccount(id.to_owned()).into()),
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
            let account = AccountConfig {
                id: new_account_id(config.accounts.iter().map(|a| a.id.as_str())),
                label,
                mode: Mode::ReadOnly,
                origin: Origin::Added,
                drive_id: String::new(),
                login_hint: String::new(),
                legacy_token: false,
                migrate_files: false,
                root: None,
                ignore: None,
                machine_name: String::new(),
                thumbnails: None,
                old_pause_on_metered: None,
                old_on_battery: None,
            };
            config.accounts.push(account.clone());
            Ok(account)
        })
    }

    /// `Account.SetLabel`: returns the label as stored (trimmed).
    pub fn set_label(&self, id: &str, label: &str) -> Result<String, ConfigError> {
        self.update(|config| {
            let label = check_label(label, config, Some(id)).map_err(ConfigError::InvalidLabel)?;
            let account = config.account_mut(id).ok_or_else(|| ConfigError::NoAccount(id.to_owned()))?;
            account.label = label.clone();
            Ok(label)
        })
    }

    /// Takes the account's section out of the file and returns it. Its files, token and
    /// folder are the caller's to deal with (`Accounts.Remove`).
    pub fn remove_account(&self, id: &str) -> Result<AccountConfig, ConfigError> {
        self.update(|config| {
            let at = config.accounts.iter().position(|a| a.id == id).ok_or_else(|| ConfigError::NoAccount(id.to_owned()))?;
            Ok(config.accounts.remove(at))
        })
    }

    /// The registration's record of the account's folder (`None`: forgotten). The drive
    /// stays: it is the account's, not the folder's.
    pub fn set_root(&self, id: &str, root: Option<RootConfig>) -> Result<(), ConfigError> {
        self.update_account(id, |account| {
            account.root = root;
            Ok(())
        })
    }

    /// `config.toml` as it is *now*, read again: what a hand edit made since the daemon
    /// started (a drive added to `write_test_drive_ids`) counts. `None` for a store that is
    /// poisoned and a file that cannot be read now, which callers take as refusing writes.
    pub fn current(&self) -> Option<Config> {
        let inner = self.lock();
        if inner.poisoned.is_some() {
            return None;
        }
        match read(&self.file) {
            Ok(Read::V2(config)) => Some(config),
            _ => None,
        }
    }

    /// Whether account `id` may be read-write: its drive is on the gate's list
    /// ([`Config::writes_allowed`]). See [`writable_drive`](Self::writable_drive).
    pub fn writes_allowed(&self, id: &str) -> bool {
        self.writable_drive(id).is_some()
    }

    /// The drive of account `id` when the gate lets it through, as `config.toml` says *now*:
    /// the file is read again, so an edit of the list — a drive taken off it — counts at once,
    /// not at the next write. `None` for an account that is not there, a drive not listed, a
    /// store that is poisoned, and a file that cannot be read now: the gate fails closed.
    pub fn writable_drive(&self, id: &str) -> Option<String> {
        self.write_standing(id).and_then(|(_, drive)| drive)
    }

    /// What `config.toml` says *now* about account `id`'s writes, from one reading of the
    /// file: its mode, and its drive when the gate lets it through (the mode and
    /// the list it is gated by are never read at different times). `None` for a store that
    /// is poisoned, a file that cannot be read now, and an account that is not there:
    /// callers take that as read-only.
    pub fn write_standing(&self, id: &str) -> Option<(Mode, Option<String>)> {
        let inner = self.lock();
        if inner.poisoned.is_some() {
            return None;
        }
        let Ok(Read::V2(config)) = read(&self.file) else { return None };
        let account = config.account(id)?;
        let drive = config.writes_allowed(&account.drive_id).then(|| account.drive_id.clone());
        Some((account.mode, drive))
    }

    /// Records `drive_id` as the account's drive when it has none yet, and returns the
    /// account's drive, which differs from `drive_id` when another was recorded before: the
    /// caller's same-account check (§8.1) compares them. Writes nothing when a drive is
    /// already recorded. Refused `DriveTaken` when another account has `drive_id`: a drive is
    /// one account (§8.2), whichever way it comes to be recorded.
    pub fn record_drive(&self, id: &str, drive_id: &str) -> Result<String, ConfigError> {
        self.update(|config| {
            let recorded = config.account(id).ok_or_else(|| ConfigError::NoAccount(id.to_owned()))?.drive_id.clone();
            if !recorded.is_empty() {
                return Ok(recorded);
            }
            if let Some(other) = config.accounts.iter().find(|a| a.id != id && !drive_id.is_empty() && a.drive_id == drive_id) {
                return Err(ConfigError::DriveTaken(other.label.clone()));
            }
            let account = config.account_mut(id).expect("found above");
            account.drive_id = drive_id.to_owned();
            Ok(account.drive_id.clone())
        })
    }
}

/// Writes `data` to `path` through a temp file in the same directory and a rename.
///
/// The temp file is `fsync`ed before the rename, and the directory after it
///: without the first, a crash soon after could
/// leave `path` naming an empty or partial file — `config.toml` holds the
/// registered folder, and one lost is a folder the next start never
/// recovers; without the second, the rename itself could be lost.
pub fn write_atomic(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    // Both files this is used for (config.toml, account.json) can hold data that is not
    // meant for other local users: config.toml nothing sensitive today, but account.json
    // holds the signed-in name and email. Private from creation — and made so again, for a
    // temp file a crash left behind — so the file is never briefly world-readable.
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(data)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// Accepts the canonical GUID form `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (hex digits, any case).
/// konedrive's own application registration in Microsoft Entra (personal Microsoft accounts,
/// the loopback redirect). A public client's ID is not a secret: it is sent in every sign-in
/// URL. `config.toml`'s `client_id` overrides it for anyone who registers their own.
pub const DEFAULT_CLIENT_ID: &str = "384b100c-c384-4a68-99b2-17a596bb66b9";

pub fn is_valid_client_id(id: &str) -> bool {
    let groups: Vec<&str> = id.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests;
pub mod migrate;
