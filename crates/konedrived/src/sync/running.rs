//! What background work an account runs now (`docs/design/writes.md` §11): the one place
//! that decides it, from the user's pause, the automatic hold (a metered connection, the
//! battery: [`Conditions`], told by `sync::conditions`) and the settings. Every
//! reader of the pause — the transfer pool (through `SyncService::show_pause`), the outbox
//! worker, the poll and the replacements it runs, the thumbnail filler — asks here, not
//! the tree store.
//!
//! - **Paused or held back**: everything but work on demand stops — uploads, pinned downloads,
//!   replacements of changed files, checking OneDrive for changes, thumbnails. Opening a
//!   file and `Hydrate` go on.
//! - **Thumbnails off**: the filler makes no request; everything else runs.
//!
//! The hold is not the user's pause: it is not kept in the store, never shows as `Paused`,
//! and is worked out again from the sources after a restart. The account runs only while
//! neither is on. `SyncAnyway` lifts the hold until a source or the global
//! `pause_on_metered` / `on_battery` changes.
//!
//! The hold's two settings ([`HoldSettings`]) are one pair for the whole app (issue #95):
//! the hub tells every account, as it tells the conditions. Thumbnails stay per account.

use std::sync::Mutex;

use tokio::sync::Notify;

use crate::config::{AccountConfig, Config, OnBattery};
use crate::tree::Store;

/// An account's own settings that decide what runs, as its section of `config.toml` gives
/// them (`Folder.Thumbnails`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub thumbnails: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { thumbnails: true }
    }
}

impl Settings {
    pub fn of(account: &AccountConfig) -> Self {
        Self { thumbnails: account.thumbnails_on() }
    }
}

/// The settings of the hold, one pair for every account (issue #95): `config.toml`'s global
/// `pause_on_metered` and `on_battery` (`Accounts.PauseOnMetered`, `OnBattery`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldSettings {
    pub pause_on_metered: bool,
    pub on_battery: OnBattery,
}

impl Default for HoldSettings {
    fn default() -> Self {
        Self { pause_on_metered: true, on_battery: OnBattery::default() }
    }
}

impl HoldSettings {
    pub fn of(config: &Config) -> Self {
        Self { pause_on_metered: config.pauses_on_metered(), on_battery: config.on_battery() }
    }
}

/// What the machine's sources say now (`sync::conditions`): one value for the daemon,
/// told to every account. A source that is missing or cannot be read says "no reason to
/// hold back".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Conditions {
    /// NetworkManager's `Metered` is yes or guessed yes.
    pub metered: bool,
    /// UPower's `OnBattery`.
    pub on_battery: bool,
    /// The power profile is `power-saver`.
    pub power_saver: bool,
}

/// Why an account holds its background work back by itself (`Folder.HeldBack`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    Metered,
    OnBattery,
    PowerSaver,
}

impl Hold {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Metered => "metered",
            Self::OnBattery => "on-battery",
            Self::PowerSaver => "power-saver",
        }
    }

    /// Why `settings` hold an account back under `conditions`, if they do: with a network
    /// and a battery reason at once, the network's. On mains power the battery never holds.
    pub fn of(settings: HoldSettings, conditions: Conditions) -> Option<Self> {
        if conditions.metered && settings.pause_on_metered {
            return Some(Self::Metered);
        }
        if !conditions.on_battery {
            return None;
        }
        match settings.on_battery {
            OnBattery::Sync => None,
            OnBattery::PowerSaver => conditions.power_saver.then_some(Self::PowerSaver),
            OnBattery::Pause => Some(Self::OnBattery),
        }
    }
}

/// Why background work stops now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The user's pause: until then (unix seconds), 0 for until resumed.
    Paused(i64),
    /// The automatic hold.
    Held(Hold),
}

/// One account's decision of what runs. Shared by the account's sync and every task it
/// starts.
#[derive(Debug, Default)]
pub struct Running {
    inner: Mutex<Inner>,
    /// Woken when thumbnails are turned on: the filler asks for what is missing.
    thumbnails_on: Notify,
}

#[derive(Debug, Default)]
struct Inner {
    settings: Settings,
    hold: HoldSettings,
    conditions: Conditions,
    /// `SyncAnyway`: the hold is lifted until the conditions or the hold's settings change.
    anyway: bool,
}

impl Running {
    pub fn new(settings: Settings) -> Self {
        Self { inner: Mutex::new(Inner { settings, ..Inner::default() }), thumbnails_on: Notify::new() }
    }

    pub fn settings(&self) -> Settings {
        self.inner.lock().unwrap().settings
    }

    /// Takes `change` into the settings; a thumbnail setting turned on wakes the filler.
    pub fn change(&self, change: impl FnOnce(&mut Settings)) {
        let (before, after) = {
            let mut inner = self.inner.lock().unwrap();
            let before = inner.settings;
            change(&mut inner.settings);
            (before, inner.settings)
        };
        if after.thumbnails && !before.thumbnails {
            self.thumbnails_on.notify_one();
        }
    }

