//! The folders of every account of one daemon, and what they share (`docs/design/accounts.md` §3.1–§3.4).
//!
//! [`Registry`] is the list of the daemon's accounts as their folders see each other: whose
//! folder an open file is in (the [router](Registry::route)), whether a folder would overlap
//! another account's, whose an item id is, the per-inode locks, and what the machine's
//! sources and the hold's settings say. It holds the daemon's one link to the helper
//! (`helper::hub`), which tells it as the link comes and goes ([`Served`]): on connect every
//! account is resumed in turn, then hydration requests are served; on loss every account is
//! told at once. A request carries nothing about accounts: the router finds the account
//! whose folder the file is in.
//!
//! The list has one writer: `daemon::manager`, which adds an account in the lines that add
//! it to its own list and removes it where it removes it there. A service does not put
//! itself on it.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

use konedrive_fs::placeholder::XATTR_ITEM_ID;
use konedrive_fs::proc_path;
use nix::fcntl::{openat2, OFlag, OpenHow};
use xattr::FileExt;

use super::SyncService;
use crate::conditions::running::{Conditions, HoldSettings};
use crate::config::AccountId;
use crate::folder::disk::beneath;
use crate::folder::locks::{InodeKey, InodeLocks};
use crate::helper::hub::{HelperHub, Served};
use crate::helper::status::HelperState;
use crate::helper::{HelperLink, HydrateRequest};
use crate::hydration::server::{serve, Filler, Fillers, Router};
use crate::hydration::source::ContentSource;

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

/// The daemon's accounts, as their folders see each other, and the link they share.
pub struct Registry {
    /// The one link to the helper.
    hub: Arc<HelperHub>,
    /// One inode belongs to one account only, so one table serves them all:
    /// a fill on open, a `Hydrate` and a free-up of the same inode never run
    /// at once, whichever account asked.
    locks: InodeLocks,
    /// Every account, in account order: the order they are resumed in. Weak, because a
    /// service holds the registry; an entry is taken out by whoever put it in.
    accounts: Mutex<Vec<(AccountId, Weak<SyncService>)>>,
    /// Held by every new registration, whichever account makes it, from its
    /// overlap check to its end: two accounts cannot both pass the check
    /// with folders that nest (§6.3).
    pub(super) registering: tokio::sync::Mutex<()>,
    /// The item ids of what left each account's folder and waits in its
    /// outbox (`move-out` rows, and what the base has inside a moved-out
    /// folder): a fill of one of these is that account's, wherever the object
    /// is now — outside every folder, or inside another account's (`docs/design/writes.md`
    /// §8.3).
    moved_out: Mutex<HashMap<AccountId, HashSet<String>>>,
    /// What every account was told last; one that is added later is told what it is then.
    told: Mutex<Told>,
}

impl Registry {
    /// No account yet, and no link.
    pub fn new() -> Arc<Self> {
        Self::with_link(None)
    }

    /// A registry whose hub holds `link` already (tests).
    pub fn with_link(link: Option<HelperLink>) -> Arc<Self> {
        Arc::new_cyclic(|me: &Weak<Self>| Self {
            hub: HelperHub::new(link, me.clone() as Weak<dyn Served>),
            locks: InodeLocks::new(),
            accounts: Mutex::new(Vec::new()),
            registering: tokio::sync::Mutex::new(()),
            moved_out: Mutex::new(HashMap::new()),
            told: Mutex::new(Told::default()),
        })
    }

    /// The link to the helper every account shares.
    pub fn hub(&self) -> &Arc<HelperHub> {
        &self.hub
    }

    /// The lock table every fill and free-up of every account shares.
    pub fn locks(&self) -> InodeLocks {
        self.locks.clone()
    }

    /// Makes `account` one of the daemon's, after every other: it is told the
    /// `HelperState`, the conditions and the hold's settings of now, and misses no later
    /// change of them. The account manager's, in the lines that list the account.
    pub fn add(&self, account: &Arc<SyncService>) {
        let mut accounts = self.list();
        accounts.retain(|(id, a)| a.strong_count() > 0 && id != account.id());
        // Under the accounts' lock, as every later change is told: none is missed.
        let state = self.hub.state();
        account.state().update(|s| s.folder.helper_state = state);
        let told = *crate::panic::lock(&self.told);
        {
            let _telling = Telling::begin();
            account.hold_by(told.hold, told.conditions);
        }
        accounts.push((account.id().clone(), Arc::downgrade(account)));
    }

