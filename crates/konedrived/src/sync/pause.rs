//! What holds an account's background work back (`docs/design/writes.md` §11): the user's
//! pause, the automatic hold, and the clock that ends a timed pause.
//!
//! **The pause** is per account and kept with the account's settings, in its section of
//! `config.toml` (`paused_until`), so it survives a restart, a timed one ends by itself, and
//! it is set and shown whether the account's sync runs or not. It stops the outbox, the
//! poll (and so the replacements it runs) and the thumbnails; fills on open, `Hydrate` and
//! the watcher's detection go on, and rows keep coalescing. A pause a version before this
//! one kept in the tree store is moved over when the store is first opened
//! ([`SyncService::take_old_pause`]).
//!
//! **The hold** is worked out from the machine's sources and the global hold settings
//! (`conditions::running`), is never kept, and shows as `HeldBack`.
//!
//! Both reach the bus through one function, [`SyncService::show_pause`], and one call of it
//! runs at a time ([`PauseClock::show`]).

use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::{SyncError, SyncService};
use crate::conditions::running::{Clock, Conditions, HoldSettings};

/// How often at most a timed pause is looked at by the clock, in seconds: a sleep counts
/// only the time the machine is awake, so a suspend would stretch a longer one.
const LOOK: i64 = 60;

/// The clock of one account's pause. It keeps the pause as it was last shown and, for a
/// timed one, the one timer that says when it has run out. Its time and its waits are the
/// account's [`Clock`]'s: the one the keeper of the pause (`conditions::running`), the
/// poll and the outbox worker read too, so a pause that is over here is over for them.
///
/// What is shown is worked out under the clock's lock ([`show`](Self::show),
/// [`forget`](Self::forget)), so the last one to show is the last one to have looked: a
/// `Pause` that lands as the timer ends the one before it is never undone on the bus.
pub(super) struct PauseClock {
    shared: Arc<Shared>,
}

struct Shared {
    clock: Arc<dyn Clock>,
    /// Called by the timer, with no lock held, when the timed pause it kept time for has
    /// run out. It is expected to [`show`](PauseClock::show) the pause again.
    over: Box<dyn Fn() + Send + Sync>,
    /// Told when the pause shown is another than before: the timer looks at once.
    changed: Notify,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The pause as last shown: until then (unix seconds), 0 for until resumed.
    shown: Option<i64>,
    timer: Option<JoinHandle<()>>,
}

impl PauseClock {
    pub(super) fn new(clock: Arc<dyn Clock>, over: impl Fn() + Send + Sync + 'static) -> Self {
        Self { shared: Arc::new(Shared { clock, over: Box::new(over), changed: Notify::new(), inner: Mutex::default() }) }
    }

    /// The time now.
    pub(super) fn now(&self) -> i64 {
        self.shared.clock.now()
    }

    /// Shows the pause: `publish` looks at it, says it wherever it is shown, and answers
    /// what it found. One `publish` runs at a time, so it must not call the clock again
    /// (but for [`now`](Self::now)) and must not wait. A timed pause gets the timer, if
    /// none runs; one that runs is told of another pause than it sleeps for, so that a
    /// shorter pause ends at its own time. Outside a runtime no timer is started.
    pub(super) fn show(&self, publish: impl FnOnce() -> Option<i64>) {
        let mut inner = crate::panic::lock(&self.shared.inner);
        let shown = publish();
        let running = inner.timer.as_ref().is_some_and(|timer| !timer.is_finished());
        if running && shown != inner.shown {
            self.shared.changed.notify_one();
        }
        inner.shown = shown;
        if running || !shown.is_some_and(|until| until > 0) {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            inner.timer = Some(runtime.spawn(keep_time(Arc::clone(&self.shared))));
        }
    }

    /// The pause is gone with what it was a pause of: the timer stops, and `publish` takes
    /// it off wherever it is shown, as one step of [`show`](Self::show)'s kind.
    pub(super) fn forget(&self, publish: impl FnOnce()) {
        let mut inner = crate::panic::lock(&self.shared.inner);
        if let Some(timer) = inner.timer.take() {
            timer.abort();
        }
        inner.shown = None;
        publish();
    }
}

