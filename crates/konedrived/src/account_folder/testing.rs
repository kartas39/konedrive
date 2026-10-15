//! Test support for `account_folder/`: a [`Wiring`] of fakes, so that a test reaches a state of the
//! folder through public calls and what it was made with, never through a field of
//! [`SyncService`].
//!
//! - the account: [`Account`], over a state the test changes, counting what the folder
//!   tells it;
//! - `config.toml`: a temporary one ([`persist`]), or the test's own;
//! - the content sources: [`Sources`], the real ones until the test puts its own in;
//! - the clock: [`ManualClock`], moved by hand.
//!
//! OneDrive itself is wiremock, or the fake OneDrive (`crate::fake_onedrive`), behind the drive a
//! test gives [`Builder::onedrive`].
//!
//! Built for the crate's tests and, with the `fault-injection` feature, for the VM suite.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use konedrive_graph::drive::DriveClient;
use tokio::sync::watch;

use super::registry::Registry;
use super::wiring::{self, OneDrive, Persist, SyncPaths, Wiring};
use super::SyncService;
use crate::account::quota::Quota;
use crate::account::state::{AccountSnapshot, SignInState, StateHandle};
use crate::account::FolderAccount;
use crate::conditions::running::Clock;
use crate::config::ConfigStore;
use crate::desktop::baloo::Baloo;
use crate::content_source::graph_source::GraphSource;
use crate::content_source::source::{ContentSource, Fetched, SourceError};
use crate::listing::listing_poller::Schedule;

impl SyncService {
}

/// The downloads that never ended in a folder served through FUSE: each one's size on disk
/// and its checkpoint (`ItemFilesInterface::list_unfinished_downloads`); none for a folder
/// that is not mounted.
///
/// The VM suite asks for them (`fuse-service staged`): after a file was read whole it
/// checks that the list is empty (`mounted_folder.sh`), and the service group prints it
/// after a download that was cut off (`guest.sh`). The list is of the files the mount's
/// download states keep — a running download's file that is not whole, and the file kept
/// of a failed one —, each with its size on disk and the checkpoint written on it.
pub async fn staged(service: &SyncService) -> Vec<(u64, Option<u64>)> {
    let Some(mount) = service.mount() else { return Vec::new() };
    let unfinished = mount.files_interface().list_unfinished_downloads().await.unwrap_or_default();
    unfinished.into_iter().map(|(_, bytes_on_disk, checkpoint)| (bytes_on_disk, checkpoint)).collect()
}

/// A clock that stands still until the test moves it. Whoever sleeps by it wakes when it
/// has been moved far enough, however the sleep and the move fall in time.
pub(crate) struct ManualClock {
    now: watch::Sender<i64>,
}

impl ManualClock {
    /// A clock that reads `start` (unix seconds).
    #[cfg(test)]
    pub fn at(start: i64) -> Arc<Self> {
        Arc::new(Self { now: watch::Sender::new(start) })
    }

    /// Moves the clock ahead by `seconds`, and wakes whoever waited for less.
    #[cfg(test)]
    pub(crate) fn advance(&self, seconds: i64) {
        self.now.send_modify(|now| *now += seconds);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> i64 {
        *self.now.borrow()
    }

    fn sleep_until(&self, at: i64) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let mut now = self.now.subscribe();
        Box::pin(async move {
            while *now.borrow_and_update() < at {
                if now.changed().await.is_err() {
                    // The clock is gone: nothing moves it any more.
                    std::future::pending::<()>().await;
                }
            }
        })
    }
}

/// An account as its folder sees it: a state the test changes ([`state`](Self::state)),
/// a quota kept in that state, and a count of what the folder told it.
pub(crate) struct Account {
    state: StateHandle,
    rechecks: AtomicUsize,
    drives: Mutex<Vec<String>>,
}

impl Account {
    /// An account whose state is `state`.
    pub(crate) fn over(state: StateHandle) -> Arc<Self> {
        Arc::new(Self { state, rechecks: AtomicUsize::new(0), drives: Mutex::default() })
    }