    /// The account `id` is not one of the daemon's any more (an account removed): nothing
    /// is routed to it, and it claims nothing.
    pub fn remove(&self, id: &AccountId) {
        self.list().retain(|(a, _)| a != id);
        crate::panic::lock(&self.moved_out).remove(id);
    }

    /// Every account, in account order.
    pub fn accounts(&self) -> Vec<Arc<SyncService>> {
        self.list().iter().filter_map(|(_, a)| a.upgrade()).collect()
    }

    /// Every account but `me`.
    fn others(&self, me: &AccountId) -> Vec<Arc<SyncService>> {
        self.list().iter().filter(|(id, _)| id != me).filter_map(|(_, a)| a.upgrade()).collect()
    }

    /// The account `id`, while it is one of the daemon's.
    fn account(&self, id: &AccountId) -> Option<Arc<SyncService>> {
        self.list().iter().find(|(a, _)| a == id).and_then(|(_, a)| a.upgrade())
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

    /// The item ids whose fills are the account `me`'s wherever the objects are now:
    /// what its outbox's `move-out` rows name (`docs/design/writes.md` §8). Replaces
    /// what it said before.
    pub(super) fn set_moved_out(&self, me: &AccountId, ids: HashSet<String>) {
        let mut moved_out = crate::panic::lock(&self.moved_out);
        if ids.is_empty() {
            moved_out.remove(me);
        } else {
            moved_out.insert(me.clone(), ids);
        }
    }

    /// Whether an account other than `me` claims item `id` (`docs/design/writes.md`
    /// §8.3): its outbox waits to fetch it wherever it is
    /// (`move-out`), its tree store knows it, or the drive the id names
    /// (`<drive>!<n>`, a personal account's) is that account's. A store that
    /// cannot be read claims it. The store is kept with the folder while it is recorded, up or down,
    /// whether or not its sync runs. An account with no store open says nothing:
    /// what it would miss, its own move-out keeps (`move_out::kept`).
    /// Blocking: a reconcile asks from its own thread.
    pub(super) fn claimed_elsewhere(&self, me: &AccountId, id: &str) -> bool {
        if crate::panic::lock(&self.moved_out).iter().any(|(account, ids)| account != me && ids.contains(id)) {
            return true;
        }
        let drive = id.split_once('!').map(|(drive, _)| drive.to_owned());
        self.others(me).into_iter().any(|other| {
            let Some(store) = other.tree_store() else { return false };
            let (id, drive) = (id.to_owned(), drive.clone());
            store
                .call_blocking(move |s| {
                    let known = s.get(konedrive_tree::Table::Items, &id)?.is_some() || s.get(konedrive_tree::Table::Staging, &id)?.is_some();
                    let ours = drive.as_deref().is_some_and(|d| s.drive_id().ok().flatten().is_some_and(|m| m.eq_ignore_ascii_case(d)));
                    Ok(known || ours)
                })
                .unwrap_or(true)
        })
    }

    /// The account whose moved-out objects include the file behind `fd`, by
    /// the item id it carries. Nothing is read while no account has any.
    async fn by_moved_out(&self, fd: &OwnedFd) -> Option<Arc<SyncService>> {
        if crate::panic::lock(&self.moved_out).is_empty() {
            return None;
        }
        let id = item_id_of(fd).await?;
        let owner = crate::panic::lock(&self.moved_out).iter().find(|(_, ids)| ids.contains(&id)).map(|(account, _)| account.clone())?;
        self.account(&owner)
    }

    /// Every folder every account holds or records.
    pub(super) fn folders(&self) -> Vec<std::path::PathBuf> {
        self.accounts().iter().flat_map(|a| a.folders()).collect()
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

    /// Which account the file behind a hydration request's descriptor belongs to
    /// (§3.4), stopping at the first answer: by the item id of an object that
    /// left an account's folder (below); by device — the accounts
    /// whose folder is on the file's filesystem (as it was when registered),
    /// and one is the answer, unless some account has a folder whose device is
    /// not known, which could be the file's; then by
    /// path, verified — the name the kernel has for the file, opened beneath
    /// a candidate's folder, must be the same inode; then by item id — the
    /// file's `user.konedrive.item-id` in a candidate's tree store, for a file
    /// renamed or unlinked while its open was suspended. `None` otherwise:
    /// routing never guesses.
    pub(crate) async fn route(&self, fd: &OwnedFd) -> Option<Arc<SyncService>> {
        // An object that left an account's folder is that account's, by its
        // item id, whatever folder its path is in now (`docs/design/writes.md` §8, §8.3).
        if let Some(account) = self.by_moved_out(fd).await {
            return Some(account);
        }
        let key = InodeKey::of_fd(fd).ok()?;
        let accounts = self.accounts();
        // A folder whose device is not known — held back, or written down by
        // a registration not made yet — could be the file's: then even one
        // candidate by device is verified, never taken on trust.
        let unplaced = accounts.iter().any(|account| account.has_unplaced_folder());
        let mut candidates: Vec<Arc<SyncService>> =
            accounts.into_iter().filter(|account| account.root_device() == Some(key.dev)).collect();
        if candidates.is_empty() || (candidates.len() == 1 && !unplaced) {
            return candidates.pop();
        }
        if let Some(found) = by_path(&candidates, fd, key).await {
            return Some(found);
        }
        by_item_id(candidates, fd).await
    }
}

/// The item id the file behind `fd` carries, read on a blocking thread.
async fn item_id_of(fd: &OwnedFd) -> Option<String> {
    let file = File::from(fd.try_clone().ok()?);
    tokio::task::spawn_blocking(move || file.get_xattr(XATTR_ITEM_ID).ok().flatten())
        .await
        .ok()
        .flatten()
        .and_then(|raw| String::from_utf8(raw).ok())
}

/// The candidate whose folder holds the name the kernel has for `fd` — proved by opening
/// that name beneath the folder and finding the same inode. One section on a blocking
/// thread, over a descriptor of its own for the same open file.
async fn by_path(candidates: &[Arc<SyncService>], fd: &OwnedFd, key: InodeKey) -> Option<Arc<SyncService>> {
    let (candidates, fd) = (candidates.to_vec(), fd.try_clone().ok()?);
    tokio::task::spawn_blocking(move || by_path_blocking(&candidates, &fd, key)).await.ok().flatten()
}

fn by_path_blocking(candidates: &[Arc<SyncService>], fd: &OwnedFd, key: InodeKey) -> Option<Arc<SyncService>> {
    let shown = std::fs::read_link(proc_path(fd)).ok()?;
    candidates
        .iter()
        .find(|account| {
            let Some(reg) = account.record() else { return false };
            let Ok(rel) = shown.strip_prefix(&reg.root.path) else { return false };
            if rel.as_os_str().is_empty() {
                return false;
            }
            let Ok(Some(dir)) = reg.root.open_registered() else { return false };
            let how = OpenHow::new()
                .flags(OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
                .resolve(beneath());
            openat2(dir.as_fd(), rel, how).is_ok_and(|found| InodeKey::of_fd(&found).is_ok_and(|k| k == key))
        })
        .cloned()
}

/// The candidate whose tree store knows the item id `fd` carries — in `items`, then
/// `staging`; item ids are unique across drives. A store in use by a registration or a
/// Forget right now is not waited for: that account is passed over.
async fn by_item_id(candidates: Vec<Arc<SyncService>>, fd: &OwnedFd) -> Option<Arc<SyncService>> {
    let id = item_id_of(fd).await?;
    for account in candidates {
        let Ok(lifecycle) = Arc::clone(&account.folder).try_read_owned() else { continue };
        let Some(store) = lifecycle.store() else { continue };
        let id = id.clone();
        let known = tokio::task::spawn_blocking(move || {
            let _lifecycle = lifecycle;
            store.call_blocking(move |s| {
                Ok(s.get(konedrive_tree::Table::Items, &id)?.is_some() || s.get(konedrive_tree::Table::Staging, &id)?.is_some())
            })
        })
        .await;
        if matches!(known, Ok(Ok(true))) {
            return Some(account);
        }
    }
    None
}

/// The device a folder is on, read once when its record is made (`Record::dev`),
/// for [`Registry::route`]; `None` when it cannot be looked at. Read on a blocking thread.
pub(super) async fn device_of(path: &Path) -> Option<u64> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || std::fs::metadata(path).ok().map(|m| m.dev())).await.ok().flatten()
}

/// What fills an open of a file of `account`'s: the source its folder has now, held for
/// the whole fill, so that a download under way ends as a download whatever becomes of
/// the folder meanwhile. A folder with no source — a local one not populated yet — fills
/// nothing, and says so.
fn filler(account: Arc<SyncService>) -> Filler {
    let report = account.report().clone();
    let pool = Arc::clone(account.pool());
    let source = account.content_source().unwrap_or_else(|| Arc::new(NoSource));
    (source, report, pool)
}

/// The source of a folder that has none.
struct NoSource;

#[async_trait::async_trait]
impl ContentSource for NoSource {
    async fn fetch(&self, item_id: &str, _from: u64, _end: Option<u64>) -> Result<crate::hydration::source::Fetched, crate::hydration::source::SourceError> {
        Err(crate::hydration::source::SourceError::NotFound(format!("{item_id}: no content source is registered")))
    }
}

/// Tells one account something, on a task that tells every account: a panic of `tell` is
/// caught and written to the journal, so that the accounts after it are told too and the
/// task lives (the supervisor's and the watchers' ends stop the daemon).
fn alone(account: &SyncService, what: &str, tell: impl FnOnce()) {
    if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(tell)) {
        tracing::error!("{what} panicked in the account {}: {}", account.id(), crate::panic::message(panic));
    }
}

