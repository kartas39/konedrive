use crate::conditions::running::{Conditions, HoldSettings, Settings};

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
