use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{OpenOptionsExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::time::Duration;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use nix::sys::socket::{accept, bind, listen as sock_listen, socket, AddressFamily, Backlog, SockFlag, SockType, UnixAddr};
use tokio::sync::mpsc;
use konedrive_fs::placeholder::{read_state, State};

use crate::hydration::source::{ContentSource, Fetched, LocalDir, SourceError, Answered, FillError};
use crate::status::report::Report;
use crate::helper::{HelperLink, HydrateRequest};
use crate::account::state::{SignInState, StateHandle};
use crate::folder::locks::tests::key_of;
use crate::sync::free_up::FreedUp;
use crate::folder::locks::{InodeKey, InodeLocks};
use crate::status::snapshot::{SyncSnapshot, SyncStateHandle};
use crate::hydration::server::{fill_event, serve_hydrations, serve_hydrations_reporting};
use crate::hydration::pin::state_of_path;
use crate::hydration::pin;
use crate::hydration::source;
use crate::status::activity;
use super::*;
pub(crate) use super::testing::{persist, FakeHelper, Seen};

mod guards;
mod hydrate;
mod mode;
mod onedrive;
mod pins;
mod registration;
mod reports;
mod startup;

/// What `config.toml` records of the account's folder, in the words of
/// version 1's file that these tests were first written in. No folder
/// reads as version 1's defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Config {
    pub sync_root: String,
    pub sync_root_id: String,
    pub sync_root_intercepted: bool,
    pub sync_root_source: String,
    pub sync_root_baloo_excluded: bool,
    pub sync_root_upgrade_when_helper: Option<bool>,
}

impl Config {
    pub(super) fn load(file: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
        let config: crate::config::Config = toml::from_str(&text).map_err(|e| e.to_string())?;
        Ok(match config.accounts.first().and_then(|a| a.root.clone()) {
            Some(root) => Config {
                sync_root: root.path.display().to_string(),
                sync_root_id: root.id,
                sync_root_intercepted: root.intercepted,
                sync_root_source: root.source,
                sync_root_baloo_excluded: root.baloo_excluded,
                sync_root_upgrade_when_helper: root.upgrade_when_helper,
            },
            None => Config {
                sync_root: String::new(),
                sync_root_id: String::new(),
                sync_root_intercepted: true,
                sync_root_source: "local".into(),
                sync_root_baloo_excluded: false,
                sync_root_upgrade_when_helper: None,
            },
        })
    }
}

/// Writes a `config.toml` whose one account's folder is `root`, as a
/// daemon that knew less wrote it.
fn write_config(file: &Path, root: &str) {
    let text = format!("config_version = 2\n\n[[accounts]]\nid = \"0123456789ab\"\nlabel = \"Personal\"\n\n[accounts.root]\n{root}");
    std::fs::write(file, text).unwrap();
}

/// A stand-in helper: accepts one connection, greets, acknowledges the
/// handshake `Hello`, then acknowledges everything and reports every
/// `HydrateDone` it sees. The listener is bound on the caller's thread,
/// before this returns, so `connect` cannot race `bind`.
///
/// It reports through a `tokio` channel rather than a `std` one because
/// the tests below wait for it *inside* the runtime: a blocking
/// `recv_timeout` on a current-thread runtime would park the one thread
/// that has to run `serve_hydrations`.
pub(crate) fn fake_helper(path: std::path::PathBuf) -> mpsc::UnboundedReceiver<(u64, i32)> {
    let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
        .unwrap();
    let addr = UnixAddr::new(&path).unwrap();
    bind(fd.as_raw_fd(), &addr).unwrap();
    sock_listen(&fd, Backlog::new(16).unwrap()).unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let listener: OwnedFd = fd;
        let accepted = accept(listener.as_raw_fd()).unwrap();
        // SAFETY: `accept` just returned a freshly opened descriptor that
        // this process now solely owns.
        let stream = unsafe { UnixStream::from_raw_fd(accepted) };
        let mut channel = Channel::new(stream).unwrap();
        channel
            .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
            .unwrap();
        let (hello, _) = channel.recv::<ToHelper>().unwrap();
        assert!(
            matches!(hello, ToHelper::Hello { version } if version == PROTOCOL_VERSION),
            "{hello:?}"
        );
        channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
        while let Ok((message, _fd)) = channel.recv::<ToHelper>() {
            if let ToHelper::HydrateDone { req_id, errno } = message {
                let _ = tx.send((req_id, errno));
            }
            if channel.send(&ToDaemon::Ack { errno: 0 }, None).is_err() {
                break;
            }
        }
    });
    rx
}

