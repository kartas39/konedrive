//! The state of each file's download, by the item and the content being downloaded: what
//! `download_states` is today by OneDrive id.
//!
//! **State owned:** per (item, content version): a running download with who waits for
//! it, how far it was written and its file; or a failed one with its unfinished file kept
//! to go on from; the handle each running download's task is aborted by; and how many
//! bytes of each item were read as ranges. Changed by the
//! downloader and by read alone, through the functions here; no I/O.
//!
//! **Answers for:** the questions of the review's M4: who dedups (a download of the same
//! item and content that runs is joined, whoever asks: a read, an open for writing,
//! `Files.Hydrate`, the plan's pin); who answers "is downloading" (the state attribute,
//! free-up); from which file a read is answered while its download runs. A download of
//! another content of the same item is another download: the older one's landing finds
//! the row changed and drops its file.
//!
//! **Tested without the rest** (today's `download_states/tests.rs` is the reference):
// test: a_second_asking_for_the_same_item_and_content_joins_the_running_download
// test: an_asking_for_another_content_of_the_item_starts_another_download
// test: every_waiter_is_answered_when_the_download_ends_however_it_ends
// test: a_failed_download_keeps_its_unfinished_file_for_the_next_asking_of_that_content
// test: the_bytes_read_as_ranges_reach_the_threshold_once_and_are_forgotten_with_the_item
// test: the_waiters_are_failed_when_the_mount_stops
// test: a_download_that_is_cancelled_is_aborted_and_its_waiters_answered
// test: a_download_with_another_waiter_is_not_cancelled_for_the_plan
#![allow(dead_code, unused_variables)]

use std::collections::HashMap;
use std::fs::File;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use konedrive_tree::{ContentVersion, LocalItemId};

use super::download_states::{RunningDownloadFile, Waiter};

/// What a download is of.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct DownloadKey {
    pub(super) item: LocalItemId,
    pub(super) content: ContentVersion,
}

/// What became of an asking for a download.
pub(super) enum ItemDownloadStart {
    /// A download of this item and content runs: the waiter was added to it.
    Joined,
    /// This asking starts it; the unfinished file of an earlier try, when one was kept.
    Started { unfinished_file: Option<Arc<File>> },
}

enum State {
    Running { waiters: Vec<Waiter>, written_up_to: Arc<AtomicU64>, file: Option<Arc<File>>, abort: Option<tokio::task::AbortHandle> },
    Failed { unfinished_file: Arc<File>, failure_number: u64 },
}

/// The download states of one mount.
pub(super) struct ItemDownloadStates {
    states: std::sync::Mutex<HashMap<DownloadKey, State>>,
    bytes_read_as_ranges: std::sync::Mutex<HashMap<LocalItemId, u64>>,
    /// `[transfers] download_after`.
    download_after: u64,
}

impl ItemDownloadStates {
    /// No download runs. Made by the mount's build.
    pub(super) fn new(download_after: u64) -> Self {
        todo!()
    }

    /// A download of `key` is asked for: joined when one runs, started otherwise. Called
    /// by `ItemDownloader::download` alone.
    pub(super) fn start_or_join(&self, key: &DownloadKey, waiter: Option<Waiter>) -> ItemDownloadStart {
        todo!()
    }

    /// The task of the running download of `key` is aborted by `abort`. Called by the
    /// downloader right after it spawned the task.
    pub(super) fn keep_abort_handle(&self, key: &DownloadKey, abort: tokio::task::AbortHandle) {
        todo!()
    }

    /// Aborts the running download of `key` when nobody but the plan waits for it
    /// (`waiters_but_plan` are those a read, an open or `Files.Hydrate` added): its
    /// unfinished file is kept as after a failure. Whether it was aborted.
    pub(super) fn abort_if_unawaited(&self, key: &DownloadKey) -> bool {
        todo!()
    }

    /// Aborts every running download and answers its waiters `EIO`: the mount stops.
    pub(super) fn abort_all(&self) {
        todo!()
    }

    /// The running download of `key` writes into `file` from now on; how far it was
    /// written, raised by the downloader as bytes come.
    pub(super) fn begin_writing(&self, key: &DownloadKey, file: Arc<File>) -> Arc<AtomicU64> {
        todo!()
    }

    /// The file and the watermark of the running download of `key`: a read of a range it
    /// has written already is answered from it. Called by read.
    pub(super) fn find_running_download(&self, key: &DownloadKey) -> Option<RunningDownloadFile> {
        todo!()
    }

    /// The download of `key` ended: its waiters are answered `outcome` (the errno of a
    /// failure); a failure keeps the unfinished file.
    pub(super) fn finish(&self, key: &DownloadKey, outcome: Result<(), i32>) {
        todo!()
    }

    /// The download of `key` was dropped without an end (a panic, an abort): its waiters
    /// get `EIO`.
    pub(super) fn abandon(&self, key: &DownloadKey) {
        todo!()
    }

    /// Whether a download of `item` runs, of any content: the state attribute, free-up.
    pub(super) fn is_downloading(&self, item: LocalItemId) -> bool {
        todo!()
    }

    /// `bytes` more of `item` were read as ranges; whether that reaches the threshold at
    /// which the file is downloaded whole (`download_states::reaches_download_threshold`).
    pub(super) fn count_bytes_read(&self, item: LocalItemId, bytes: u64, file_size: u64) -> bool {
        todo!()
    }

    /// Nothing is kept of `item` any more: its row is gone, or it shows another content.
    /// Called by the steps towards the folder that remove an item or take OneDrive's
    /// content (what "the bytes-read count never forgetting a file" lacked: design §14).
    pub(super) fn forget_item(&self, item: LocalItemId) {
        todo!()
    }

    /// Every waiter gets `EIO`: the mount stops.
    pub(super) fn fail_waiters(&self) {
        todo!()
    }
}
