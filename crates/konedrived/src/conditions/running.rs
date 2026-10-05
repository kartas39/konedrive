//! What background work an account runs now (`docs/design/writes.md` §11): the one place
//! that decides it, from the user's pause, the automatic hold (a metered connection, the
//! battery: [`Conditions`], told by `conditions`) and the settings. Every
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
//! The hold's two settings ([`HoldSettings`]) are one pair for the whole app:
//! the registry tells every account, as it tells the conditions. Thumbnails stay per account.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use crate::config::{AccountConfig, Config, OnBattery};
use konedrive_tree::{Store, TreeError};

/// The clock an account's pause is kept by: the time, and a wait. Everything that asks
/// whether a timed pause is over, or waits for it to be, has the account's one clock, so
/// they agree. The daemon's is [`SystemClock`]; a test gives one it moves by hand.
pub trait Clock: Send + Sync {
    /// The time, in unix seconds.
    fn now(&self) -> i64;
    /// Returns once the clock reads `at` (unix seconds) or later, as far as the clock can
    /// tell: the system's sleeps for the time left until then, which leaves out the time
    /// the machine was suspended and knows nothing of a clock that was set. So whoever
    /// waits looks at [`now`](Self::now) again after it.
    fn sleep_until(&self, at: i64) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

/// The machine's wall clock, and the runtime's sleep.
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        crate::clock::unix_now()
    }

    fn sleep_until(&self, at: i64) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(tokio::time::sleep(Duration::from_secs(at.saturating_sub(self.now()).max(0) as u64)))
    }
}

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

/// The settings of the hold, one pair for every account: `config.toml`'s global
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

/// What the machine's sources say now (`conditions`): one value for the daemon,
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
pub struct Running {
    /// The account's clock: a timed pause is over by it.
    clock: Arc<dyn Clock>,
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

impl Default for Running {
    /// The default settings, by the system's clock.
    fn default() -> Self {
        Self::new(Settings::default(), Arc::new(SystemClock))
    }
}

impl Running {
    pub fn new(settings: Settings, clock: Arc<dyn Clock>) -> Self {
        Self { clock, inner: Mutex::new(Inner { settings, ..Inner::default() }), thumbnails_on: Notify::new() }
    }

    /// The account's clock.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// The user's pause kept in `store`, by the account's clock ([`user_pause`]).
    pub fn user_pause(&self, store: &Store) -> Option<i64> {
        user_pause(store, self.clock.now())
    }

    pub fn settings(&self) -> Settings {
        crate::panic::lock(&self.inner).settings
    }

    /// Takes `change` into the settings; a thumbnail setting turned on wakes the filler.
    pub fn change(&self, change: impl FnOnce(&mut Settings)) {
        let (before, after) = {
            let mut inner = crate::panic::lock(&self.inner);
            let before = inner.settings;
            change(&mut inner.settings);
            (before, inner.settings)
        };
        if after.thumbnails && !before.thumbnails {
            self.thumbnails_on.notify_one();
        }
    }

    pub fn hold_settings(&self) -> HoldSettings {
        crate::panic::lock(&self.inner).hold
    }

    /// The global hold settings now; a change ends a `SyncAnyway`. Whether anything changed.
    pub fn set_hold_settings(&self, hold: HoldSettings) -> bool {
        let mut inner = crate::panic::lock(&self.inner);
        if inner.hold == hold {
            return false;
        }
        inner.hold = hold;
        inner.anyway = false;
        true
    }

    /// What the sources say now; a change ends a `SyncAnyway`. Whether anything changed.
    pub fn set_conditions(&self, conditions: Conditions) -> bool {
        let mut inner = crate::panic::lock(&self.inner);
        if inner.conditions == conditions {
            return false;
        }
        inner.conditions = conditions;
        inner.anyway = false;
        true
    }

    /// `SyncAnyway`: the hold is lifted until a source or the hold's settings change.
    pub fn sync_anyway(&self) {
        crate::panic::lock(&self.inner).anyway = true;
    }

    /// Why the account holds back by itself now, if it does.
    pub fn held(&self) -> Option<Hold> {
        let inner = crate::panic::lock(&self.inner);
        if inner.anyway {
            return None;
        }
        Hold::of(inner.hold, inner.conditions)
    }

    /// Why the account's background work stops now, if it does: the user's pause, kept in
    /// `store`, before the hold.
    pub fn stop(&self, store: &Store) -> Option<Stop> {
        self.user_pause(store).map(Stop::Paused).or_else(|| self.held().map(Stop::Held))
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

/// The user's pause of the account whose tree store is `store` (`docs/design/writes.md` §11):
/// `Some(until)` while paused, unix seconds, 0 meaning until resumed; `Paused` and
/// `PausedUntil` show it. A timed pause that has run out at `now` (unix seconds, by the
/// account's [`Clock`]) is taken off here. Kept in the store's `meta`, so it survives a
/// restart. The store keeps the time the pause ends and compares it with no clock of its
/// own. Answered from the store's memory of it ([`Store::pause`]), never by a job: callable
/// from anywhere.
pub fn user_pause(store: &Store, now: i64) -> Option<i64> {
    let until = store.pause()?;
    if until != 0 && until <= now {
        store.pause_ended(until);
        return None;
    }
    Some(until)
}

/// Pauses the account whose tree store is `store` until `until` (unix
/// seconds, 0 for until resumed), or resumes it (`None`).
pub async fn set_paused(store: &Store, until: Option<i64>) -> Result<(), TreeError> {
    store.set_pause(until).await
}

/// [`set_paused`] for plain threads.
pub fn set_paused_blocking(store: &Store, until: Option<i64>) -> Result<(), TreeError> {
    store.set_pause_blocking(until)
}
#[cfg(test)]
mod tests;
