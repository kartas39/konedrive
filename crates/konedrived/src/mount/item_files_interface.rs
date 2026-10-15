//! What the daemon's bus interface `Files` asks of a mounted folder, by path, over the
//! rows.
//!
//! **State owned:** none; it holds the parts it asks. A path is turned into an item by
//! ONE reader (`ItemReader::find_item_by_path`), here and nowhere else in `mount`.
//!
//! **Answers for:** `org.konedrive.Files` (`docs/design/desktop.md` §2.9) and `Folder.FreeUpSpace`,
//! `PinnedCount`: download now, free up, pin and unpin, the status of a path.
//!
//! **Tested without the rest**, over a store in memory and a temporary backing directory:
// test: a_path_outside_the_folder_and_a_path_the_folder_does_not_show_are_refused
// test: an_item_with_no_binding_is_not_in_onedrive
// test: a_download_asked_by_path_is_the_items_download_and_is_awaited
// test: a_directory_is_freed_file_by_file_each_by_its_own_decision
// test: a_pin_is_set_by_the_stores_job_and_the_plan_woken
// test: the_status_of_a_path_is_the_rows_sync_state_its_pin_and_what_runs_for_it
// test: a_directory_is_downloaded_file_by_file_and_a_failure_is_said_after_the_rest
// test: what_is_downloaded_below_a_path_is_counted_from_the_rows
// test: a_free_up_after_the_thread_stopped_is_refused
// test: a_download_asked_by_path_while_onedrive_asks_to_wait_is_refused_with_the_time_left
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_tree::{ContentPresence, FileSyncState, Item, ItemKind, LocalItemId, OneDriveItemReference, PinChange, Store, TreeError};
use tokio::sync::oneshot;

use super::item_free_up::FreeUpDecision;
use super::item_download_states::{DownloadAsker, ItemDownloadStates};
use super::item_downloader::{ItemDownloader, ItemToDownload};
use super::item_free_up::{FreedCopy, ItemFreeUp};
use super::errno::THROTTLE_WAIT_ERRNO;
use super::item_announcements::ItemAnnouncement;
use super::item_handles::ItemHandles;
use super::item_inotify_bridge::ItemInotifyBridge;
use super::write_operation_thread::WriteOperationThread;
use crate::sync_parts::running_steps::RunningSteps;
use crate::sync_parts::sync_plan_wake::{SyncPlanWake, WakeCause};

/// How many files below a directory are read from the rows, and asked for, at a time.
const FILES_AT_ONCE: usize = 500;

/// What the bus shows of one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ItemPathStatus {
    pub(crate) item: LocalItemId,
    pub(crate) kind: ItemKind,
    /// The binding: what `WebUrl` asks OneDrive by.
    pub(crate) onedrive_item: OneDriveItemReference,
    /// `None` for a directory.
    pub(crate) sync_state: Option<FileSyncState>,
    /// Whether a pin holds for the item: the nearest mark at it or above it is a pin.
    pub(crate) is_pinned: bool,
    pub(crate) is_downloading: bool,
    pub(crate) is_open_for_writing: bool,
}

impl ItemPathStatus {
    /// What `user.konedrive.state` says of the item (`item_lookup::decide_state_attribute`
    /// of `sync_state` and `is_downloading`): what `Files.Menu` shows a file as. `None`
    /// for a directory and for a file that is no file of OneDrive's.
    pub(crate) fn find_state_attribute(&self) -> Option<konedrive_fs::placeholder::State> {
        super::item_lookup::decide_state_attribute(self.sync_state, self.is_downloading)
    }
}

/// What a free-up counts for one file it asked about: freed, kept back, or neither — a
/// file that had nothing to free. Pure.
pub(super) fn count_freed_copy(freed: FreedSpace, copy: FreedCopy) -> FreedSpace {
    match copy {
        FreedCopy::Freed { bytes } => FreedSpace { files: freed.files + 1, bytes: freed.bytes + bytes, ..freed },
        FreedCopy::Left(FreeUpDecision::NothingToFree) => freed,
        FreedCopy::Left(_) | FreedCopy::RowsChanged => FreedSpace { kept: freed.kept + 1, ..freed },
    }
}

