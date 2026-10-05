//! What `config.toml` holds (version 2), and the rules of what it may hold.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::ids::{drive_or_none, drives, is_account_id};
use crate::config::{AccountId, DriveId};

/// The version of `config.toml` this build reads and writes.
pub const CONFIG_VERSION: u32 = 2;

/// The label of the account a version-1 configuration becomes.
pub const MIGRATED_LABEL: &str = "Personal";

/// `config.toml`, version 2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub config_version: u32,
    /// The Entra application every account signs in with.
    #[serde(default)]
    pub client_id: String,
    /// The drives the development build may hand a read-write token out for
    /// (`TokenExport.ReadWrite`; `docs/design/writes.md` §2.3, §12.1): the test accounts'. It decides
    /// nothing else — an account's mode is the user's choice, whatever its drive. Empty, the
    /// default, hands none out: the developer install sets it to the test account's drive by
    /// hand, and nothing in the daemon writes it (limitations log F60).
    #[serde(default, deserialize_with = "drives", skip_serializing_if = "Vec::is_empty")]
    pub write_test_drive_ids: Vec<DriveId>,
    /// Whether every account holds its background work back on a metered connection
    /// (`docs/design/writes.md` §11); `None` for yes. One setting for the
    /// machine, not per account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_on_metered: Option<bool>,
    /// What every account does on battery: `sync`, `power-saver` or
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
    /// the limit and its count. [`konedrive_graph::pool::DEFAULT_LARGE`] when missing.
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
    pub id: AccountId,
    /// What people see and type; see [`check_label`]. Nothing on disk is named after it.
    pub label: String,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub origin: Origin,
    /// The Graph drive id: the account's identity, recorded at its first sign-in or its
    /// first `GET /me/drive`, and never changed. `None` until then.
    #[serde(default, deserialize_with = "drive_or_none", skip_serializing_if = "Option::is_none")]
    pub drive_id: Option<DriveId>,
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
    /// `None` for the defaults (`local::DEFAULT_PATTERNS`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore: Option<Vec<String>>,
    /// The name a conflict copy carries (`docs/design/writes.md` §7); empty for the host's
    /// (`local::names::default_machine_name`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub machine_name: String,
    /// Whether Graph's thumbnails of the account's images and videos are fetched;
    /// `None` for yes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnails: Option<bool>,
    /// `pause_on_metered` as an account had it before it became one setting for the whole
    /// app: read only to be moved to [`Config::pause_on_metered`]
    /// ([`crate::config::migrate::move_hold_settings`]), and gone from the file once moved.
    #[serde(default, rename = "pause_on_metered", skip_serializing_if = "Option::is_none")]
    pub old_pause_on_metered: Option<bool>,
    /// `on_battery` as an account had it before issue #95; see
    /// [`old_pause_on_metered`](Self::old_pause_on_metered).
    #[serde(default, rename = "on_battery", skip_serializing_if = "Option::is_none")]
    pub old_on_battery: Option<String>,
}

impl AccountConfig {
    /// An account with nothing but its id, its label and where it came from: read-only, no
    /// drive, no folder, every setting at its default.
    pub fn new(id: AccountId, label: impl Into<String>, origin: Origin) -> Self {
        Self {
            id,
            label: label.into(),
            mode: Mode::ReadOnly,
            origin,
            drive_id: None,
            login_hint: String::new(),
            legacy_token: false,
            migrate_files: false,
            root: None,
            ignore: None,
            machine_name: String::new(),
            thumbnails: None,
            old_pause_on_metered: None,
            old_on_battery: None,
        }
    }

    /// `thumbnails`, absent meaning on.
    pub fn thumbnails_on(&self) -> bool {
        self.thumbnails.unwrap_or(true)
    }
}

/// The two settings of the automatic hold, one for every account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldSettings {
    pub pause_on_metered: bool,
    pub on_battery: OnBattery,
}

/// What every account does on battery (`docs/design/writes.md` §11).
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
    /// `upgrade_when_helper`, with a missing value read so: a root without
    /// interception switches, and an intercepted root has nothing to switch.
    pub fn upgrades_when_helper(&self) -> bool {
        self.upgrade_when_helper.unwrap_or(!self.intercepted)
    }
}

/// An account's mode (`docs/design/writes.md` §2): read-only, the default, or read-write. The mode in
/// `config.toml` is the one the user chose; the account runs read-write only while its token
/// carries `Files.ReadWrite` and reaches the drive recorded for it (`AccountService::mode`).
/// A value this version does not know loads as
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

/// Checks a label against the rules of `Accounts.Add` and `Account.SetLabel`
/// ([`konedrive_dbus::LABEL_RULE`], the sentence a person is told; whoever changes a rule
/// here changes it there), and returns it trimmed. 12 hexadecimal digits are refused in any case,
/// so that a label is never taken for an id in `--account`; `except` is the account being
/// renamed. `@` is allowed: an account's label is commonly its email. `Err` says why, for
/// `InvalidArgs`.
pub fn check_label(label: &str, config: &Config, except: Option<&AccountId>) -> Result<String, String> {
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
    if is_account_id(&label.to_ascii_lowercase()) {
        return Err("a label may not be 12 hexadecimal digits, which is what an account id looks like".into());
    }
    let lower = label.to_lowercase();
    match config.accounts.iter().find(|a| Some(&a.id) != except && a.label.to_lowercase() == lower) {
        Some(other) => Err(format!("the label {:?} is already used", other.label)),
        None => Ok(label.to_owned()),
    }
}

impl Config {
    /// The accounts' `pause_on_metered` and `on_battery` of before, folded into
    /// the global keys and taken out of the accounts. The strictest value wins, over every
    /// account and a global key already there — an account without the key counting as its
    /// default: `on_battery` takes `pause` over `power-saver` over `sync` (a value it does not
    /// know reads `power-saver`); `pause_on_metered` is off only when every account says off.
    /// `None`, changing nothing, when no account has either key.
    pub fn take_old_hold_settings(&mut self) -> Option<HoldSettings> {
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
        Some(HoldSettings { pause_on_metered, on_battery })
    }

    pub fn account(&self, id: &AccountId) -> Option<&AccountConfig> {
        self.accounts.iter().find(|a| a.id == *id)
    }

    pub fn account_mut(&mut self, id: &AccountId) -> Option<&mut AccountConfig> {
        self.accounts.iter_mut().find(|a| a.id == *id)
    }

    /// Whether `TokenExport.ReadWrite` may hand out a token for `drive` (`docs/design/writes.md`
    /// §2.3): only for a drive listed in `write_test_drive_ids`, and for none while the list
    /// is empty, as it is by default. It is asked for nothing else: the list does not decide
    /// an account's mode.
    pub fn read_write_export_allowed(&self, drive: &DriveId) -> bool {
        self.write_test_drive_ids.contains(drive)
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
                if !account.id.is_valid() {
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
    if later.drive_id.is_some() && earlier.drive_id == later.drive_id {
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

/// konedrive's own application registration in Microsoft Entra (personal Microsoft accounts,
/// the loopback redirect). A public client's ID is not a secret: it is sent in every sign-in
/// URL. `config.toml`'s `client_id` overrides it for anyone who registers their own.
pub const DEFAULT_CLIENT_ID: &str = "384b100c-c384-4a68-99b2-17a596bb66b9";

/// Accepts the canonical GUID form `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (hex digits, any case).
pub fn is_valid_client_id(id: &str) -> bool {
    let groups: Vec<&str> = id.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.chars().all(|c| c.is_ascii_hexdigit()))
}