    pub fn hold_settings(&self) -> HoldSettings {
        self.inner.lock().unwrap().hold
    }

    /// The global hold settings now; a change ends a `SyncAnyway`. Whether anything changed.
    pub fn set_hold_settings(&self, hold: HoldSettings) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.hold == hold {
            return false;
        }
        inner.hold = hold;
        inner.anyway = false;
        true
    }

    /// What the sources say now; a change ends a `SyncAnyway`. Whether anything changed.
    pub fn set_conditions(&self, conditions: Conditions) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.conditions == conditions {
            return false;
        }
        inner.conditions = conditions;
        inner.anyway = false;
        true
    }

    /// `SyncAnyway`: the hold is lifted until a source or the hold's settings change.
    pub fn sync_anyway(&self) {
        self.inner.lock().unwrap().anyway = true;
    }

    /// Why the account holds back by itself now, if it does.
    pub fn held(&self) -> Option<Hold> {
        let inner = self.inner.lock().unwrap();
        if inner.anyway {
            return None;
        }
        Hold::of(inner.hold, inner.conditions)
    }

    /// Why the account's background work stops now, if it does: the user's pause, kept in
    /// `store`, before the hold.
    pub fn stop(&self, store: &Store) -> Option<Stop> {
        user_pause(store).map(Stop::Paused).or_else(|| self.held().map(Stop::Held))
    }

    /// Whether the account's background work stops now.
    pub fn stopped(&self, store: &Store) -> bool {
        self.stop(store).is_some()
    }

    /// Whether the thumbnail filler may ask Graph for thumbnails now.
    pub fn thumbnails_go(&self, store: &Store) -> bool {
        self.settings().thumbnails && !self.stopped(store)
    }

    /// Returns once thumbnails are turned on (at most one wake is kept).
    pub async fn thumbnails_turned_on(&self) {
        self.thumbnails_on.notified().await
    }
}

/// The user's pause of the account whose tree store is `store`
/// ([`crate::sync::upload::paused`]): `Paused` and `PausedUntil` show it.
pub fn user_pause(store: &Store) -> Option<i64> {
    crate::sync::upload::paused(store)
}

impl super::SyncService {
    /// The account's own settings that decide what runs (`Folder.Thumbnails`).
    pub fn run_settings(&self) -> Settings {
        self.running.settings()
    }

    /// The global hold settings this account runs on now, as the hub told it.
    pub fn hold_settings(&self) -> HoldSettings {
        self.running.hold_settings()
    }

    /// `SetThumbnails`: `change` is written to the account's section of `config.toml`, and
    /// taken at once. Refused `Unsupported` for a folder not connected to OneDrive, as
    /// `Pause` is.
    pub async fn change_run_settings(&self, change: impl FnOnce(&mut Settings) + Send + 'static) -> Result<(), super::SyncError> {
        self.require_onedrive()?;
        // `config.toml` and the settings in memory change together, under the file's own
        // lock: two calls at once leave both the same. Only what changed is written.
        let (running, persist) = (std::sync::Arc::clone(&self.running), self.persist.clone());
        tokio::task::spawn_blocking(move || {
            let Some(persist) = persist else {
                running.change(change);
                return Ok(());
            };
            persist
                .store
                .update_account(&persist.account, |a| {
                    let before = running.settings();
                    let mut after = before;
                    change(&mut after);
                    if after.thumbnails != before.thumbnails {
                        a.thumbnails = Some(after.thumbnails);
                    }
                    running.change(|s| *s = after);
                    Ok::<_, crate::config::ConfigError>(())
                })
                .map_err(|e| super::SyncError::Io(format!("cannot write config.toml: {e}")))
        })
        .await
        .map_err(|e| super::SyncError::Io(format!("the settings task failed: {e}")))??;
        self.show_pause();
        Ok(())
    }

    /// What the machine's sources say now (`sync::conditions`, through the hub): the hold is
    /// worked out again, and what it held back goes at once when it ends.
    pub fn set_conditions(&self, conditions: Conditions) {
        if self.running.set_conditions(conditions) {
            self.show_pause();
        }
    }

    /// The global hold settings now (the hub's, `Accounts.SetPauseOnMetered` and
    /// `SetOnBattery`): the hold is worked out again, and a change ends a `SyncAnyway`.
    pub fn set_hold_settings(&self, hold: HoldSettings) {
        if self.running.set_hold_settings(hold) {
            self.show_pause();
        }
    }

    /// `SyncAnyway()`: the hold is lifted now, until a source or the global
    /// `pause_on_metered` / `on_battery` changes. Not kept across a restart. Refused
    /// `Unsupported` as `SetThumbnails` is.
    pub fn sync_anyway(&self) -> Result<(), super::SyncError> {
        self.require_onedrive()?;
        self.running.sync_anyway();
        tracing::info!("syncing anyway, though the account would hold back");
        self.show_pause();
        Ok(())
    }
}

#[cfg(test)]
mod tests;