/// The file `item` as a download is asked for: `None` for what has nothing to download —
/// a directory, a file whose content is on disk, an item OneDrive does not have. Pure.
pub(super) fn find_file_to_download(item: &Item) -> Option<ItemToDownload> {
    let content = item.local.as_ref()?.content.clone()?;
    let is_not_on_disk = item.kind == ItemKind::File && item.content_presence == ContentPresence::NotOnDisk;
    Some(ItemToDownload { item: item.local_item_id, onedrive_item: item.onedrive_item.clone().filter(|_| is_not_on_disk)?, content })
}

fn to_refusal(error: TreeError) -> FilesRefusal {
    FilesRefusal::Io(error.to_string())
}

/// The `Files` interface of one mount.
pub(crate) struct ItemFilesInterface {
    /// The folder's path, which is the mount.
    folder: PathBuf,
    store: Store,
    download_states: Arc<ItemDownloadStates>,
    downloader: Arc<ItemDownloader>,
    free_up: Arc<ItemFreeUp>,
    item_handles: Arc<ItemHandles>,
    write_operation_thread: Arc<WriteOperationThread>,
    sync_plan_wake: Arc<SyncPlanWake>,
    /// Told when a pin was set or taken off (`ItemAnnouncement::MarksChanged`).
    inotify_bridge: Arc<ItemInotifyBridge>,
    /// Whether a step runs for an item: what a free-up leaves a file for
    /// ([`decide_free_up`](Self::decide_free_up) reads it as the free-up does).
    running_steps: Arc<RunningSteps>,
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
        inotify_bridge: Arc<ItemInotifyBridge>,
        running_steps: Arc<RunningSteps>,
    ) -> Self {
        Self { folder, store, download_states, downloader, free_up, item_handles, write_operation_thread, sync_plan_wake, inotify_bridge, running_steps }
    }

    /// The item of OneDrive the folder shows at `path`, a full path, with whether a pin
    /// holds for it: by `ItemReader::find_item_by_path`, the one reader that turns a path
    /// into an item, on the store's reading connection.
    async fn find_onedrive_item(&self, path: &Path) -> Result<(Item, bool), FilesRefusal> {
        let relative = path.strip_prefix(&self.folder).map(Path::to_owned).map_err(|_| FilesRefusal::OutsideFolder)?;
        let read = self.store.read_items(move |reader| {
            let Some(item) = reader.find_item_by_path(&relative)?.filter(|item| item.onedrive_item.is_some()) else { return Ok(None) };
            Ok(Some((reader.is_pinned(item.local_item_id)?, item)))
        });
        let (is_pinned, item) = read.await.map_err(to_refusal)?.ok_or(FilesRefusal::NotInOneDrive)?;
        Ok((item, is_pinned))
    }

    /// The files the folder shows below `directory`, after the file `after`, a part at a
    /// time (`ItemReader::list_shown_files_below`).
    async fn list_files_below(&self, directory: LocalItemId, after: Option<LocalItemId>) -> Result<Vec<Item>, FilesRefusal> {
        self.store.read_items(move |reader| reader.list_shown_files_below(directory, after, FILES_AT_ONCE)).await.map_err(to_refusal)
    }

    /// The status of `path`, a full path: the item by `ItemReader::find_item_by_path` on
    /// the store's reading connection, its pin (`is_pinned`), and what runs for it.
    /// `OutsideFolder` for a path that is not below the folder, `NotInOneDrive` for a name
    /// the folder does not show and for an item with no binding.
    pub(crate) async fn find_path_status(&self, path: &Path) -> Result<ItemPathStatus, FilesRefusal> {
        let (item, is_pinned) = self.find_onedrive_item(path).await?;
        let Some(onedrive_item) = item.onedrive_item.clone() else { return Err(FilesRefusal::NotInOneDrive) };
        let id = item.local_item_id;
        Ok(ItemPathStatus { item: id, kind: item.kind, onedrive_item, sync_state: item.file_sync_state(), is_pinned, is_downloading: self.download_states.is_downloading(id), is_open_for_writing: self.item_handles.is_open_for_writing(id) })
    }

    /// Asks for the download of each of `files` and waits for them all: the first
    /// failure, when there is one, after the rest was waited for. While OneDrive asks the
    /// account to wait no download is started, and one that runs is given up
    /// (`ItemDownloader::download`): [`FilesRefusal::ThrottleWait`].
    async fn download_files(&self, files: Vec<ItemToDownload>) -> Result<(), FilesRefusal> {
        let asked: Vec<oneshot::Receiver<Result<(), i32>>> = files
            .into_iter()
            .map(|file| {
                let (waiter, heard) = oneshot::channel();
                self.downloader.download(file, DownloadAsker::FilesInterface, Some(waiter));
                heard
            })
            .collect();
        let mut failure = Ok(());
        for heard in asked {
            let downloaded = match heard.await {
                Ok(Ok(())) => Ok(()),
                // Not started, or given up: OneDrive asked the account to wait.
                Ok(Err(THROTTLE_WAIT_ERRNO)) => Err(FilesRefusal::ThrottleWait { seconds_left: self.downloader.throttle_wait().seconds_left() }),
                Ok(Err(errno)) => Err(FilesRefusal::Io(format!("the download failed: {}", std::io::Error::from_raw_os_error(errno)))),
                Err(_) => Err(FilesRefusal::Io("the download was given up".into())),
            };
            failure = failure.and(downloaded);
        }
        failure
    }

    /// `Files.Hydrate`: downloads the file at `path` now and waits for its landing; for a
    /// directory, every file below it that is not on disk
    /// (`ItemReader::list_shown_files_below`), a part of them at a time. A file whose
    /// content is on disk has nothing to download.
    pub(crate) async fn download(&self, path: &Path) -> Result<(), FilesRefusal> {
        let (item, _) = self.find_onedrive_item(path).await?;
        if item.kind == ItemKind::File {
            return self.download_files(find_file_to_download(&item).into_iter().collect()).await;
        }
        let (mut after, mut failure) = (None, Ok(()));
        loop {
            let files = self.list_files_below(item.local_item_id, after).await?;
            let Some(last) = files.last() else { return failure };
            after = Some(last.local_item_id);
            failure = failure.and(self.download_files(files.iter().filter_map(find_file_to_download).collect()).await);
        }
    }

    /// Frees the copy of `item` as one job of the write-operation thread, waited for. A
    /// thread that is stopped drops the job: the free-up ends there, refused.
    async fn free_copy(&self, item: LocalItemId) -> Result<FreedCopy, FilesRefusal> {
        let free_up = Arc::clone(&self.free_up);
        let freed = self.write_operation_thread.run_and_wait(move || free_up.free_downloaded_copy(item)).await;
        freed.map_err(|_| FilesRefusal::Io("the free-up was given up: the folder is no longer served".into()))
    }

    /// Frees the copies at and below `item`: of a file its own, of a directory those of
    /// the files the rows say are on disk below it, read a part at a time, each freed by
    /// its own decision in its own job, so that a program's write waits for one file of a
    /// long free-up and not for all of them.
    async fn free_up_below(&self, item: &Item) -> Result<FreedSpace, FilesRefusal> {
        if item.kind == ItemKind::File {
            return Ok(count_freed_copy(FreedSpace::default(), self.free_copy(item.local_item_id).await?));
        }
        let (mut after, mut freed) = (None, FreedSpace::default());
        loop {
            let files = self.list_files_below(item.local_item_id, after).await?;
            let Some(last) = files.last() else { return Ok(freed) };
            after = Some(last.local_item_id);
            for file in files.iter().filter(|file| file.content_presence == ContentPresence::OnDisk) {
                freed = count_freed_copy(freed, self.free_copy(file.local_item_id).await?);
            }
        }
    }

    /// `Files.FreeUp` / `Dehydrate`: frees the copy at `path`, or of every file below a
    /// directory, each as one job of the write-operation thread
    /// (`ItemFreeUp::free_downloaded_copy`); what was freed and what was left.
    pub(crate) async fn free_up(&self, path: &Path) -> Result<FreedSpace, FilesRefusal> {
        let (item, _) = self.find_onedrive_item(path).await?;
        self.free_up_below(&item).await
    }

    /// `Folder.FreeUpSpace`: as [`free_up`](Self::free_up) of the root.
    pub(crate) async fn free_up_all(&self) -> Result<FreedSpace, FilesRefusal> {
        let folder = self.folder.clone();
        self.free_up(&folder).await
    }

    /// `Files.Pin` / `Unpin` / `FreeUp`: `ItemStore::set_item_pin` as one job of the
    /// store — a pin holds for the item from now on, or does not, whatever a directory
    /// above it has, and every mark below a directory is off — then the plan woken
    /// (`WakeCause::UserAnswered`), which downloads what the pin holds, and the change
    /// announced to whoever watches the folder (`ItemInotifyBridge::announce` with
    /// `ItemAnnouncement::MarksChanged` of the item at its place: the emblem is read from
    /// `user.konedrive.pin`). Answers whether a mark changed: `false` where a pin held for
    /// the item already, or held not, and there was no mark below it.
    pub(crate) async fn set_pin(&self, path: &Path, is_pinned: bool) -> Result<bool, FilesRefusal> {
        let (item, _) = self.find_onedrive_item(path).await?;
        let id = item.local_item_id;
        match self.store.call(move |store| store.item_store().set_item_pin(id, is_pinned)).await.map_err(to_refusal)? {
            PinChange::Written => {
                self.sync_plan_wake.wake(WakeCause::UserAnswered);
                // The emblem is read from the pin: whoever watches the folder is told it changed.
                if let Some(place) = item.local.and_then(|local| local.place) {
                    self.inotify_bridge.announce(vec![ItemAnnouncement::MarksChanged { item: id, place }]);
                }
                Ok(true)
            }
            PinChange::AlreadySo => Ok(false),
            PinChange::NoBinding | PinChange::NoItem => Err(FilesRefusal::NotInOneDrive),
        }
    }

    /// What a free-up of the item at `path` would decide now
    /// (`item_free_up::decide_item_free_up`, with the row, its pin, what runs for it and
    /// whether it is open for writing, read as a free-up reads them) — and, with
    /// `is_pin_taken_off`, what it would decide once no pin holds for the item, as
    /// `Files.FreeUp` makes it first. Changes nothing. What `Dehydrate`, `FreeUp`'s check
    /// and `Files.Menu` say as a refusal or offer. For a directory: `LeavePinned` or
    /// `NothingToFree`, by the pin that holds for its own row and nothing else; its files
    /// are decided each by itself when the free-up runs. For a file that is not on disk:
    /// `NothingToFree`, whatever keeps it, as the free-up answers it
    /// (`item_free_up::find_copy_on_disk`).
    pub(crate) async fn decide_free_up(&self, path: &Path, is_pin_taken_off: bool) -> Result<FreeUpDecision, FilesRefusal> {
        let (item, is_pinned) = self.find_onedrive_item(path).await?;
        let id = item.local_item_id;
        let is_pinned = is_pinned && !is_pin_taken_off;
        Ok(match (item.kind, super::item_free_up::find_copy_on_disk(&item)) {
            // A directory has no copy of its own, and nothing that runs for its row keeps the files below it: the free-up decides each of them by itself.
            (ItemKind::Directory, _) if is_pinned => FreeUpDecision::LeavePinned,
            (ItemKind::Directory, _) => FreeUpDecision::NothingToFree,
            // As the free-up answers it before its decision (`ItemFreeUp::write_not_on_disk`): what is not on disk takes no space, whatever keeps it.
            (ItemKind::File, None) => FreeUpDecision::NothingToFree,
            (ItemKind::File, Some(_)) => {
                // As the free-up reads it: a file a step runs for is left as one that is being downloaded is.
                let is_downloading_or_sent = self.download_states.is_downloading(id) || self.running_steps.has_running_step(id);
                super::item_free_up::decide_item_free_up(&item, is_pinned, is_downloading_or_sent, self.item_handles.is_open_for_writing(id))
            }
        })
    }

    /// The files whose whole download began and has not landed: a download that runs now,
    /// and the unfinished files earlier downloads left to be gone on from
    /// (`ItemDownloadStates::list_unfinished_files`, which holds them: none has a name in
    /// the backing directory). Of each: its full path, its size on disk and
    /// its checkpoint (`user.konedrive.progress`; none where none was written). For the
    /// test support of the daemon (`account_folder::testing::staged`); no member of the
    /// bus asks it.
    pub(crate) async fn list_unfinished_downloads(&self) -> Result<Vec<(PathBuf, u64, Option<u64>)>, FilesRefusal> {
        let files = self.download_states.list_unfinished_files();
        let items: Vec<LocalItemId> = files.iter().map(|(item, _)| *item).collect();
        let paths = self.store.read_items(move |reader| items.into_iter().map(|item| reader.find_path(item)).collect::<Result<Vec<_>, _>>()).await.map_err(to_refusal)?;
        let mut unfinished = Vec::new();
        for ((_, file), path) in files.into_iter().zip(paths) {
            // A file the folder shows no more by now has no path to list it under.
            let Some(path) = path else { continue };
            let size = file.metadata().map_err(|error| FilesRefusal::Io(error.to_string()))?.len();
            let checkpoint = konedrive_fs::placeholder::read_progress(&file).map_err(|error| FilesRefusal::Io(error.to_string()))?.map(|progress| progress.bytes);
            unfinished.push((self.folder.join(path), size, checkpoint));
        }
        Ok(unfinished)
    }

    /// `Folder.PinnedCount` (`ItemReader::count_pins`).
    pub(crate) async fn count_pins(&self) -> u64 {
        self.store.read_items(|reader| reader.count_pins()).await.unwrap_or_else(|error| {
            tracing::warn!("the pins could not be counted: {error}");
            0
        })
    }

    /// How much is downloaded at and below `path`: the sizes of the local contents of the
    /// rows there that say on disk (`ItemReader::list_shown_files_below`); the backing
    /// directory is not walked.
    pub(crate) async fn count_downloaded_below(&self, path: &Path) -> Result<DownloadedSpace, FilesRefusal> {
        let (item, _) = self.find_onedrive_item(path).await?;
        let count = |counted: DownloadedSpace, file: &Item| match (file.content_presence, file.local.as_ref().and_then(|local| local.content.as_ref())) {
            (ContentPresence::OnDisk, Some(content)) if file.kind == ItemKind::File => DownloadedSpace { files: counted.files + 1, bytes: counted.bytes + content.size() },
            _ => counted,
        };
        if item.kind == ItemKind::File {
            return Ok(count(DownloadedSpace::default(), &item));
        }
        let (mut after, mut counted) = (None, DownloadedSpace::default());
        loop {
            let files = self.list_files_below(item.local_item_id, after).await?;
            let Some(last) = files.last() else { return Ok(counted) };
            after = Some(last.local_item_id);
            counted = files.iter().fold(counted, count);
        }
    }
}

/// What a free-up did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FreedSpace {
    pub(crate) files: u64,
    pub(crate) bytes: u64,
    /// Downloaded files left as they are: pinned, being downloaded, or changed here and not
    /// uploaded yet.
    pub(crate) kept: u64,
}

/// The space the downloaded copies at and below a path take: how many files, and their
/// bytes on disk.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DownloadedSpace {
    pub(crate) files: u64,
    pub(crate) bytes: u64,
}

/// Why the mount refuses what the `Files` interface asked.
#[derive(Debug, thiserror::Error)]
pub(crate) enum FilesRefusal {
    #[error("not inside this folder")]
    OutsideFolder,
    /// A file or directory that is not in OneDrive yet, or a name the folder does not have.
    #[error("not in OneDrive")]
    NotInOneDrive,
    #[error("{0}")]
    Io(String),
    /// A download was asked for while OneDrive asks the account to wait: nothing is
    /// downloaded until the wait, of which this many seconds are left, is over.
    #[error("OneDrive asked to wait: {seconds_left} s left, and nothing is downloaded until then")]
    ThrottleWait { seconds_left: u32 },
}

#[cfg(test)]
mod tests;
