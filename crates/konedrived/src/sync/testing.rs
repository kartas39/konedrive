//! Test support for `sync/`: a [`Wiring`] of fakes, so that a test reaches a state of the
//! folder through public calls and what it was made with, never through a field of
//! [`SyncService`].
//!
//! - the helper: [`FakeHelper`], which records what it is asked, refuses on demand, and
//!   holds an answer until the test lets it go (a change held open is "the helper has not
//!   answered yet");
//! - the account: [`Account`], over a state the test changes, counting what the folder
//!   tells it;
//! - `config.toml`: a temporary one ([`persist`]), or the test's own;
//! - the content sources: [`Sources`], the real ones until the test puts its own in;
//! - the watcher: [`Watchers`], the real one until the test makes it fail;
//! - the clock: [`ManualClock`], moved by hand.
//!
//! OneDrive itself is wiremock, or the fake OneDrive (`crate::fake_onedrive`), behind the drive a
//! test gives [`Builder::onedrive`].
//!
//! Built for the crate's tests and, with the `fault-injection` feature, for the VM suite.

mod helper;

use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use konedrive_graph::drive::DriveClient;
use tokio::sync::watch;

pub use helper::{FakeHelper, Seen};

use super::hub::HelperHub;
use super::wiring::{self, OneDrive, Persist, SyncPaths, Wiring};
use super::SyncService;
use crate::account::quota::Quota;
use crate::account::state::{AccountSnapshot, SignInState, StateHandle};
use crate::account::FolderAccount;
use crate::conditions::running::Clock;
use crate::config::ConfigStore;
use crate::desktop::baloo::Baloo;
use crate::helper::HelperLink;
use crate::hydration::graph_source::GraphSource;
use crate::hydration::source::{ContentSource, Fetched, LocalDir, SourceError};
use crate::local::watcher::{Sink, WatchConfig, Watcher};
use crate::remote::listing::Schedule;

/// A clock that stands still until the test moves it. Whoever sleeps by it wakes when it
/// has been moved far enough, however the sleep and the move fall in time.
pub struct ManualClock {
    now: watch::Sender<i64>,
}

impl ManualClock {
    /// A clock that reads `start` (unix seconds).
    pub fn at(start: i64) -> Arc<Self> {
        Arc::new(Self { now: watch::Sender::new(start) })
    }

    /// Moves the clock ahead by `seconds`, and wakes whoever waited for less.
    pub fn advance(&self, seconds: i64) {
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
pub struct Account {
    state: StateHandle,
    rechecks: AtomicUsize,
    drives: Mutex<Vec<String>>,
}

impl Account {
    /// An account whose state is `state`.
    pub fn over(state: StateHandle) -> Arc<Self> {
        Arc::new(Self { state, rechecks: AtomicUsize::new(0), drives: Mutex::default() })
    }

    /// A signed-in, read-only account.
    pub fn signed_in() -> Arc<Self> {
        Self::over(StateHandle::new(AccountSnapshot { state: SignInState::SignedIn, ..AccountSnapshot::default() }))
    }

    /// The account's state: a test signs it in or out, or changes its mode, here.
    pub fn state(&self) -> &StateHandle {
        &self.state
    }

    /// How many times the folder asked for the mode to be worked out again.
    pub fn mode_rechecks(&self) -> usize {
        self.rechecks.load(Ordering::SeqCst)
    }

    /// Every drive a cycle said the account's token reaches though it is not the folder's.
    pub fn drives_seen(&self) -> Vec<String> {
        self.drives.lock().unwrap().clone()
    }
}

impl FolderAccount for Account {
    fn snapshot(&self) -> AccountSnapshot {
        self.state.get()
    }

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
        None => store.add_account("Personal").map(|a| a.id).unwrap_or_else(|_| "0123456789ab".into()),
    };
    Persist { store: Arc::new(store), account }
}

/// The content sources of a test: the real ones, until the test puts its own in their
/// place ([`replace`](Self::replace)) — for every folder made with them, also one whose
/// source exists already.
#[derive(Default)]
pub struct Sources {
    instead: Arc<Mutex<Option<Arc<dyn ContentSource>>>>,
}

impl Sources {
    /// From now on every fetch goes to `source`; `None` gives the real source back.
    pub fn replace(&self, source: Option<Arc<dyn ContentSource>>) {
        *self.instead.lock().unwrap() = source;
    }
}

impl wiring::Sources for Sources {
    fn onedrive(&self, drive: &DriveClient) -> Arc<dyn ContentSource> {
        Arc::new(Replaceable { real: Arc::new(GraphSource::new(drive.clone())), instead: Arc::clone(&self.instead) })
    }

    fn directory(&self, local: LocalDir) -> Arc<dyn ContentSource> {
        Arc::new(Replaceable { real: Arc::new(local), instead: Arc::clone(&self.instead) })
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

    fn progress(&self, done: u64, size: u64) {
        self.now().progress(done, size);
    }
}

/// The watcher of a test: the real one, until the test makes it fail to start
/// ([`fail`](Self::fail)).
#[derive(Default)]
pub struct Watchers {
    failing: AtomicBool,
}

impl Watchers {
    /// Whether a watcher fails to start from now on.
    pub fn fail(&self, failing: bool) {
        self.failing.store(failing, Ordering::SeqCst);
    }