impl Drop for PauseClock {
    fn drop(&mut self) {
        if let Some(timer) = crate::panic::lock(&self.shared.inner).timer.take() {
            timer.abort();
        }
    }
}

/// The timer: sleeps until the timed pause shown has run out, looking at the clock at
/// least every [`LOOK`], then says so. It ends when no timed pause is shown; another pause
/// shown while it sleeps wakes it, and is the one it keeps time for from then on.
///
/// Whoever is told looks at the same clock, so it finds the pause over too and shows that.
/// A pause still shown, and still over, after it was said to have run out has nobody left
/// to show it (the service is going): the timer ends, and the next pause shown starts
/// another.
async fn keep_time(shared: Arc<Shared>) {
    loop {
        let until = {
            let mut inner = crate::panic::lock(&shared.inner);
            match inner.shown {
                Some(until) if until > 0 => until,
                // Under the lock: a pause shown after this finds no timer and starts one.
                _ => {
                    inner.timer = None;
                    return;
                }
            }
        };
        let now = shared.clock.now();
        if until > now {
            shared.sleep_until(until.min(now.saturating_add(LOOK))).await;
            continue;
        }
        (shared.over)();
        let mut inner = crate::panic::lock(&shared.inner);
        if inner.shown == Some(until) && until <= shared.clock.now() {
            inner.timer = None;
            return;
        }
    }
}

impl Shared {
    /// Sleeps until the clock reads `at`, or until another pause is shown.
    async fn sleep_until(&self, at: i64) {
        tokio::select! {
            () = self.clock.sleep_until(at) => {}
            () = self.changed.notified() => {}
        }
    }
}

impl SyncService {
    /// `Pause(seconds)`: nothing is uploaded, and OneDrive is not asked for
    /// changes, until `seconds` have passed — or until `Resume()` when 0.
    /// Kept in the account's section of `config.toml`, so it outlasts a restart, and is
    /// taken whether the account's sync runs or not: one that starts later starts paused.
    /// Refused `Unsupported` for a folder not connected to OneDrive.
    pub async fn pause_syncing(&self, seconds: u32) -> Result<(), SyncError> {
        let until = if seconds == 0 { 0 } else { self.clock.now() + i64::from(seconds) };
        self.change_run_settings(move |s| s.paused_until = Some(until)).await?;
        tracing::info!("syncing paused{}", if seconds == 0 { " until resumed".to_owned() } else { format!(" for {seconds} s") });
        Ok(())
    }

    /// `Resume()`: the pause ends now; the outbox and the poll go at once.
    pub async fn resume_syncing(&self) -> Result<(), SyncError> {
        self.change_run_settings(|s| s.paused_until = None).await?;
        tracing::info!("syncing resumed");
        Ok(())
    }

    /// A forgotten folder's pause goes with it, from `config.toml` too: a folder registered
    /// later starts unpaused, as it did while the pause was kept in the folder's store. A
    /// `config.toml` that cannot be written keeps it, with a warning; it is the next
    /// folder's then, shown and ended like any other.
    pub(super) async fn drop_pause(&self) {
        let persist = self.wiring.persist.clone();
        let written = tokio::task::spawn_blocking(move || {
            persist.store.update_account(&persist.account, |a| {
                a.paused_until = None;
                Ok::<_, crate::config::ConfigError>(())
            })
        })
        .await;
        match written {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("the forgotten folder's pause stays in config.toml: {e}"),
            Err(e) => tracing::warn!("the task taking the pause off config.toml failed: {e}"),
        }
        self.forget_pause();
    }

