//! What the daemon's bus interface `Files` asks of a mounted folder, by path, over the
//! rows: what `files_interface` is today over the directory contents.
//!
//! **State owned:** none; it holds the parts it asks. A path is turned into an item by
//! ONE reader (`ItemReader::find_item_by_path`), here and nowhere else in `fuse`.
//!
//! **Answers for:** `org.konedrive.Files` (the map's 6.3) and `Folder.FreeUpSpace`,
//! `PinnedCount`: download now, free up, pin and unpin, the status of a path.
//!
//! **Tested without the rest**, over a store in memory and a temporary backing directory:
// test: a_path_outside_the_folder_and_a_path_the_folder_does_not_show_are_refused
// test: an_item_with_no_binding_is_not_in_onedrive
// test: a_download_asked_by_path_is_the_items_download_and_is_awaited
// test: a_directory_is_freed_file_by_file_each_by_its_own_decision
// test: a_pin_is_set_by_the_stores_job_and_the_plan_woken
// test: the_status_of_a_path_is_the_rows_sync_state_its_pin_and_what_runs_for_it
#![allow(dead_code, unused_variables)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_tree::{FileSyncState, ItemKind, LocalItemId, OneDriveItemReference, Store};

use super::files_interface::{DownloadedSpace, FilesRefusal, FreedSpace};
use super::item_download_states::ItemDownloadStates;
use super::item_downloader::ItemDownloader;
use super::item_free_up::ItemFreeUp;
use super::item_handles::ItemHandles;
use super::write_operation_thread::WriteOperationThread;
use crate::folder::sync_plan_wake::SyncPlanWake;

/// What the bus shows of one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemPathStatus {
    pub item: LocalItemId,
    pub kind: ItemKind,
    /// The binding: what `WebUrl` asks OneDrive by.
    pub onedrive_item: OneDriveItemReference,
    /// `None` for a directory.
    pub sync_state: Option<FileSyncState>,
    pub is_pinned: bool,
    pub is_downloading: bool,
    pub is_open_for_writing: bool,
}

/// The `Files` interface of one mount.
pub struct ItemFilesInterface {
    /// The folder's path, which is the mount.
    folder: PathBuf,
    store: Store,
    download_states: Arc<ItemDownloadStates>,
    downloader: Arc<ItemDownloader>,
    free_up: Arc<ItemFreeUp>,
    item_handles: Arc<ItemHandles>,
    write_operation_thread: Arc<WriteOperationThread>,
    sync_plan_wake: Arc<SyncPlanWake>,
}

impl ItemFilesInterface {
    /// Made by the mount's build, last of the parts.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        folder: PathBuf,
        store: Store,
        download_states: Arc<ItemDownloadStates>,
        downloader: Arc<ItemDownloader>,
        free_up: Arc<ItemFreeUp>,
        item_handles: Arc<ItemHandles>,
        write_operation_thread: Arc<WriteOperationThread>,
        sync_plan_wake: Arc<SyncPlanWake>,
    ) -> Self {
        Self { folder, store, download_states, downloader, free_up, item_handles, write_operation_thread, sync_plan_wake }
    }

    /// The status of `path`, a full path: the item by `ItemReader::find_item_by_path` on
    /// the store's reading connection, its pin (`is_pinned`), and what runs for it.
    /// `OutsideFolder` for a path that is not below the folder, `NotInOneDrive` for a name
    /// the folder does not show and for an item with no binding.
    pub async fn find_path_status(&self, path: &Path) -> Result<ItemPathStatus, FilesRefusal> {
        todo!()
    }

    /// `Files.Hydrate`: downloads the file at `path` now and waits for its landing; for a
    /// directory, every file below it that is not on disk
    /// (`ItemReader::list_shown_files_below`).
    pub async fn download(&self, path: &Path) -> Result<(), FilesRefusal> {
        todo!()
    }

    /// `Files.FreeUp` / `Dehydrate`: frees the copy at `path`, or of every file below a
    /// directory, each as one job of the write-operation thread
    /// (`ItemFreeUp::free_downloaded_copy`); what was freed and what was left.
    pub async fn free_up(&self, path: &Path) -> Result<FreedSpace, FilesRefusal> {
        todo!()
    }

    /// `Folder.FreeUpSpace`: as [`free_up`](Self::free_up) of the root.
    pub async fn free_up_all(&self) -> Result<FreedSpace, FilesRefusal> {
        todo!()
    }

    /// `Files.Pin` / `Unpin`: `ItemStore::set_item_pin` as one job of the store, then the
    /// plan woken (`WakeCause::UserAnswered`), which downloads what the pin holds.
    pub async fn set_pin(&self, path: &Path, is_pinned: bool) -> Result<(), FilesRefusal> {
        todo!()
    }

    /// `Folder.PinnedCount` (`ItemReader::count_pins`).
    pub async fn count_pins(&self) -> u64 {
        todo!()
    }

    /// How much is downloaded at and below `path`: the sizes of the local contents of the
    /// rows there that say on disk (`ItemReader::list_shown_files_below`); the backing
    /// directory is not walked.
    pub async fn count_downloaded_below(&self, path: &Path) -> Result<DownloadedSpace, FilesRefusal> {
        todo!()
    }
}
