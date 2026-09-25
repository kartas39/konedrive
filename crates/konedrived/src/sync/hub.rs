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

use super::helper::{Clearance, HelperError, HelperLink, HydrateRequest};
use super::helper_status::{self, HelperState, HelperUnit};
use super::listing::LinkCell;
use super::source::ContentSource;
use super::{serve, Fillers, InodeKey, InodeLocks, SyncService, MAX_HELPER_BACKOFF};

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
    /// `Accounts1.HelperState`; every account's snapshot has a copy, which
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
            unit: Mutex::new(Arc::new(helper_status::NotAsked)),
            changed: Arc::new(Notify::new()),
            state: watch::Sender::new(state),
            publishing: Mutex::new(()),
            accounts: Mutex::new(Vec::new()),
            registering: tokio::sync::Mutex::new(()),
            moved_out: Mutex::new(Vec::new()),
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
            store
                .with(|s| {
                    let known = s.get(crate::tree::Table::Items, id)?.is_some() || s.get(crate::tree::Table::Staging, id)?.is_some();
                    let ours = drive.as_deref().is_some_and(|d| s.meta("drive_id").ok().flatten().is_some_and(|m| m.eq_ignore_ascii_case(d)));
                    Ok(known || ours)
                })
                .unwrap_or(true)
        })
    }

    /// The account whose moved-out objects include the file behind `fd`, by
    /// the item id it carries. Nothing is read while no account has any.
    fn by_moved_out(&self, fd: &OwnedFd) -> Option<Arc<SyncService>> {
        if self.moved_out.lock().unwrap().is_empty() {
            return None;
        }
        let file = File::from(fd.try_clone().ok()?);
        let id = String::from_utf8(file.get_xattr(XATTR_ITEM_ID).ok()??).ok()?;
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

    /// `HelperState` as it changes (`Accounts1`'s `PropertiesChanged`).
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
        account
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
    pub(super) fn overlapping(&self, me: &SyncService, path: &Path) -> Option<String> {
        let resolved = std::fs::canonicalize(path).ok()?;
        let identity = |path: &Path| std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()));
        let here = identity(&resolved);
        self.accounts().into_iter().filter(|other| !std::ptr::eq(Arc::as_ptr(other), me)).find_map(|other| {
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
    pub(super) async fn route(&self, fd: &OwnedFd) -> Option<Arc<SyncService>> {
        // An object that left an account's folder is that account's, by its
        // item id, whatever folder its path is in now (`docs/design/writes.md` §8, §8.3).
        if let Some(account) = self.by_moved_out(fd) {
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
        if let Some(found) = by_path(&candidates, fd, key) {
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

/// The candidate whose folder holds the name the kernel has for `fd` — proved by opening
/// that name beneath the folder and finding the same inode.
fn by_path(candidates: &[Arc<SyncService>], fd: &OwnedFd, key: InodeKey) -> Option<Arc<SyncService>> {
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
    let file = File::from(fd.try_clone().ok()?);
    let id = tokio::task::spawn_blocking(move || file.get_xattr(XATTR_ITEM_ID).ok().flatten())
        .await
        .ok()
        .flatten()
        .and_then(|raw| String::from_utf8(raw).ok())?;
    for account in candidates {
        let Ok(lifecycle) = Arc::clone(&account.lifecycle).try_read_owned() else { continue };
        let Some(store) = account.store.lock().unwrap().clone() else { continue };
        let id = id.clone();
        let known = tokio::task::spawn_blocking(move || {
            let _lifecycle = lifecycle;
            store.with(|s| {
                Ok(s.get(crate::tree::Table::Items, &id)?.is_some() || s.get(crate::tree::Table::Staging, &id)?.is_some())
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
    watch_every(hub, helper_status::RECHECK).await
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
/// for [`HelperHub::route`]; `None` when it cannot be looked at.
pub(super) fn device_of(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.dev())
}

/// A content source is what an account is, to the fill loop.
pub(super) fn filler(account: Arc<SyncService>) -> (Arc<dyn ContentSource>, super::activity::Report) {
    let report = account.report().clone();
    (account as Arc<dyn ContentSource>, report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Change, Kind, Placement, Row, Store, TreeStore};

    /// An account on `hub` whose folder `dir` is registered without interception, with no
    /// helper anywhere.
    async fn account_at(hub: &Arc<HelperHub>, dir: &Path) -> Arc<SyncService> {
        let account = SyncService::on_hub(hub, None, None);
        hub.set_socket(dir.join("no-helper.sock"));
        std::fs::create_dir_all(dir).unwrap();
        account.register_root_without_interception(dir).await.unwrap();
        account
    }

    /// A file `name` in `dir` carrying item id `id`, opened as the helper hands one over.
    fn opened(dir: &Path, name: &str, id: &str) -> OwnedFd {
        std::fs::write(dir.join(name), b"").unwrap();
        xattr::set(dir.join(name), XATTR_ITEM_ID, id.as_bytes()).unwrap();
        File::open(dir.join(name)).unwrap().into()
    }

    fn same(a: &Option<Arc<SyncService>>, b: &Arc<SyncService>) -> bool {
        a.as_ref().is_some_and(|a| Arc::ptr_eq(a, b))
    }

    /// Design §2.4, step 1 (test 5): the one account whose folder is on the file's
    /// filesystem is the answer — a file moved out of its folder included. Needs a second
    /// filesystem (`/dev/shm`) beside the temporary directory's.
    #[tokio::test]
    async fn the_one_folder_on_the_files_filesystem_is_the_answer() {
        let (Ok(here), Ok(there)) = (tempfile::tempdir(), tempfile::tempdir_in("/dev/shm")) else {
            return eprintln!("no /dev/shm: nothing to test");
        };
        if device_of(here.path()) == device_of(there.path()) {
            return eprintln!("/dev/shm is on the temporary directory's filesystem: nothing to test");
        }
        let hub = HelperHub::new();
        let a = account_at(&hub, &here.path().join("A")).await;
        let b = account_at(&hub, &there.path().join("B")).await;

        assert!(same(&hub.route(&opened(&here.path().join("A"), "f", "1")).await, &a));
        assert!(same(&hub.route(&opened(&there.path().join("B"), "f", "2")).await, &b));
        assert!(same(&hub.route(&opened(here.path(), "moved-out", "3")).await, &a), "by device alone");
    }

    /// Steps 2 and 3: two folders on one filesystem are told apart by the name the kernel
    /// has for the file, verified by its inode — a file renamed while its open waits
    /// included — and, for a file unlinked meanwhile, by its item id in a tree store. A
    /// file in neither folder, whose id no store knows, is no one's.
    #[tokio::test]
    async fn two_folders_on_one_filesystem_are_told_apart_by_path_then_by_item_id() {
        let dir = tempfile::tempdir().unwrap();
        let (in_a, in_b) = (dir.path().join("A"), dir.path().join("B"));
        let hub = HelperHub::new();
        let a = account_at(&hub, &in_a).await;
        let b = account_at(&hub, &in_b).await;

        assert!(same(&hub.route(&opened(&in_a, "f", "1")).await, &a));
        let renamed = opened(&in_b, "f", "2");
        std::fs::rename(in_b.join("f"), in_b.join("g")).unwrap();
        assert!(same(&hub.route(&renamed).await, &b), "renamed while its open waited");

        // Both have a tree store; only B's knows the id.
        let store = |id: &str| {
            let row = Row {
                id: id.into(),
                parent_id: Some("ROOT".into()),
                name: "h".into(),
                kind: Kind::File,
                size: 0,
                mtime: 0,
                etag: None,
                ctag: None,
                quickxor: None,
                mime: None,
                placement: Placement::Placed,
            };
            let store = Store::new(TreeStore::in_memory().unwrap());
            store.with(|s| s.commit_page(&[Change::Upsert(row)], "next")).unwrap();
            store
        };
        *a.store.lock().unwrap() = Some(store("ITEM-A"));
        *b.store.lock().unwrap() = Some(store("ITEM-B"));
        let unlinked = opened(&in_b, "h", "ITEM-B");
        std::fs::remove_file(in_b.join("h")).unwrap();
        assert!(same(&hub.route(&unlinked).await, &b), "found by its item id");

        let nowhere = opened(dir.path(), "stray", "ITEM-X");
        assert!(hub.route(&nowhere).await.is_none(), "routing never guesses");
    }

    /// Write design §4.6, §8.5: the fill of an object that left an account's folder goes to that
    /// account, by its item id — even from inside another account's folder, which its path says.
    #[tokio::test]
    async fn a_moved_out_object_is_routed_by_its_item_id() {
        let dir = tempfile::tempdir().unwrap();
        let (in_a, in_b) = (dir.path().join("A"), dir.path().join("B"));
        let hub = HelperHub::new();
        let a = account_at(&hub, &in_a).await;
        let b = account_at(&hub, &in_b).await;
        let moved = opened(&in_b, "came-from-a", "ITEM-A");
        assert!(same(&hub.route(&moved).await, &b), "by its path while nothing says otherwise");
        hub.set_moved_out(&Arc::downgrade(&a), HashSet::from(["ITEM-A".to_owned()]));
        assert!(same(&hub.route(&moved).await, &a), "by its item id");
        assert!(same(&hub.route(&opened(&in_b, "theirs", "ITEM-B")).await, &b));
        hub.set_moved_out(&Arc::downgrade(&a), HashSet::new());
        assert!(same(&hub.route(&moved).await, &b), "the row went");
    }

    /// An item id is another account's while that account's outbox waits to
    /// fetch it, its tree store knows it, or the id names its drive; never an account's own.
    #[tokio::test]
    async fn another_accounts_item_ids_are_claimed() {
        let dir = tempfile::tempdir().unwrap();
        let hub = HelperHub::new();
        let a = account_at(&hub, &dir.path().join("A")).await;
        let b = account_at(&hub, &dir.path().join("B")).await;
        let (of_a, of_b) = (Arc::downgrade(&a), Arc::downgrade(&b));
        assert!(!hub.claimed_elsewhere(&of_b, "ITEM-A"), "nothing says so yet");
        hub.set_moved_out(&of_a, HashSet::from(["ITEM-A".to_owned()]));
        assert!(hub.claimed_elsewhere(&of_b, "ITEM-A"), "A's move out waits for it");
        assert!(!hub.claimed_elsewhere(&of_a, "ITEM-A"), "never one's own");

        let row = Row {
            id: "ITEM-S".into(),
            parent_id: Some("ROOT".into()),
            name: "s".into(),
            kind: Kind::File,
            size: 0,
            mtime: 0,
            etag: None,
            ctag: None,
            quickxor: None,
            mime: None,
            placement: Placement::Placed,
        };
        let store = Store::new(TreeStore::in_memory().unwrap());
        store
            .with(|s| {
                s.commit_page(&[Change::Upsert(row)], "next")?;
                s.set_meta("drive_id", Some("abc123"))
            })
            .unwrap();
        *a.store.lock().unwrap() = Some(store);
        assert!(hub.claimed_elsewhere(&of_b, "ITEM-S"), "A's tree knows it");
        assert!(hub.claimed_elsewhere(&of_b, "ABC123!42"), "the id names A's drive");
        assert!(!hub.claimed_elsewhere(&of_b, "DEF456!42"));
        assert!(!hub.claimed_elsewhere(&of_a, "ITEM-S"));
    }

    /// Review M3: one candidate by device is the answer only while every other account's
    /// folder is placed. With another account's folder held back — its device unknown, and
    /// the file could be in it — the one candidate is verified like two, and a file outside
    /// its folder is no one's.
    #[tokio::test]
    async fn one_candidate_is_verified_while_another_folder_cannot_be_placed() {
        let dir = tempfile::tempdir().unwrap();
        let hub = HelperHub::new();
        let a = account_at(&hub, &dir.path().join("A")).await;
        assert!(same(&hub.route(&opened(dir.path(), "moved-out", "1")).await, &a), "unverified while all is placed");

        let config = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::config::ConfigStore::open(&crate::config::Paths::in_dir(config.path()), async { false }).await);
        let id = store.add_account("B").unwrap().id;
        let folder = crate::config::RootConfig {
            path: dir.path().join("B"),
            id: String::new(),
            intercepted: true,
            source: "local".into(),
            baloo_excluded: false,
            upgrade_when_helper: None,
        };
        store.set_root(&id, Some(folder)).unwrap();
        let b = SyncService::on_hub(&hub, None, Some(super::super::Persist { store, account: id }));
        b.hold_back("a test");

        assert!(same(&hub.route(&opened(&dir.path().join("A"), "f", "2")).await, &a), "verified by path");
        assert!(hub.route(&opened(dir.path(), "moved-out-too", "3")).await.is_none(), "not taken on trust");
    }
}
