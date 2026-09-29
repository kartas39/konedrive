//! What background work an account runs now (`docs/design/writes.md` §11): the one place
//! that decides it, from the user's pause and the account's thumbnail setting. Every
//! reader of the pause — the transfer pool (through `SyncService::show_pause`), the outbox
//! worker, the poll and the replacements it runs, the thumbnail filler — asks here, not
//! the tree store.
//!
//! - **Paused**: everything but work on demand stops — uploads, pinned downloads,
//!   replacements of changed files, checking OneDrive for changes, thumbnails. Opening a
//!   file and `Hydrate` go on.
//! - **Thumbnails off**: the filler makes no request; everything else runs.

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

/// Why background work stops now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The user's pause: until then (unix seconds), 0 for until resumed.
    Paused(i64),
}

/// One account's decision of what runs. Shared by the account's sync and every task it
/// starts.
#[derive(Debug, Default)]
pub struct Running {
    settings: Mutex<Settings>,
    /// Woken when thumbnails are turned on: the filler asks for what is missing.
    thumbnails_on: Notify,
}

impl Running {
    pub fn new(settings: Settings) -> Self {
        Self { settings: Mutex::new(settings), thumbnails_on: Notify::new() }
    }

    pub fn settings(&self) -> Settings {
        *self.settings.lock().unwrap()
    }

    /// Takes `change` into the settings; a thumbnail setting turned on wakes the filler.
    pub fn change(&self, change: impl FnOnce(&mut Settings)) {
        let (before, after) = {
            let mut settings = self.settings.lock().unwrap();
            let before = *settings;
            change(&mut settings);
            (before, *settings)
        };
        if after.thumbnails && !before.thumbnails {
            self.thumbnails_on.notify_one();
        }
    }

    /// Why the account's background work stops now, if it does: the user's pause, kept in
    /// `store`.
    pub fn stop(&self, store: &Store) -> Option<Stop> {
        user_pause(store).map(Stop::Paused)
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
