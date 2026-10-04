//! The one link to konedrive-helper, shared by every account (design §2.1–§2.4).
//!
//! The helper sends a uid's opens to that uid's newest connection only, so one daemon keeps
//! one link for all its accounts. [`HelperHub`] holds it — with its supervisor, `HelperState`,
//! the daemon-wide per-inode locks and the helper's socket — and each account's
//! [`SyncService`] holds the hub. On connect the hub resumes every account in turn, then
//! serves hydration requests; on loss it tells every account at once. A request carries
//! nothing about accounts: the [router](HelperHub::route) finds the account whose folder
//! the file is in.

use std::collections::HashSet;
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::XATTR_ITEM_ID;
use nix::fcntl::{openat2, OFlag, OpenHow, ResolveFlag};
use tokio::sync::{watch, Notify};
use xattr::FileExt;

use crate::helper::{Clearance, HelperError, HelperLink, HydrateRequest};
use crate::helper::status::{HelperState, HelperUnit};
use crate::helper::LinkCell;
use crate::hydration::source::ContentSource;
use crate::hydration::server::{serve, Filler, Fillers, Router};
use crate::folder::locks::{InodeKey, InodeLocks};
use super::{SyncService, MAX_HELPER_BACKOFF};

/// The link to the helper and everything that goes with it, for every account of one
/// daemon.
pub struct HelperHub {
    /// Replaceable, because the helper can go away and come back:
    /// [`supervise`] swaps it for `None` the moment the connection drops and
    /// back to a live link when it reconnects. Shared with every account's
    /// folder, whose sync reads it at every reconcile.
    link: LinkCell,
    /// One inode belongs to one account only, so one table serves them all:
    /// a fill on open, a `Hydrate` and a free-up of the same inode never run
    /// at once, whichever account asked.
    locks: InodeLocks,
    /// Where the helper's socket is, for local rule: with no
    /// link, a punch first looks there to see whether a helper — and so a
    /// fanotify group that could hold a mark — exists at all. Set by
    /// [`supervise`] to the path it connects to.
    socket: Mutex<PathBuf>,
    /// What `HelperState` asks while there is no link (HS1). Starts as
    /// [`helper_status::NotAsked`], which asks nothing: only `main` installs
    /// systemd, so no test reaches the system bus.
    unit: Mutex<Arc<dyn HelperUnit>>,
    /// Told whenever the link comes or goes, so [`watch`] asks again at once.
    changed: Arc<Notify>,
    /// `Accounts.HelperState`; every account's snapshot has a copy, which
    /// its `LastError` is worked out from.
    state: watch::Sender<HelperState>,
    /// Held while `HelperState` is published, so that a link published while
    /// systemd is asked wins, and an account that joins misses nothing.
    publishing: Mutex<()>,
    /// Every account, in account order: the order they are resumed in.
    accounts: Mutex<Vec<Weak<SyncService>>>,
    /// Held by every new registration, whichever account makes it, from its
    /// overlap check to its end: two accounts cannot both pass the check
    /// with folders that nest (design §8.3).
    pub(super) registering: tokio::sync::Mutex<()>,
    /// The item ids of what left each account's folder and waits in its
    /// outbox (`move-out` rows, and what the base has inside a moved-out
    /// folder): a fill of one of these is that account's, wherever the object
    /// is now — outside every folder, or inside another account's (write
    /// design §4.6, §8.5).
    moved_out: Mutex<Vec<(Weak<SyncService>, HashSet<String>)>>,
    /// What the machine's sources say (`conditions`): every account is told, and one
    /// that joins later is told what they say then.
    conditions: Mutex<crate::conditions::running::Conditions>,
    /// The hold's settings, one pair for every account (issue #95): every account is told,
    /// and one that joins later is told what they are then.
    hold: Mutex<crate::conditions::running::HoldSettings>,
}

impl HelperHub {
    pub fn new() -> Arc<Self> {
        Self::with_link(None)
    }

