//! The account's own settings, kept in its section of `config.toml` and taken at once:
//! thumbnails, the user's pause, the ignore list, and the name conflict copies are made
//! with.

use std::sync::Arc;

use super::{Persist, SyncError, SyncService};
use crate::conditions::running::Settings;
use crate::config::ConfigError;
use crate::local::{IgnoreList, SharedIgnore};

/// The ignore list `config.toml` gives the account, or the defaults.
pub(super) fn configured_ignore(persist: &Persist) -> SharedIgnore {
    let account = persist.store.account(&persist.account);
    IgnoreList::configured(account.as_ref().and_then(|a| a.ignore.as_deref())).shared()
}

impl SyncService {
    /// The account's own settings that decide what runs (`Folder.Thumbnails`).
    pub fn run_settings(&self) -> Settings {
        self.running.settings()
    }

    /// `SetThumbnails`, `Pause`, `Resume`: `change` is written to the account's section of
    /// `config.toml`, and taken at once. Refused `Unsupported` for a folder not connected to
    /// OneDrive. Nothing here needs the account's sync to be running.
    pub async fn change_run_settings(&self, change: impl FnOnce(&mut Settings) + Send + 'static) -> Result<(), SyncError> {
        self.require_onedrive()?;
        // `config.toml` first, and the settings in memory only once it is written: a change
        // that is refused changes nothing, here or on the bus. One change at a time
        // ([`Running::changing`]), so two calls at once leave the file and memory the same.
        // Only what changed is written.
        let (running, persist) = (Arc::clone(&self.running), self.wiring.persist.clone());
        tokio::task::spawn_blocking(move || {
            let _one = running.changing();
            let before = running.settings();
            let mut after = before;
            change(&mut after);
            persist
                .store
                .update_account(&persist.account, |a| {
                    if after.thumbnails != before.thumbnails {
                        a.thumbnails = Some(after.thumbnails);
                    }
                    // By the file, not by memory: a timed pause that ran out is gone from
                    // memory and still written there.
                    if a.paused_until != after.paused_until {
                        a.paused_until = after.paused_until;
                    }
                    Ok::<_, ConfigError>(())
                })
                .map_err(|e| SyncError::Config(format!("cannot write config.toml: {e}")))?;
            running.change(|s| *s = after);
            Ok::<_, SyncError>(())
        })
        .await
        .map_err(|e| SyncError::Io(format!("the settings task failed: {e}")))??;
        self.show_pause();
        Ok(())
    }

    /// `IgnorePatterns`: the account's ignore list (`docs/design/writes.md` §4.4).
    pub fn ignore_patterns(&self) -> Vec<String> {
        crate::panic::read(&self.ignore).patterns().to_vec()
    }

    /// `SetIgnorePatterns(patterns)`: the account's ignore list from now on,
    /// written to `config.toml`. A Full local scan follows, so that a name
    /// no longer ignored is uploaded. Refused `InvalidArgs` for a pattern that
    /// cannot match a name ([`IgnoreList::invalid`]).
    pub async fn set_ignore_patterns(&self, patterns: Vec<String>) -> Result<(), SyncError> {
        if let Some((pattern, why)) = patterns.iter().find_map(|p| IgnoreList::invalid(p).map(|why| (p, why))) {
            return Err(SyncError::InvalidArgs(format!("{pattern:?}: {why}")));
        }
        let mut unique: Vec<String> = Vec::new();
        for pattern in patterns {
            if !unique.contains(&pattern) {
                unique.push(pattern);
            }
        }
        // `config.toml` and the list the watcher reads change together, under the
        // list's own lock: two calls at once leave both the same (the outbox on the bus).
        let (shared, persist) = (Arc::clone(&self.ignore), self.wiring.persist.clone());
        tokio::task::spawn_blocking(move || {
            let mut list = crate::panic::write(&shared);
            let kept = unique.clone();
            persist
                .store
                .update_account(&persist.account, |a| {
                    a.ignore = Some(kept);
                    Ok::<_, ConfigError>(())
                })
                .map_err(|e| SyncError::Config(format!("cannot write config.toml: {e}")))?;
            *list = IgnoreList::new(unique);
            Ok::<(), SyncError>(())
        })
        .await
        .map_err(|e| SyncError::Io(format!("the settings task failed: {e}")))??;
        self.rescan_for_ignore_list();
        Ok(())
    }

    /// `MachineName`: what a conflict copy is named after (`docs/design/writes.md` §7):
    /// `machine_name` in `config.toml`, or the host's name.
    pub fn machine_name(&self) -> String {
        let persist = &self.wiring.persist;
        let configured = persist.store.account(&persist.account).map(|a| a.machine_name).unwrap_or_default();
        if configured.is_empty() {
            crate::local::names::default_machine_name()
        } else {
            crate::local::names::machine_name(&configured)
        }
    }
}
