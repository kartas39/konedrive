//! What downloads a file of OneDrive whole into the backing directory, by item: what
//! `file_downloader` is today by a row of `onedrive_items` and a path.
//!
//! **State owned:** none; the states are `ItemDownloadStates`'. It holds what a download
//! goes through.
//!
//! **Who takes the pool's slot:** the downloader, and nobody else: a step of the plan that
//! waits for a download holds none (the plan's task only counts its downloads against
//! `StepLimits::download`).
//!
//! **One download:** asked for with the item, its binding and the content to download
//! ([`ItemToDownload`]); joined or started (`ItemDownloadStates::start_or_join`); fetched
//! on the runtime into a file that has no name, through the content source and the
//! transfer pool, as today; checked against the content's size and hash; synced — by the
//! downloader, with no lock held; then landed as ONE job of the write-operation thread
//! (`ItemDownloadLanding::land`), which links it under the name the row has by then,
//! syncs the directory and commits conditionally; then announced and its waiters
//! answered: `ESTALE` when the landing found the row changed.
//!
//! **Tested without the rest**, with a content source the test holds, a temporary backing
//! directory and a store in memory (today's `file_downloader/tests.rs` is the reference):
// test: a_download_lands_at_the_place_the_row_has_when_it_ends
// test: a_download_whose_content_the_row_no_longer_has_lands_nothing_and_answers_stale
// test: a_download_asked_twice_runs_once_and_answers_both
// test: a_download_that_fails_keeps_its_unfinished_file_and_goes_on_from_it
// test: the_file_is_synced_before_the_landing_job_is_queued
// test: a_download_for_the_plan_commits_as_the_plans_step
// test: a_sign_out_a_throttle_and_a_missing_item_of_the_source_are_answered_as_such
// test: the_plans_download_is_cancelled_when_nobody_else_waits_for_it
// test: a_stop_aborts_every_download
#![allow(dead_code, unused_variables)]

use std::path::PathBuf;
use std::sync::Arc;

use konedrive_graph::pool::{Class, TransferPool};
use konedrive_tree::{ContentVersion, ItemVersions, LocalItemId, OneDriveItemReference};

use super::backing_directory::BackingDirectory;
use super::download_states::Waiter;
use super::item_download_landing::{ItemDownloadLanding, ItemLandingOutcome};
use super::item_download_states::ItemDownloadStates;
use super::item_inotify_bridge::ItemInotifyBridge;
use super::write_operation_thread::WriteOperationThread;
use crate::hydration::source::{ContentSource, Share};
use crate::status::report::Report;

/// A file of OneDrive that is to be downloaded whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ItemToDownload {
    pub(super) item: LocalItemId,
    /// What the content source is asked by.
    pub(super) onedrive_item: OneDriveItemReference,
    /// The content to download: the item's local content, which is OneDrive's while the
    /// file is not on disk.
    pub(super) content: ContentVersion,
    /// The versions the plan's step was planned from, for a download the plan asked for:
    /// its landing commits as the step (`commit_download_content`). `None` for a read, an
    /// open for writing and `Files.Hydrate`, whose landing asks the content alone.
    pub(super) planned_from: Option<ItemVersions>,
}

/// Why the content source did not give a file, by what the plan's task does about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadRefusal {
    /// The account is signed out (401): all downloads wait for the sign-in.
    SignedOut,
    /// 429 or 503, with the time the source named.
    Throttled { retry_after: Option<std::time::Duration> },
    /// OneDrive does not have the item, or not this content of it, any more.
    ItemGone,
    /// The network, or the download was aborted: tried again later.
    Transient { detail: String },
}

/// The downloader of one mount.
pub(super) struct ItemDownloader {
    backing_directory: Arc<BackingDirectory>,
    download_states: Arc<ItemDownloadStates>,
    download_landing: Arc<ItemDownloadLanding>,
    inotify_bridge: Arc<ItemInotifyBridge>,
    write_operation_thread: Arc<WriteOperationThread>,
    source: Arc<dyn ContentSource>,
    pool: Arc<TransferPool>,
    split_download_share: Arc<Share>,
    report: Report,
    /// The folder's path, which is the mount: a download is shown under its file's path.
    folder: PathBuf,
    runtime: tokio::runtime::Handle,
}

impl ItemDownloader {
    /// Made by the mount's build, after the landing and the bridge.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        backing_directory: Arc<BackingDirectory>,
        download_states: Arc<ItemDownloadStates>,
        download_landing: Arc<ItemDownloadLanding>,
        inotify_bridge: Arc<ItemInotifyBridge>,
        write_operation_thread: Arc<WriteOperationThread>,
        source: Arc<dyn ContentSource>,
        pool: Arc<TransferPool>,
        split_download_share: Arc<Share>,
        report: Report,
        folder: PathBuf,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        todo!()
    }

    /// Downloads `file` unless a download of the same item and content runs already;
    /// `waiter` is answered when the download ends, however it ends. `class` is the pool's:
    /// `Open` for a read, an open for writing and `Files.Hydrate`, `Download` for the
    /// plan's pin. Spawns on the runtime and returns at once.
    ///
    /// Called by read at the threshold of bytes read as ranges, by the open for writing
    /// and the truncation of a file that is not on disk, by
    /// `ItemFilesInterface::download`, and by `FolderSteps::run_download_content`.
    pub(super) fn download(self: &Arc<Self>, file: ItemToDownload, class: Class, waiter: Option<Waiter>) {
        todo!()
    }

    /// As [`download`](Self::download), awaited: what the landing answered, or the
    /// source's refusal typed, for the plan's step
    /// (`FolderSteps::run_download_content`, which turns it into
    /// `StepRefusal::{SignedOut, Throttled, ItemGone, Transient}`).
    pub(super) async fn download_and_land(self: &Arc<Self>, file: ItemToDownload, class: Class) -> Result<ItemLandingOutcome, DownloadRefusal> {
        todo!()
    }

    /// The plan gives the download of `item`'s `content` up — a pause, a sign-out, a stop
    /// of the sync: aborted when nobody else waits for it
    /// (`ItemDownloadStates::abort_if_unawaited`). Called when the task that awaits
    /// `download_and_land` is aborted (`RunningSteps::abort_requests`): by a guard that
    /// task holds.
    pub(super) fn cancel(&self, item: LocalItemId, content: &ContentVersion) {
        todo!()
    }

    /// Every download is aborted (`ItemDownloadStates::abort_all`): the mount stops. After
    /// it no spawned download holds a clone of the store.
    pub(super) fn stop(&self) {
        todo!()
    }
}