    /// A hub that holds `link` already (tests).
    pub fn with_link(link: Option<HelperLink>) -> Arc<Self> {
        let state = if link.is_some() { HelperState::Connected } else { HelperState::Unknown };
        Arc::new(Self {
            link: Arc::new(Mutex::new(link)),
            locks: InodeLocks::new(),
            socket: Mutex::new(PathBuf::from(konedrive_proto::SOCKET_PATH)),
            unit: Mutex::new(Arc::new(crate::helper::status::NotAsked)),
            changed: Arc::new(Notify::new()),
            state: watch::Sender::new(state),
            publishing: Mutex::new(()),
            accounts: Mutex::new(Vec::new()),
            registering: tokio::sync::Mutex::new(()),
            moved_out: Mutex::new(Vec::new()),
            conditions: Mutex::new(crate::conditions::running::Conditions::default()),
            hold: Mutex::new(crate::conditions::running::HoldSettings::default()),
        })
    }

    /// The item ids whose fills are `account`'s wherever the objects are now:
    /// what its outbox's `move-out` rows name (`docs/design/writes.md` §8). Replaces
    /// what it said before.
    pub(super) fn set_moved_out(&self, account: &Weak<SyncService>, ids: HashSet<String>) {
        let mut moved_out = self.moved_out.lock().unwrap();
        moved_out.retain(|(a, _)| a.strong_count() > 0 && !a.ptr_eq(account));
        if !ids.is_empty() {
            moved_out.push((account.clone(), ids));
        }
    }

    /// Whether an account other than `me` claims item `id` (write design
    /// §8.3): its outbox waits to fetch it wherever it is
    /// (`move-out`), its tree store knows it, or the drive the id names
    /// (`<drive>!<n>`, a personal account's) is that account's. A store that
    /// cannot be read claims it. An account with no store open says nothing:
    /// what it would miss, its own move-out keeps (`move_out::kept`).
    /// Blocking: a reconcile asks from its own thread.
    pub(super) fn claimed_elsewhere(&self, me: &Weak<SyncService>, id: &str) -> bool {
        let others = |a: &Weak<SyncService>| !a.ptr_eq(me);
        if self.moved_out.lock().unwrap().iter().any(|(a, ids)| others(a) && ids.contains(id)) {
            return true;
        }
        let drive = id.split_once('!').map(|(drive, _)| drive.to_owned());
        self.accounts().into_iter().filter(|a| !std::ptr::eq(Arc::as_ptr(a), me.as_ptr())).any(|other| {
            let Some(store) = other.store.lock().unwrap().clone() else { return false };
            let (id, drive) = (id.to_owned(), drive.clone());
            store
                .call_blocking(move |s| {
                    let known = s.get(konedrive_tree::Table::Items, &id)?.is_some() || s.get(konedrive_tree::Table::Staging, &id)?.is_some();
                    let ours = drive.as_deref().is_some_and(|d| s.meta("drive_id").ok().flatten().is_some_and(|m| m.eq_ignore_ascii_case(d)));
                    Ok(known || ours)
                })
                .unwrap_or(true)
        })
    }

    /// The account whose moved-out objects include the file behind `fd`, by
    /// the item id it carries. Nothing is read while no account has any.
    async fn by_moved_out(&self, fd: &OwnedFd) -> Option<Arc<SyncService>> {
        if self.moved_out.lock().unwrap().is_empty() {
            return None;
        }
        let id = item_id_of(fd).await?;
        self.moved_out.lock().unwrap().iter().find(|(_, ids)| ids.contains(&id)).and_then(|(a, _)| a.upgrade())
    }

    /// The live helper link, if there is one right now.
    pub fn link(&self) -> Option<HelperLink> {
        self.link.lock().unwrap().clone()
    }

    pub(super) fn link_cell(&self) -> LinkCell {
        Arc::clone(&self.link)
    }

    /// The lock table every fill and free-up of every account shares.
    pub fn locks(&self) -> InodeLocks {
        self.locks.clone()
    }

    /// Where the helper's socket is. Defaults to `konedrive_proto::SOCKET_PATH`.
    pub fn set_socket(&self, path: impl Into<PathBuf>) {
        *self.socket.lock().unwrap() = path.into();
    }

    /// What a punch goes by when nothing ties it to a link of its own
    /// (local rule, on [`Clearance`]): the live link if there
    /// is one, the helper's socket if not.
    pub(super) fn clearance(&self) -> Clearance {
        match self.link() {
            Some(link) => Clearance::Link(link),
            None => Clearance::NoLink(self.socket.lock().unwrap().clone()),
        }
    }

    /// What `HelperState` asks while there is no link (HS1): `main`
    /// installs systemd ([`helper_status::Systemd`]); a test, a fake.
    pub fn set_unit(&self, unit: Arc<dyn HelperUnit>) {
        *self.unit.lock().unwrap() = unit;
        self.changed.notify_one();
    }

