//! What a mount is made with: what the folder's sync hands it.

use std::path::PathBuf;
use std::sync::Arc;

use konedrive_graph::pool::TransferPool;
use konedrive_tree::Store;

use crate::folder::root::SyncRoot;
use crate::hydration::source::{ContentSource, Share};
use crate::status::report::Report;

/// What a folder that takes changes shares with its sync.
pub struct WriteSetup {
    /// The folder's tree lock: a change is recorded under it, as a cycle of the delta listing
    /// holds it from staging to its swap.
    pub tree_lock: Arc<tokio::sync::Mutex<()>>,
    /// The account's ignore list: what is never uploaded.
    pub ignore: crate::local::SharedIgnore,
    /// For conflict copies: `name-<machine>.ext`.
    pub machine_name: String,
}

/// What a mount is made with.
pub struct MountSetup {
    pub root: SyncRoot,
    /// The folder's tree store: its file, for the mount's own reading connection, and its one
    /// writer.
    pub store_path: PathBuf,
    pub store: Store,
    pub source: Arc<dyn ContentSource>,
    pub report: Report,
    pub pool: Arc<TransferPool>,
    /// What the daemon's large downloads in parallel streams share.
    pub split_download_share: Arc<Share>,
    /// `[transfers] download_after`: once that many bytes of a file were read as ranges, the
    /// file is downloaded whole.
    pub download_after: u64,
    /// What the folder shares with its sync when it takes changes.
    pub write_setup: WriteSetup,
}
