//! The folders of every account of one daemon, and what they share (`docs/design/accounts.md` §3.1–§3.4).
//!
//! [`Registry`] is the list of the daemon's accounts as their folders see each other:
//! whether a folder would overlap another account's, the per-inode locks, and what the
//! machine's sources and the hold's settings say.
//!
//! The list has one writer: `daemon::manager`, which adds an account in the lines that add
//! it to its own list and removes it where it removes it there. A service does not put
//! itself on it.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

use super::SyncService;
use crate::conditions::running::{Conditions, HoldSettings};
use crate::config::AccountId;
use crate::folder::locks::InodeLocks;

/// What every account is told alike: what the machine's sources say (`conditions`), and
/// the hold's settings, one pair for every account.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Told {
    conditions: Conditions,
    hold: HoldSettings,
}

thread_local! {
    /// Whether this thread is telling the accounts something with the list's lock held
    /// ([`Registry::tell`]).
    static TELLING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Says, for as long as it lives, that this thread tells accounts under the list's lock.
struct Telling;

impl Telling {
    fn begin() -> Self {
        TELLING.set(true);
        Self
    }
}

impl Drop for Telling {
    fn drop(&mut self) {
        TELLING.set(false);
    }
}

/// The daemon's accounts, as their folders see each other.
pub struct Registry {
    /// One inode belongs to one account only, so one table serves them all.
    locks: InodeLocks,
    /// Every account, in account order: the order they are resumed in. Weak, because a
    /// service holds the registry; an entry is taken out by whoever put it in.
    accounts: Mutex<Vec<(AccountId, Weak<SyncService>)>>,
    /// Held by every new registration, whichever account makes it, from its
    /// overlap check to its end: two accounts cannot both pass the check
    /// with folders that nest (§6.3).
    pub(super) registering: tokio::sync::Mutex<()>,
    /// What every account was told last; one that is added later is told what it is then.
    told: Mutex<Told>,
}

impl Registry {
    /// No account yet.
    pub fn new() -> Arc<Self> {
        Arc::new(Self { locks: InodeLocks::new(), accounts: Mutex::new(Vec::new()), registering: tokio::sync::Mutex::new(()), told: Mutex::new(Told::default()) })
    }

    /// The lock table the uploads of every account share.
    pub fn locks(&self) -> InodeLocks {
        self.locks.clone()
    }

    /// Makes `account` one of the daemon's, after every other: it is told the
    /// conditions and the hold's settings of now, and misses no later change of them. The account manager's, in the lines that list the account.
    pub fn add(&self, account: &Arc<SyncService>) {
        let mut accounts = self.list();
        accounts.retain(|(id, a)| a.strong_count() > 0 && id != account.id());
        // Under the accounts' lock, as every later change is told: none is missed.
        let told = *crate::panic::lock(&self.told);
        {
            let _telling = Telling::begin();
            account.hold_by(told.hold, told.conditions);
        }
        accounts.push((account.id().clone(), Arc::downgrade(account)));
    }

    /// The account `id` is not one of the daemon's any more (an account removed).
    pub fn remove(&self, id: &AccountId) {
        self.list().retain(|(a, _)| a != id);
    }

    /// Every account, in account order.
    pub fn accounts(&self) -> Vec<Arc<SyncService>> {
        self.list().iter().filter_map(|(_, a)| a.upgrade()).collect()
    }

    /// Every account but `me`.
    fn others(&self, me: &AccountId) -> Vec<Arc<SyncService>> {
        self.list().iter().filter(|(id, _)| id != me).filter_map(|(_, a)| a.upgrade()).collect()
    }

    /// The list, locked. What an account does when it is told ([`tell`](Self::tell)) runs
    /// under this lock and must not come back here: said in a debug build, where it would
    /// otherwise stand still.
    fn list(&self) -> std::sync::MutexGuard<'_, Vec<(AccountId, Weak<SyncService>)>> {
        debug_assert!(!TELLING.get(), "an account asked the registry while it was being told the hold");
        // A panic under the lock leaves the list as it was: every change of it is one call
        // on the vector. The daemon's stop reads it, and must not fail on a poisoned lock.
        crate::panic::lock(&self.accounts)
    }

    /// The hold's settings every account runs on now.
    pub fn hold_settings(&self) -> HoldSettings {
        crate::panic::lock(&self.told).hold
    }

    /// The hold's settings, one pair for every account (`Accounts.SetPauseOnMetered`,
    /// `SetOnBattery`): every account works its hold out again, and a change ends its
    /// `SyncAnyway`.
    pub fn set_hold_settings(&self, hold: HoldSettings) {
        self.tell(|told| told.hold = hold);
    }

    /// What the machine's sources say now: every account works its hold out again.
    pub fn set_conditions(&self, conditions: Conditions) {
        if self.tell(|told| told.conditions = conditions) {
            tracing::info!(
                "the machine is {}metered, on {}{}",
                if conditions.metered { "" } else { "not " },
                if conditions.on_battery { "battery" } else { "mains power" },
                if conditions.power_saver { ", in power-saver mode" } else { "" }
            );
        }
    }

    /// Takes `change` into what every account is told, and tells them when it changed
    /// anything; whether it did. Told under the accounts' lock, so that two changes reach
    /// every account in the order they were made, and an account added meanwhile misses
    /// neither.
    fn tell(&self, change: impl FnOnce(&mut Told)) -> bool {
        let accounts = self.list();
        let now = {
            let mut told = crate::panic::lock(&self.told);
            let before = *told;
            change(&mut told);
            if *told == before {
                return false;
            }
            *told
        };
        let _telling = Telling::begin();
        for account in accounts.iter().filter_map(|(_, a)| a.upgrade()) {
            alone(&account, "taking the hold's settings", || account.hold_by(now.hold, now.conditions));
        }
        true
    }

    /// The label of an account other than `me` whose folder `path` is, is
    /// inside, or contains (`docs/design/accounts.md` §6.3): compared by component on the
    /// resolved paths, and by `(st_dev, st_ino)` for the same directory
    /// reached another way. A folder counts whether it is registered, held,
    /// or only recorded in `config.toml`.
    ///
    /// The paths are looked at on a blocking thread, in one section. A section the runtime
    /// gave up at its own end is an `Err`: the check was not made, and the registration is
    /// refused.
    pub(super) async fn overlapping(self: &Arc<Self>, me: &AccountId, path: &Path) -> Result<Option<String>, super::SyncError> {
        let (registry, me, path) = (Arc::clone(self), me.clone(), path.to_owned());
        match tokio::task::spawn_blocking(move || registry.overlapping_blocking(&me, &path)).await {
            Ok(found) => Ok(found),
            // As before the section was one: a panic in the check is the caller's.
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(_) => Err(super::SyncError::Stopping),
        }
    }

    /// [`overlapping`](Self::overlapping)'s work.
    fn overlapping_blocking(&self, me: &AccountId, path: &Path) -> Option<String> {
        let resolved = std::fs::canonicalize(path).ok()?;
        let identity = |path: &Path| std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()));
        let here = identity(&resolved);
        self.others(me).into_iter().find_map(|other| {
            let nests = other.folders().iter().any(|folder| {
                resolved.starts_with(folder) || folder.starts_with(&resolved) || (here.is_some() && identity(folder) == here)
            });
            nests.then(|| other.label())
        })
    }
}

/// Tells one account something, on a task that tells every account: a panic of `tell` is
/// caught and written to the journal, so that the accounts after it are told too and the
/// task lives (the watchers' ends stop the daemon).
fn alone(account: &SyncService, what: &str, tell: impl FnOnce()) {
    if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(tell)) {
        tracing::error!("{what} panicked in the account {}: {}", account.id(), crate::panic::message(panic));
    }
}

/// What the watchers of the network and the power sources tell every account
/// (`conditions`).
impl crate::conditions::Accounts for Registry {
    fn set_conditions(&self, conditions: Conditions) {
        Registry::set_conditions(self, conditions);
    }

    fn refresh_now(&self) {
        for account in self.accounts() {
            alone(&account, "asking for a cycle", || account.refresh_now());
        }
    }
}