    /// `HelperState` (HS1).
    pub fn state(&self) -> HelperState {
        *self.state.borrow()
    }

    /// `HelperState` as it changes (`Accounts`'s `PropertiesChanged`).
    pub fn subscribe(&self) -> watch::Receiver<HelperState> {
        self.state.subscribe()
    }

    /// Publishes a new helper link, or its loss — and so `HelperState`
    /// (HS1): `connected` at once, or, on a loss, `unknown` until [`watch`]
    /// has asked systemd. Every account sees the same.
    pub fn set_link(&self, link: Option<HelperLink>) {
        let now = if link.is_some() { HelperState::Connected } else { HelperState::Unknown };
        let _publishing = self.publishing.lock().unwrap();
        *self.link.lock().unwrap() = link;
        self.publish(now);
        drop(_publishing);
        self.changed.notify_one();
    }

    /// Works `HelperState` out again: `connected` while there is a link,
    /// else what systemd says of the unit. A link that came up while systemd
    /// was being asked wins.
    pub async fn check(&self) {
        if self.link().is_some() {
            let _publishing = self.publishing.lock().unwrap();
            self.publish(HelperState::Connected);
            return;
        }
        let unit = Arc::clone(&self.unit.lock().unwrap());
        let found = match unit.states().await {
            Some((load, active)) => HelperState::of_unit(&load, &active),
            None => HelperState::Unknown,
        };
        let _publishing = self.publishing.lock().unwrap();
        if self.link().is_none() {
            self.publish(found);
        }
    }

    /// `HelperState` for the hub and every account, with `publishing` held.
    fn publish(&self, now: HelperState) {
        self.state.send_if_modified(|state| std::mem::replace(state, now) != now);
        for account in self.accounts() {
            account.state().update(|s| s.helper_state = now);
        }
    }

    /// Makes `new` one of the hub's accounts, after every other; `new` gets the
    /// `HelperState` of now. [`SyncService::on_hub`]'s.
    pub(super) fn join(&self, new: impl FnOnce(HelperState) -> Arc<SyncService>) -> Arc<SyncService> {
        let _publishing = self.publishing.lock().unwrap();
        let account = new(self.state());
        let mut accounts = self.accounts.lock().unwrap();
        accounts.retain(|a| a.strong_count() > 0);
        accounts.push(Arc::downgrade(&account));
        // Under the accounts' lock, as `set_conditions` tells them: none is missed.
        account.set_conditions(*self.conditions.lock().unwrap());
        account.set_hold_settings(*self.hold.lock().unwrap());
        account
    }

    /// The hold's settings every account runs on now.
    pub fn hold_settings(&self) -> crate::conditions::running::HoldSettings {
        *self.hold.lock().unwrap()
    }

    /// The hold's settings, one pair for every account (`Accounts.SetPauseOnMetered`,
    /// `SetOnBattery`): every account works its hold out again, and a change ends its
    /// `SyncAnyway`.
    pub fn set_hold_settings(&self, hold: crate::conditions::running::HoldSettings) {
        let accounts = {
            let _accounts = self.accounts.lock().unwrap();
            let mut kept = self.hold.lock().unwrap();
            if *kept == hold {
                return;
            }
            *kept = hold;
            drop(kept);
            _accounts.iter().filter_map(Weak::upgrade).collect::<Vec<_>>()
        };
        for account in accounts {
            account.set_hold_settings(hold);
        }
    }

    /// What the machine's sources say now: every account works its hold out again.
    pub fn set_conditions(&self, conditions: crate::conditions::running::Conditions) {
        let accounts = {
            let _accounts = self.accounts.lock().unwrap();
            let mut kept = self.conditions.lock().unwrap();
            if *kept == conditions {
                return;
            }
            *kept = conditions;
            drop(kept);
            _accounts.iter().filter_map(Weak::upgrade).collect::<Vec<_>>()
        };
        tracing::info!(
            "the machine is {}metered, on {}{}",
            if conditions.metered { "" } else { "not " },
            if conditions.on_battery { "battery" } else { "mains power" },
            if conditions.power_saver { ", in power-saver mode" } else { "" }
        );
        for account in accounts {
            account.set_conditions(conditions);
        }
    }