/// The fill loop's routing ([`Registry::route`]).
#[async_trait::async_trait]
impl Router for Registry {
    async fn route(&self, fd: &OwnedFd) -> Option<Filler> {
        Registry::route(self, fd).await.map(filler)
    }
}

/// What the link tells its accounts as it comes and goes (`helper::hub::supervise`).
#[async_trait::async_trait]
impl Served for Registry {
    /// Every account's snapshot has a copy of `HelperState`, which its `LastError` is
    /// worked out from.
    fn helper_state(&self, now: HelperState) {
        for account in self.accounts() {
            alone(&account, "taking the helper's state", || account.state().update(|s| s.folder.helper_state = now));
        }
    }

    /// Every account's folder brought up on the new link, one after another, in account
    /// order: each re-registers its root, whose walk the helper performs, then recovers it.
    ///
    /// An account whose bring-up panics is left down, saying so, and the next account is
    /// brought up all the same: this runs on the supervisor's own task, whose end stops
    /// the daemon (`daemon::stop::Tasks`), and a defect that one account's folder meets
    /// would otherwise take every account down at each connect.
    async fn helper_back(&self) {
        use futures_util::FutureExt;
        for account in self.accounts() {
            let Err(panic) = std::panic::AssertUnwindSafe(account.resume()).catch_unwind().await else { continue };
            let panic = crate::panic::message(panic);
            tracing::error!("bringing up the folder of the account {} panicked: {panic}", account.id());
            if std::panic::AssertUnwindSafe(account.bring_up_panicked(&panic)).catch_unwind().await.is_err() {
                tracing::error!("the folder of the account {} could not be left down either; it stays as the panic left it", account.id());
            }
        }
    }

    /// Answers hydration requests for every account, each filled by the account its file
    /// belongs to ([`Registry::route`]), until the helper goes away.
    async fn serve(self: Arc<Self>, link: HelperLink, requests: tokio::sync::mpsc::Receiver<HydrateRequest>) {
        let locks = self.locks();
        serve(link, requests, locks, Fillers::Routed(self)).await;
    }

    fn helper_lost(&self) {
        for account in self.accounts() {
            alone(&account, "taking the helper's loss", || account.report_helper_lost());
        }
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

#[cfg(test)]
mod tests;