    fn start(&self, config: WatchConfig, sink: Box<dyn Sink>) -> io::Result<Watcher> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(io::Error::other("failed on purpose"));
        }
        Watcher::start(config, sink)
    }
}

/// What a test's service was made with, for the test to act on: found again by
/// [`parts`].
#[derive(Clone)]
pub struct Parts {
    pub account: Arc<Account>,
    pub persist: Persist,
    pub sources: Arc<Sources>,
    pub watchers: Arc<Watchers>,
    /// `None`: the service runs on the system's clock.
    pub clock: Option<Arc<ManualClock>>,
    /// The directory of a `config.toml` made for the service, kept as long as its parts.
    _config: Option<Arc<tempfile::TempDir>>,
}

/// Every service made here that still lives, with its parts.
static MADE: Mutex<Vec<(Weak<SyncService>, Parts)>> = Mutex::new(Vec::new());

/// What `service` was made with. It was made by [`Builder::build`].
pub fn parts(service: &SyncService) -> Parts {
    let made = MADE.lock().unwrap();
    let found = made.iter().find(|(made, _)| std::ptr::eq(made.as_ptr(), service));
    found.map(|(_, parts)| parts.clone()).expect("the service was made by sync::testing")
}

/// A [`Wiring`] of fakes, part by part; what is not said is: a hub of the service's own
/// with no link, a signed-in read-only account, a `config.toml` in a temporary directory
/// of its own, no drive (every folder is local), no Baloo, the default schedule, the real
/// sources and watcher behind their switches, and the system's clock.
#[derive(Default)]
pub struct Builder {
    hub: Option<Arc<HelperHub>>,
    link: Option<HelperLink>,
    account: Option<StateHandle>,
    persist: Option<Persist>,
    onedrive: Option<OneDrive>,
    baloo: Option<Baloo>,
    schedule: Option<Schedule>,
    clock: Option<Arc<ManualClock>>,
}

/// Starts a [`Builder`].
pub fn wiring() -> Builder {
    Builder::default()
}

impl Builder {
    /// The service joins `hub`, after the accounts it has.
    pub fn hub(mut self, hub: &Arc<HelperHub>) -> Self {
        self.hub = Some(Arc::clone(hub));
        self
    }

    /// The service gets a hub of its own that holds `link` already.
    pub fn link(mut self, link: Option<HelperLink>) -> Self {
        self.link = link;
        self
    }

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

    pub fn baloo(mut self, baloo: Baloo) -> Self {
        self.baloo = Some(baloo);
        self
    }

    pub fn schedule(mut self, schedule: Schedule) -> Self {
        self.schedule = Some(schedule);
        self
    }

    /// The account's pause is kept by `clock`.
    pub fn clock(mut self, clock: &Arc<ManualClock>) -> Self {
        self.clock = Some(Arc::clone(clock));
        self
    }

    /// The service, made with all of it. [`parts`] finds what it was made with.
    pub fn build(self) -> Arc<SyncService> {
        let hub = self.hub.unwrap_or_else(|| HelperHub::with_link(self.link));
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
        let (sources, watchers) = (Arc::new(Sources::default()), Arc::new(Watchers::default()));
        let starting = Arc::clone(&watchers);
        let mut wiring = Wiring {
            onedrive: self.onedrive,
            sources: Arc::clone(&sources) as Arc<dyn wiring::Sources>,
            watchers: Arc::new(move |config, sink| starting.start(config, sink)),
            ..Wiring::new(hub, Arc::clone(&account) as Arc<dyn FolderAccount>, persist.clone())
        };
        if let Some(baloo) = self.baloo {
            wiring.baloo = baloo;
        }
        if let Some(schedule) = self.schedule {
            wiring.schedule = schedule;
        }
        if let Some(clock) = &self.clock {
            wiring.clock = Arc::clone(clock) as Arc<dyn Clock>;
        }
        let service = SyncService::new(wiring);
        let parts = Parts { account, persist, sources, watchers, clock: self.clock, _config: config };
        let mut made = MADE.lock().unwrap();
        made.retain(|(service, _)| service.strong_count() > 0);
        made.push((Arc::downgrade(&service), parts));
        service
    }
}

/// A service with a hub of its own holding `link`, over `account`'s state (or a signed-in
/// account) and recording its folder in `persist` (or in a `config.toml` of its own).
pub fn service(link: Option<HelperLink>, account: Option<StateHandle>, persist: Option<Persist>) -> Arc<SyncService> {
    let mut builder = wiring().link(link);
    if let Some(state) = account {
        builder = builder.account(state);
    }
    if let Some(persist) = persist {
        builder = builder.persist(persist);
    }
    builder.build()
}