pub(crate) fn placeholder(dir: &std::path::Path, name: &str, item_id: &str, size: u64) -> OwnedFd {
    let handle = std::fs::File::open(dir).unwrap();
    konedrive_fs::placeholder::create_placeholder(
        &handle,
        name,
        item_id,
        size,
        std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
    )
    .unwrap();
    std::fs::File::options()
        .read(true)
        .write(true)
        .open(dir.join(name))
        .unwrap()
        .as_fd()
        .try_clone_to_owned()
        .unwrap()
}

// --- SyncService -------------------------------------------------------

async fn service_with_helper() -> (Arc<SyncService>, tempfile::TempDir, FakeHelper) {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = FakeHelper::start(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    (testing::service(Some(link), None, None), sockets, helper)
}

/// The newest events first, as (kind, path, detail), oldest first.
async fn activity_of(service: &SyncService) -> Vec<(String, String, String)> {
    let mut events = service.recent_activity(200).await.unwrap();
    events.reverse();
    events.into_iter().map(|e| (e.kind.as_str().to_owned(), e.path, e.detail)).collect()
}

/// A source whose one stream is the reading end of a pipe the test
/// writes into: how a test holds a download half-way.
struct Piped {
    reader: std::sync::Mutex<Option<tokio::io::DuplexStream>>,
    size: u64,
}

#[async_trait]
impl ContentSource for Piped {
    async fn fetch(&self, _item_id: &str, from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        let stream = self.reader.lock().unwrap().take().ok_or_else(|| SourceError::NotFound("fetched twice".into()))?;
        Ok(Fetched {
            served_from: from,
            size: self.size,
            mtime: std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
            version: None,
            stream: Box::new(stream),
        })
    }
}

/// A source that reports the largest number of fetches that were ever in
/// flight at once, and holds each one open for `delay` so that a second
/// one has time to arrive.
pub(crate) struct CountingSource {
    dir: std::path::PathBuf,
    delay: Duration,
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
    peak: Arc<std::sync::atomic::AtomicUsize>,
}

impl CountingSource {
    pub(crate) fn new(dir: &std::path::Path, delay: Duration) -> (Arc<Self>, Arc<std::sync::atomic::AtomicUsize>) {
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let source = Arc::new(Self {
            dir: dir.to_path_buf(),
            delay,
            in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            peak: Arc::clone(&peak),
        });
        (source, peak)
    }
}

#[async_trait]
impl ContentSource for CountingSource {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        use std::sync::atomic::Ordering::SeqCst;
        let now = self.in_flight.fetch_add(1, SeqCst) + 1;
        self.peak.fetch_max(now, SeqCst);
        tokio::time::sleep(self.delay).await;
        let fetched = LocalDir::new(self.dir.clone()).fetch(item_id, from, end).await;
        self.in_flight.fetch_sub(1, SeqCst);
        fetched
    }
}

/// From now on the folder's files are filled from `source`, not from what it was
/// populated from.
fn install_source(service: &SyncService, source: Arc<dyn ContentSource>) {
    testing::parts(service).sources.replace(Some(source));
}

async fn wait_for_state(service: &SyncService, path: &std::path::Path, want: &str) {
    for _ in 0..400 {
        if service.item_state(path).await == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("{} never reached state {want}", path.display());
}

async fn populated_service(
    bytes: &[u8],
) -> (Arc<SyncService>, tempfile::TempDir, tempfile::TempDir, tempfile::TempDir, FakeHelper) {
    let (service, sockets, helper) = service_with_helper().await;
    let source_dir = tempfile::tempdir().unwrap();
    std::fs::write(source_dir.path().join("f.bin"), bytes).unwrap();
    let root_dir = tempfile::tempdir().unwrap();
    service.register_root(root_dir.path()).await.unwrap();
    service.populate_from_directory(source_dir.path()).await.unwrap();
    (service, root_dir, source_dir, sockets, helper)
}

pub(crate) async fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..600 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{what} never happened");
}

/// A fake helper, a service connected to it that persists into a config
/// file of its own, and everything that has to outlive the test body.
async fn service_with_config() -> (Arc<SyncService>, FakeHelper, PathBuf, tempfile::TempDir, tempfile::TempDir) {
    let sockets = tempfile::tempdir().unwrap();
    let socket_path = sockets.path().join("helper.sock");
    let helper = FakeHelper::start(socket_path.clone());
    let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let config_file = config_dir.path().join("config.toml");
    let service = testing::service(Some(link), None, Some(persist(&config_file)));
    (service, helper, config_file, sockets, config_dir)
}

fn recorded_root(config_file: &Path) -> String {
    Config::load(config_file).unwrap().sync_root
}

fn resolved(path: &Path) -> String {
    std::fs::canonicalize(path).unwrap().display().to_string()
}

fn data_blocks(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().blocks()
}
