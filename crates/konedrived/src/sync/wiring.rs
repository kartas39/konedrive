//! What an account's folder is made with ([`Wiring`]): everything [`SyncService`] takes from
//! the rest of the daemon, given once, to its constructor, and never changed after it. The
//! daemon's is built by `daemon::manager`; a test's by `sync::testing`.
//!
//! [`SyncService`]: super::SyncService

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use konedrive_graph::drive::DriveClient;

use super::registry::Registry;
use crate::account::FolderAccount;
use crate::conditions::running::{Clock, SystemClock};
use crate::config::ConfigStore;
use crate::desktop::baloo::Baloo;
use crate::hydration::graph_source::GraphSource;
use crate::hydration::source::{ContentSource, LocalDir};
use crate::local::watcher::{Sink, WatchConfig, Watcher};
use crate::remote::listing::Schedule;

/// Where a OneDrive folder's own files live.
#[derive(Debug, Clone)]
pub struct SyncPaths {
    pub tree_db: PathBuf,
    pub rescue_dir: PathBuf,
    /// The freedesktop thumbnail cache. `None` runs no thumbnail
    /// filler at all: the VM suite's real-account run, which must not fetch
    /// a thumbnail of every image in the drive.
    pub thumbnails: Option<PathBuf>,
}

/// Where a folder is recorded so that it survives a restart: its account's
/// `[accounts.root]` in `config.toml`, written only through the daemon's one
/// [`ConfigStore`]. The account's own settings are read and written there too.
#[derive(Clone)]
pub struct Persist {
    pub store: Arc<ConfigStore>,
    /// The account's id.
    pub account: crate::config::AccountId,
}

/// The drive a folder registered while signed in shows, and where the daemon keeps what it
/// knows of it.
#[derive(Clone)]
pub struct OneDrive {
    pub drive: DriveClient,
    pub paths: SyncPaths,
}

/// The limits of the account's transfer pool (`[transfers]` in `config.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfers {
    /// The emergency ceiling (`max`).
    pub ceiling: usize,
    /// How many large files move at once (`large`).
    pub large: usize,
}

impl Default for Transfers {
    fn default() -> Self {
        Self { ceiling: konedrive_graph::pool::DEFAULT_CEILING, large: konedrive_graph::pool::DEFAULT_LARGE }
    }
}

/// Where a folder's files are filled from.
pub trait Sources: Send + Sync {
    /// The source of a folder that shows `drive`.
    fn onedrive(&self, drive: &DriveClient) -> Arc<dyn ContentSource>;
    /// The source of a local folder, populated from the directory `local` reads.
    fn directory(&self, local: LocalDir) -> Arc<dyn ContentSource>;
}

/// The daemon's sources: Graph for a OneDrive folder, the directory itself for a local one.
pub struct RealSources;

impl Sources for RealSources {
    fn onedrive(&self, drive: &DriveClient) -> Arc<dyn ContentSource> {
        Arc::new(GraphSource::new(drive.clone()))
    }

    fn directory(&self, local: LocalDir) -> Arc<dyn ContentSource> {
        Arc::new(local)
    }
}

/// Starts the watcher of a read-write folder: [`Watcher::start`] in the daemon.
pub type Watchers = Arc<dyn Fn(WatchConfig, Box<dyn Sink>) -> io::Result<Watcher> + Send + Sync>;

/// Everything one account's folder is made with. Nothing in it is replaced while the
/// service lives; what does change with the account (its standing, its mode) is told to
/// the service by a call.
pub struct Wiring {
    /// The folders of the daemon's accounts, and the link to the helper they share.
    pub registry: Arc<Registry>,
    /// The folder's account: its sign-in, its quota, and what the folder tells it.
    pub account: Arc<dyn FolderAccount>,
    /// The account's entry in `config.toml`.
    pub persist: Persist,
    /// `None`: every folder is a local one, filled with `PopulateFromDirectory`.
    pub onedrive: Option<OneDrive>,
    /// Keeps KDE's Baloo indexer out of a fresh OneDrive folder, and lets a forgotten one
    /// back in (`desktop::baloo`). Only `main` gives the real `balooctl6`.
    pub baloo: Baloo,
    /// How often a OneDrive folder is synced.
    pub schedule: Schedule,
    pub transfers: Transfers,
    pub sources: Arc<dyn Sources>,
    pub watchers: Watchers,
    /// The clock the account's pause is kept by.
    pub clock: Arc<dyn Clock>,
}

impl Wiring {
    /// The wiring of the daemon for what has only one real answer: Graph and the
    /// directory as sources, the real watcher, the system's clock, the default schedule
    /// and transfer limits, and no Baloo. The caller sets what its account has.
    pub fn new(registry: Arc<Registry>, account: Arc<dyn FolderAccount>, persist: Persist) -> Self {
        Self {
            registry,
            account,
            persist,
            onedrive: None,
            baloo: Baloo::disabled(),
            schedule: Schedule::default(),
            transfers: Transfers::default(),
            sources: Arc::new(RealSources),
            watchers: Arc::new(Watcher::start),
            clock: Arc::new(SystemClock),
        }
    }
}