    /// A pause that a version before this one kept in the tree store (`meta.paused_until`)
    /// becomes the account's: read once, when the folder's store is opened, and taken off
    /// the store. It is taken only while the account has no pause of its own in
    /// `config.toml`, and only while it has not run out. A store that cannot say, or a
    /// `config.toml` that cannot be written, leaves the account as it is, with a warning:
    /// the sync starts all the same.
    pub(super) async fn take_old_pause(&self, store: &konedrive_tree::Store) {
        let old = match store.old_pause().await {
            Ok(Some(until)) => until.max(0),
            Ok(None) => return,
            Err(e) => {
                tracing::warn!("the pause kept in the tree store cannot be read: {e}");
                return;
            }
        };
        if old == 0 || old > self.clock.now() {
            let taken = self.change_run_settings(move |s| {
                if s.paused_until.is_none() {
                    s.paused_until = Some(old);
                }
            });
            // Not written: it stays in the store, for the next start.
            if let Err(e) = taken.await {
                tracing::warn!("the pause kept in the tree store is not taken over: {e}");
                return;
            }
            tracing::info!("the pause kept in the tree store is the account's now");
        }
        if let Err(e) = store.set_old_pause(None).await {
            tracing::warn!("the pause kept in the tree store cannot be taken off it: {e}");
        }
    }

    /// Works out the pause and the hold and shows them: `Paused`/`PausedUntil` and
    /// `HeldBack` from `running`, and the transfer pool,
    /// which hands out nothing but opens while the account's background work stops. When
    /// anything changed, what was held back is woken — the outbox, and the poll, which
    /// looks at the pause before each cycle. Called whenever either may have changed:
    /// `Pause`, `Resume`, a sync starting, the sources, the settings, and the clock when a
    /// timed pause has run out.
    ///
    /// Neither needs the folder's sync or its store: a OneDrive folder that is registered
    /// shows its pause in any state.
    pub(super) fn show_pause(&self) {
        // Only a OneDrive folder has background work to hold back.
        let held = match self.require_onedrive() {
            Ok(_) => self.running.held().map(|h| h.as_str().to_owned()).unwrap_or_default(),
            Err(_) => String::new(),
        };
        let onedrive = self.require_onedrive().is_ok();
        let mut changed = false;
        self.clock.show(|| {
            let before = self.state.get();
            let paused = if onedrive { self.running.user_pause() } else { None };
            let stopped = paused.is_some() || !held.is_empty();
            self.state.update(|s| {
                s.pause.paused_until = paused;
                s.pause.held_back = held.clone();
            });
            let was_stopped = self.pool.set_paused(stopped);
            changed = before.pause.paused_until != paused || before.pause.held_back != held || was_stopped != stopped;
            paused
        });
        if changed {
            self.wake_outbox();
            self.nudge();
        }
    }

    /// The folder is forgotten: so are its pause and its hold, on the bus, and the pause
    /// in memory. Taking it off `config.toml` is the caller's ([`Self::drop_pause`]).
    pub(super) fn forget_pause(&self) {
        self.running.change(|s| s.paused_until = None);
        self.clock.forget(|| {
            self.state.update(|s| {
                s.pause.paused_until = None;
                s.pause.held_back.clear();
            });
            self.pool.set_paused(false);
        });
    }

    /// The global hold settings this account runs on now, as the registry told it.
    pub fn hold_settings(&self) -> HoldSettings {
        self.running.hold_settings()
    }

    /// What every account is told alike (`registry`): the hold's settings (`Accounts.
    /// SetPauseOnMetered`, `SetOnBattery`) and what the machine's sources say
    /// (`conditions`). The hold is worked out again; what it held back goes at once when it
    /// ends, and a change ends a `SyncAnyway`.
    pub(super) fn hold_by(&self, hold: HoldSettings, conditions: Conditions) {
        let settings = self.running.set_hold_settings(hold);
        let sources = self.running.set_conditions(conditions);
        if settings || sources {
            self.show_pause();
        }
    }

    /// `SyncAnyway()`: the hold is lifted now, until a source or the global
    /// `pause_on_metered` / `on_battery` changes. Not kept across a restart. Refused
    /// `Unsupported` for a folder not connected to OneDrive, as `Pause` is.
    pub fn sync_anyway(&self) -> Result<(), SyncError> {
        self.require_onedrive()?;
        self.running.sync_anyway();
        tracing::info!("syncing anyway, though the account would hold back");
        self.show_pause();
        Ok(())
    }
}

#[cfg(test)]
mod tests;
