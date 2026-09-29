//! What background work an account runs now (`docs/design/writes.md` §11): the one place
//! that decides it, from the user's pause, the automatic hold (a metered connection, the
//! battery: [`Conditions`], told by `sync::conditions`) and the account's settings. Every
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
//! neither is on. `SyncAnyway` lifts the hold until a source or the account's
//! `pause_on_metered` / `on_battery` changes.

use std::sync::Mutex;

use tokio::sync::Notify;

use crate::config::{AccountConfig, OnBattery};
use crate::tree::Store;

/// An account's settings that decide what runs, as `config.toml` gives them
/// (`Folder.Thumbnails`, `PauseOnMetered`, `OnBattery`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub thumbnails: bool,
    pub pause_on_metered: bool,
    pub on_battery: OnBattery,
}

impl Default for Settings {
    fn default() -> Self {
        Self { thumbnails: true, pause_on_metered: true, on_battery: OnBattery::default() }
    }
}

impl Settings {
    pub fn of(account: &AccountConfig) -> Self {
        Self { thumbnails: account.thumbnails_on(), pause_on_metered: account.pauses_on_metered(), on_battery: account.on_battery() }
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
    pub fn of(settings: Settings, conditions: Conditions) -> Option<Self> {
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

    /// Takes `change` into the settings; a thumbnail setting turned on wakes the filler, and
    /// a change of `pause_on_metered` or `on_battery` ends a `SyncAnyway`.
    pub fn change(&self, change: impl FnOnce(&mut Settings)) {
        let (before, after) = {
            let mut inner = self.inner.lock().unwrap();
            let before = inner.settings;
            change(&mut inner.settings);
            let after = inner.settings;
            if (after.pause_on_metered, after.on_battery) != (before.pause_on_metered, before.on_battery) {
                inner.anyway = false;
            }
            (before, after)
        };
        if after.thumbnails && !before.thumbnails {
            self.thumbnails_on.notify_one();
        }
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
        Hold::of(inner.settings, inner.conditions)
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
    /// The account's settings that decide what runs (`Folder.Thumbnails`, `PauseOnMetered`,
    /// `OnBattery`).
    pub fn run_settings(&self) -> Settings {
        self.running.settings()
    }

    /// `SetThumbnails`, `SetPauseOnMetered`, `SetOnBattery`: `change` is written to the
    /// account's section of `config.toml`, and taken at once. Refused `Unsupported` for a
    /// folder not connected to OneDrive, as `Pause` is.
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
                    if after.pause_on_metered != before.pause_on_metered {
                        a.pause_on_metered = Some(after.pause_on_metered);
                    }
                    if after.on_battery != before.on_battery {
                        a.on_battery = Some(after.on_battery.as_str().to_owned());
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

    /// `SyncAnyway()`: the hold is lifted now, until a source or the account's
    /// `pause_on_metered` / `on_battery` changes. Not kept across a restart. Refused
    /// `Unsupported` as the setters are.
    pub fn sync_anyway(&self) -> Result<(), super::SyncError> {
        self.require_onedrive()?;
        self.running.sync_anyway();
        tracing::info!("syncing anyway, though the account would hold back");
        self.show_pause();
        Ok(())
    }

    /// `SetOnBattery(choice)`: refused `InvalidArgs` for anything but `sync`, `power-saver`
    /// or `pause`.
    pub async fn set_on_battery(&self, choice: &str) -> Result<(), super::SyncError> {
        let Some(choice) = OnBattery::parse(choice) else {
            return Err(super::SyncError::InvalidArgs(format!("{choice:?}: not sync, power-saver or pause")));
        };
        self.change_run_settings(move |s| s.on_battery = choice).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::TreeStore;

    #[tokio::test]
    async fn thumbnails_go_only_while_on_and_nothing_stops() {
        let store = Store::new(TreeStore::in_memory().unwrap());
        let running = Running::default();
        assert!(running.thumbnails_go(&store));
        running.change(|s| s.thumbnails = false);
        assert!(!running.thumbnails_go(&store));
        assert!(!running.stopped(&store), "thumbnails off stop nothing else");
        running.change(|s| s.thumbnails = true);
        tokio::time::timeout(std::time::Duration::from_secs(1), running.thumbnails_turned_on()).await.expect("turned on wakes the filler");
        crate::sync::upload::set_paused(&store, Some(0)).await.unwrap();
        assert_eq!(running.stop(&store), Some(Stop::Paused(0)));
        assert!(!running.thumbnails_go(&store), "a pause stops thumbnails too");
    }

    /// Issue #57: each `on_battery` choice against on battery or on mains, in the
    /// power-saver profile or another — on mains the battery never holds — and the
    /// metered connection, which `pause_on_metered = false` ignores and which wins over a
    /// battery reason.
    #[test]
    fn the_hold_follows_the_conditions_and_the_settings() {
        let settings = |on_battery| Settings { on_battery, ..Settings::default() };
        let conditions = |on_battery, power_saver| Conditions { metered: false, on_battery, power_saver };
        for (choice, on_battery, power_saver, held) in [
            (OnBattery::Sync, true, true, None),
            (OnBattery::Sync, true, false, None),
            (OnBattery::PowerSaver, true, true, Some(Hold::PowerSaver)),
            (OnBattery::PowerSaver, true, false, None),
            (OnBattery::Pause, true, true, Some(Hold::OnBattery)),
            (OnBattery::Pause, true, false, Some(Hold::OnBattery)),
        ] {
            assert_eq!(Hold::of(settings(choice), conditions(on_battery, power_saver)), held, "{choice:?}, power-saver {power_saver}");
            for power_saver in [true, false] {
                assert_eq!(Hold::of(settings(choice), conditions(false, power_saver)), None, "{choice:?} on mains");
            }
        }
        let metered = Conditions { metered: true, on_battery: true, power_saver: true };
        assert_eq!(Hold::of(settings(OnBattery::Pause), metered), Some(Hold::Metered), "the network's reason first");
        let ignoring = Settings { pause_on_metered: false, on_battery: OnBattery::Sync, ..Settings::default() };
        assert_eq!(Hold::of(ignoring, metered), None, "pause_on_metered = false");
    }

    /// `SyncAnyway` lifts the hold until the conditions change, or the hold's own settings
    /// do; the thumbnail setting and conditions told again unchanged leave it lifted.
    #[tokio::test]
    async fn sync_anyway_lasts_until_a_source_or_the_holds_settings_change() {
        let store = Store::new(TreeStore::in_memory().unwrap());
        let running = Running::default();
        let metered = Conditions { metered: true, ..Conditions::default() };
        assert!(running.set_conditions(metered));
        assert_eq!(running.stop(&store), Some(Stop::Held(Hold::Metered)));
        running.sync_anyway();
        assert_eq!(running.held(), None);
        assert!(!running.set_conditions(metered), "the same again is no change");
        running.change(|s| s.thumbnails = false);
        assert_eq!(running.held(), None);
        running.set_conditions(Conditions { on_battery: true, ..metered });
        assert_eq!(running.held(), Some(Hold::Metered), "a source changed: worked out again");
        running.sync_anyway();
        running.change(|s| s.on_battery = OnBattery::Pause);
        assert_eq!(running.held(), Some(Hold::Metered), "the hold's setting changed");
        crate::sync::upload::set_paused(&store, Some(0)).await.unwrap();
        assert_eq!(running.stop(&store), Some(Stop::Paused(0)), "the user's pause is said first");
    }

    #[test]
    fn settings_read_config_toml_with_its_defaults() {
        let mut account: AccountConfig = toml::from_str("id = \"a\"\nlabel = \"A\"\n").unwrap();
        assert_eq!(Settings::of(&account), Settings::default());
        assert_eq!(Settings::default(), Settings { thumbnails: true, pause_on_metered: true, on_battery: OnBattery::PowerSaver });
        account.on_battery = Some("whenever".into());
        assert_eq!(account.on_battery(), OnBattery::PowerSaver, "an unknown value falls back");
        account.on_battery = Some("pause".into());
        account.thumbnails = Some(false);
        account.pause_on_metered = Some(false);
        assert_eq!(Settings::of(&account), Settings { thumbnails: false, pause_on_metered: false, on_battery: OnBattery::Pause });
    }
}