    /// A signed-in, read-only account.
    pub(crate) fn signed_in() -> Arc<Self> {
        Self::over(StateHandle::new(AccountSnapshot { state: SignInState::SignedIn, ..AccountSnapshot::default() }))
    }

    /// The account's state: a test signs it in or out, or changes its mode, here.
    #[cfg(test)]
    pub(crate) fn state(&self) -> &StateHandle {
        &self.state
    }
}

impl FolderAccount for Account {
    fn snapshot(&self) -> AccountSnapshot {
        self.state.get()
    }

    fn tell_unsent_count(&self, _unsent: u64) {}

    fn changes(&self) -> watch::Receiver<AccountSnapshot> {
        self.state.subscribe()
    }

    fn quota(&self) -> Quota {
        Quota::new(self.state.clone(), None)
    }

    fn recheck_mode(&self) {
        self.rechecks.fetch_add(1, Ordering::SeqCst);
    }

    fn drive_seen(&self, drive: &str) {
        self.drives.lock().unwrap().push(drive.to_owned());
    }
}

/// Where a service persists its folder: the one account of the `config.toml` at `file`
/// (added when there is none), in a store opened from the file — as each start opens it.
/// A file that cannot be read makes a store that refuses every write, as the daemon's
/// does.
pub fn persist(file: &Path) -> Persist {
    assert_eq!(file.file_name().and_then(|n| n.to_str()), Some("config.toml"));
    let paths = crate::config::Paths::in_dir(file.parent().unwrap());
    // `open` awaits nothing but the wallet check, which here is ready.
    let opening = std::pin::pin!(ConfigStore::open(&paths, async { false }));
    let std::task::Poll::Ready(store) = opening.poll(&mut std::task::Context::from_waker(std::task::Waker::noop())) else {
        unreachable!("ConfigStore::open waited")
    };
    let account = match store.snapshot().accounts.first() {
        Some(account) => account.id.clone(),
        None => store.add_account("Personal").map(|a| a.id).unwrap_or_else(|_| crate::config::AccountId::new("0123456789ab")),
    };
    Persist { store: Arc::new(store), account }
}

/// The content sources of a test: the real ones, until the test puts its own in their
/// place ([`replace`](Self::replace)) — for every folder made with them, also one whose
/// source exists already.
#[derive(Default)]
pub(crate) struct Sources {
    instead: Arc<Mutex<Option<Arc<dyn ContentSource>>>>,
    broken: AtomicBool,
}

impl Sources {
}

impl wiring::Sources for Sources {
    fn onedrive(&self, drive: &DriveClient) -> Arc<dyn ContentSource> {
        assert!(!self.broken.load(Ordering::SeqCst), "broken on purpose");
        Arc::new(Replaceable { real: Arc::new(GraphSource::new(drive.clone())), instead: Arc::clone(&self.instead) })
    }
}

/// A folder's real source, or what the test put in its place.
struct Replaceable {
    real: Arc<dyn ContentSource>,
    instead: Arc<Mutex<Option<Arc<dyn ContentSource>>>>,
}

impl Replaceable {
    fn now(&self) -> Arc<dyn ContentSource> {
        self.instead.lock().unwrap().clone().unwrap_or_else(|| Arc::clone(&self.real))
    }
}

#[async_trait]
impl ContentSource for Replaceable {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        self.now().fetch(item_id, from, end).await
    }

    async fn read_range(&self, item_id: &str, from: u64, end: u64) -> Result<Fetched, SourceError> {
        self.now().read_range(item_id, from, end).await
    }

    fn forget(&self, item_id: &str) {
        self.now().forget(item_id);
    }

    fn progress(&self, done: u64, size: u64) {
        self.now().progress(done, size);
    }
}

/// What a test's service was made with, for the test to act on: found again by
/// [`parts`].
#[derive(Clone)]
pub(crate) struct Parts {
    /// Read by this crate's tests alone.
    #[cfg(test)]
    pub(crate) account: Arc<Account>,
    #[cfg(test)]
    pub(crate) persist: Persist,
    /// The directory of a `config.toml` made for the service, kept as long as its parts.
    _config: Option<Arc<tempfile::TempDir>>,
}