    /// `account` is not one of the hub's any more (an account removed).
    pub fn leave(&self, account: &SyncService) {
        self.accounts.lock().unwrap().retain(|a| a.upgrade().is_some_and(|a| !std::ptr::eq(Arc::as_ptr(&a), account)));
    }

    /// Every account, in account order.
    pub fn accounts(&self) -> Vec<Arc<SyncService>> {
        self.accounts.lock().unwrap().iter().filter_map(Weak::upgrade).collect()
    }

    /// The label of an account other than `me` whose folder `path` is, is
    /// inside, or contains (design §8.3): compared by component on the
    /// resolved paths, and by `(st_dev, st_ino)` for the same directory
    /// reached another way. A folder counts whether it is registered, held,
    /// or only recorded in `config.toml`.
    ///
    /// The paths are looked at on a blocking thread, in one section.
    pub(super) async fn overlapping(self: &Arc<Self>, me: &SyncService, path: &Path) -> Option<String> {
        // Only compared, never read: the caller holds `me` across the call.
        let (hub, me, path) = (Arc::clone(self), std::ptr::from_ref(me) as usize, path.to_owned());
        match tokio::task::spawn_blocking(move || hub.overlapping_blocking(me, &path)).await {
            Ok(found) => found,
            // As before the section was one: a panic in the check is the caller's.
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(_) => None,
        }
    }

