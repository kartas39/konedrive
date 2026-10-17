//! What an account's folder is made with ([`Wiring`]): everything [`SyncService`] takes from
//! the rest of the daemon, given once, to its constructor, and never changed after it. The
//! daemon's is built by `daemon::manager`; a test's by `account_folder::testing`.
//!
//! [`SyncService`]: super::SyncService

use std::path::PathBuf;
use std::sync::Arc;

use konedrive_graph::drive::DriveClient;

use super::registry::Registry;
use crate::account::FolderAccount;
use crate::conditions::running::{Clock, SystemClock};
use crate::config::ConfigStore;
use crate::desktop::baloo::Baloo;
use crate::content_source::graph_source::GraphSource;
use crate::content_source::source::ContentSource;
use crate::listing::listing_poller::Schedule;

/// Where a OneDrive folder's own files live.
#[derive(Debug, Clone)]
pub struct SyncPaths {
    pub tree_db: PathBuf,
    /// The freedesktop thumbnail cache. `None` runs no thumbnail
    /// filler at all.
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
    pub(crate) drive: DriveClient,
    pub(crate) paths: SyncPaths,
}

/// The limits of the account's transfer pool (`[transfers]` in `config.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfers {
    /// The emergency ceiling (`max`).
    pub(crate) ceiling: usize,
    /// How many large files move at once (`large`).
    pub(crate) large: usize,
    /// How much of a file that is not downloaded is read before it is downloaded
    /// whole (`download_after`).
    pub(crate) download_after: u64,
}

impl Default for Transfers {
    fn default() -> Self {
        Self { ceiling: konedrive_graph::pool::DEFAULT_CEILING, large: konedrive_graph::pool::DEFAULT_LARGE, download_after: crate::config::DOWNLOAD_AFTER }
    }
}

/// Where a folder's files are filled from.
pub(crate) trait Sources: Send + Sync {
    /// The source of a folder that shows `drive`.
    fn onedrive(&self, drive: &DriveClient) -> Arc<dyn ContentSource>;
}

/// The daemon's source: Graph.
pub(crate) struct RealSources;

impl Sources for RealSources {
    fn onedrive(&self, drive: &DriveClient) -> Arc<dyn ContentSource> {
        Arc::new(GraphSource::new(drive.clone()))
    }
}

/// Everything one account's folder is made with. Nothing in it is replaced while the
/// service lives; what does change with the account (its standing, its mode) is told to
/// the service by a call.
pub struct Wiring {
    /// The folders of the daemon's accounts.
    pub(crate) registry: Arc<Registry>,
    /// The folder's account: its sign-in, its quota, and what the folder tells it.
    pub(crate) account: Arc<dyn FolderAccount>,
    /// The account's entry in `config.toml`.
    pub(crate) persist: Persist,
    /// `None`: the account has no drive, and no folder can be registered for it.
    pub(crate) onedrive: Option<OneDrive>,
    /// Keeps KDE's Baloo indexer out of a fresh OneDrive folder, and lets a forgotten one
    /// back in (`desktop::baloo`). Only `main` gives the real `balooctl6`.
    pub(crate) baloo: Baloo,
    /// How often a OneDrive folder is synced.
    pub(crate) schedule: Schedule,
    pub(crate) transfers: Transfers,
    pub(crate) sources: Arc<dyn Sources>,
    /// The clock the account's pause is kept by.
    pub(crate) clock: Arc<dyn Clock>,
    /// The bus the desktop's programs are told on to read the folder's directories again
    /// (`desktop::dirnotify`): the daemon's own connection to the session bus. `None`: the
    /// service tells the desktop nothing, and reaches no bus by itself.
    pub(crate) desktop_bus: Option<zbus::Connection>,
}

impl Wiring {
    /// The wiring of the daemon for what has only one real answer: Graph as the source,
    /// the system's clock, the default schedule
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
            clock: Arc::new(SystemClock),
            desktop_bus: None,
        }
    }
}