/// Every service made here that still lives, with its parts.
static MADE: Mutex<Vec<(Weak<SyncService>, Parts)>> = Mutex::new(Vec::new());

/// What `service` was made with. It was made by [`Builder::build`].
#[cfg(test)]
pub(crate) fn parts(service: &SyncService) -> Parts {
    let made = MADE.lock().unwrap();
    let found = made.iter().find(|(made, _)| std::ptr::eq(made.as_ptr(), service));
    found.map(|(_, parts)| parts.clone()).expect("the service was made by account_folder::testing")
}

/// A [`Wiring`] of fakes, part by part; what is not said is: a registry of the service's
/// own, a signed-in read-only account, a `config.toml` in a temporary directory
/// of its own, no drive (no folder can be registered), no Baloo, no bus to tell the desktop on, the default schedule, the
/// real source behind its switch, and the system's clock.
#[derive(Default)]
pub struct Builder {
    registry: Option<Arc<Registry>>,
    account: Option<StateHandle>,
    persist: Option<Persist>,
    onedrive: Option<OneDrive>,
    baloo: Option<Baloo>,
    schedule: Option<Schedule>,
    clock: Option<Arc<ManualClock>>,
    desktop_bus: Option<zbus::Connection>,
}

/// Starts a [`Builder`].
pub fn wiring() -> Builder {
    Builder::default()
}

impl Builder {

    /// The account's state is `state`, which the test keeps and changes.
    pub fn account(mut self, state: StateHandle) -> Self {
        self.account = Some(state);
        self
    }

    /// The service records its folder there: a restart is another service on the same.
    pub fn persist(mut self, persist: Persist) -> Self {
        self.persist = Some(persist);
        self
    }

    /// A folder registered while signed in shows `drive`.
    pub fn onedrive(mut self, drive: DriveClient, paths: SyncPaths) -> Self {
        self.onedrive = Some(OneDrive { drive, paths });
        self
    }

    /// The account's pause is kept by `clock`.
    #[cfg(test)]
    pub(crate) fn clock(mut self, clock: &Arc<ManualClock>) -> Self {
        self.clock = Some(Arc::clone(clock));
        self
    }

    /// The desktop's programs are told on `connection`, a private bus of the test's: without
    /// it the service tells the desktop nothing.
    #[cfg(test)]
    pub(crate) fn desktop_bus(mut self, connection: zbus::Connection) -> Self {
        self.desktop_bus = Some(connection);
        self
    }

    /// The service, made with all of it and added to its registry, as the account manager
    /// adds the daemon's. [`parts`] finds what it was made with.
    pub fn build(self) -> Arc<SyncService> {
        let registry = self.registry.unwrap_or_else(Registry::new);
        let account = match self.account {
            Some(state) => Account::over(state),
            None => Account::signed_in(),
        };
        let (persist, config) = match self.persist {
            Some(persist) => (persist, None),
            None => {
                let dir = tempfile::tempdir().expect("a temporary directory for config.toml");
                (persist(&dir.path().join("config.toml")), Some(Arc::new(dir)))
            }
        };
        let sources = Arc::new(Sources::default());
        let mut wiring = Wiring {
            onedrive: self.onedrive,
            sources: Arc::clone(&sources) as Arc<dyn wiring::Sources>,
            ..Wiring::new(Arc::clone(&registry), Arc::clone(&account) as Arc<dyn FolderAccount>, persist.clone())
        };
        if let Some(baloo) = self.baloo {
            wiring.baloo = baloo;
        }
        if let Some(schedule) = self.schedule {
            wiring.schedule = schedule;
        }
        wiring.desktop_bus = self.desktop_bus;
        if let Some(clock) = &self.clock {
            wiring.clock = Arc::clone(clock) as Arc<dyn Clock>;
        }
        let service = SyncService::new(wiring);
        registry.add(&service);
        let parts = Parts {
            #[cfg(test)]
            account,
            #[cfg(test)]
            persist,
            _config: config,
        };
        let mut made = MADE.lock().unwrap();
        made.retain(|(service, _)| service.strong_count() > 0);
        made.push((Arc::downgrade(&service), parts));
        service
    }
}