    /// [`overlapping`](Self::overlapping)'s work; `me` is the asking account's address.
    fn overlapping_blocking(&self, me: usize, path: &Path) -> Option<String> {
        let resolved = std::fs::canonicalize(path).ok()?;
        let identity = |path: &Path| std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()));
        let here = identity(&resolved);
        self.accounts().into_iter().filter(|other| Arc::as_ptr(other) as usize != me).find_map(|other| {
            let nests = other.folders().iter().any(|folder| {
                resolved.starts_with(folder) || folder.starts_with(&resolved) || (here.is_some() && identity(folder) == here)
            });
            nests.then(|| other.label())
        })
    }

    /// Which account the file behind a hydration request's descriptor belongs to
    /// (design §2.4), stopping at the first answer: by device — the accounts
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

    /// A descriptor for the object `handle` names, from the helper
    /// (`OpenByHandle`, `docs/design/writes.md` §8.2): for an item gone from its folder,
    /// to learn where it went and to keep and fill a placeholder that left.
    /// `dir` is a directory of this user's on the object's filesystem, such as
    /// the folder's root. See [`HelperLink::open_by_handle`] for what comes
    /// back; `NotRunning` while there is no link.
    pub async fn open_by_handle(&self, dir: &File, handle: &FileHandle) -> Result<OwnedFd, HelperError> {
        let link = self.link().ok_or(HelperError::NotRunning)?;
        link.open_by_handle(dir, handle).await
    }

    /// Takes the helper's mark off a directory (`UnmarkDir`): one moved out of
    /// the folder, whose marks travelled with it (design §4.6), once what it
    /// held is downloaded. Any directory of this user's on a device the helper
    /// has a root of theirs on, wherever it is now; `NotRunning` while there
    /// is no link.
    pub async fn unmark_dir(&self, dir: &File) -> Result<(), HelperError> {
        let link = self.link().ok_or(HelperError::NotRunning)?;
        link.unmark_dir(dir).await
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
    let shown = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).ok()?;
    candidates
        .iter()
        .find(|account| {
            let Some(reg) = account.registration() else { return false };
            let Ok(rel) = shown.strip_prefix(&reg.root.path) else { return false };
            if rel.as_os_str().is_empty() {
                return false;
            }
            let Ok(Some(dir)) = reg.root.open_registered() else { return false };
            let how = OpenHow::new()
                .flags(OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
                .resolve(ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_SYMLINKS | ResolveFlag::RESOLVE_NO_MAGICLINKS);
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
        let Ok(lifecycle) = Arc::clone(&account.lifecycle).try_read_owned() else { continue };
        let Some(store) = account.store.lock().unwrap().clone() else { continue };
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

/// Keeps a helper link alive for the life of the daemon.
///
/// Connects, brings every account's folder up on that link — one after
/// another, in account order: each re-registers its root, whose walk the
/// helper performs, then recovers it — serves hydration requests until the
/// connection drops, publishes the drop to every account at once — not once
/// the downloads under way have finished — and tries again after a backoff
/// that grows to a cap. Nothing reconnected before: when the helper went
/// away `serve_hydrations` simply returned, `RootState` stayed `ready` with
/// `LastError` empty, and every un-hydrated file in the folder read as zeros
/// with nothing saying so.
///
/// `backoff` is the first delay; each failure doubles it up to
/// `MAX_HELPER_BACKOFF`. A successful connection resets it.
pub async fn supervise(hub: Arc<HelperHub>, socket_path: PathBuf, backoff: Duration) {
    let mut wait = backoff;
    loop {
        match HelperLink::connect(&socket_path).await {
            Ok((link, requests)) => {
                wait = backoff;
                tracing::info!("connected to the konedrive helper at {}", socket_path.display());
                hub.set_socket(&socket_path);
                hub.set_link(Some(link.clone()));
                // Re-register every root before serving anything: a helper
                // that has just started has no marks at all, and a root's
                // own registration is what puts them back.
                for account in hub.accounts() {
                    account.resume().await;
                }
                // A task of its own, and the end of the connection is
                // waited for on the link itself. Awaiting
                // `serve_hydrations` here waited for every fill still
                // running as well — a download of any length — and until
                // then the loss was not published, the dead link was still
                // handed out, and nothing reconnected. The fills already
                // running finish in that task, their `HydrateDone` going
                // nowhere; the per-inode locks keep each of them ahead of
                // any fill of the same file on the next connection.
                let serving = tokio::spawn(serve_routed(link.clone(), requests, Arc::clone(&hub)));
                link.closed().await;
                tracing::error!("the konedrive helper connection dropped");
                hub.set_link(None);
                for account in hub.accounts() {
                    account.report_helper_lost();
                }
                drop(serving);
            }
            Err(e) => {
                tracing::warn!(
                    "cannot connect to the konedrive helper at {}: {e}; retrying in {:?}",
                    socket_path.display(),
                    wait
                );
            }
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(MAX_HELPER_BACKOFF);
    }
}

/// Answers hydration requests for every account, each filled by the account its file
/// belongs to ([`HelperHub::route`]), until the helper goes away.
pub(super) async fn serve_routed(link: HelperLink, requests: tokio::sync::mpsc::Receiver<HydrateRequest>, hub: Arc<HelperHub>) {
    let locks = hub.locks();
    serve(link, requests, locks, Fillers::Routed(hub)).await;
}

/// Keeps `HelperState` current for the life of the daemon (HS1): worked out
/// again whenever the link comes or goes, and every
/// [`helper_status::RECHECK`] while there is none — a helper installed,
/// started or failed meanwhile shows within that.
pub async fn watch(hub: Arc<HelperHub>) {
    watch_every(hub, crate::helper::status::RECHECK).await
}

/// [`watch`], asking systemd again every `every` while there is no link
/// (tests: well under a second).
pub async fn watch_every(hub: Arc<HelperHub>, every: Duration) {
    let changed = Arc::clone(&hub.changed);
    loop {
        hub.check().await;
        if hub.link().is_some() {
            changed.notified().await;
        } else {
            tokio::select! {
                () = changed.notified() => {}
                () = tokio::time::sleep(every) => {}
            }
        }
    }
}

/// The device a folder is on, read once when its registration is made (`Registration::dev`),
/// for [`HelperHub::route`]; `None` when it cannot be looked at. Read on a blocking thread.
pub(super) async fn device_of(path: &Path) -> Option<u64> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || std::fs::metadata(path).ok().map(|m| m.dev())).await.ok().flatten()
}

/// A content source is what an account is, to the fill loop.
pub(crate) fn filler(account: Arc<SyncService>) -> Filler {
    let report = account.report().clone();
    let pool = Arc::clone(account.pool());
    (account as Arc<dyn ContentSource>, report, pool)
}

/// The fill loop's routing ([`HelperHub::route`]).
#[async_trait::async_trait]
impl Router for HelperHub {
    async fn route(&self, fd: &OwnedFd) -> Option<Filler> {
        HelperHub::route(self, fd).await.map(filler)
    }
}

/// What the watchers of the network and the power sources tell every account
/// (`conditions`).
impl crate::conditions::Accounts for HelperHub {
    fn set_conditions(&self, conditions: crate::conditions::running::Conditions) {
        HelperHub::set_conditions(self, conditions);
    }

    fn refresh_now(&self) {
        for account in self.accounts() {
            account.refresh_now();
        }
    }
}

#[cfg(test)]
mod tests;
