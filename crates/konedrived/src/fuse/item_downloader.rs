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
//! **What the source refuses, typed.** The download (`download_staged`) answers an errno;
//! the source it fetches through (`download_source`) keeps what the content source
//! refused, and a download that failed is answered by it: a sign-out, a throttle, an item
//! OneDrive does not have, another content than the asked one.
//!
//! **Another content than was asked for** — OneDrive states another content than the row
//! by now — is told from the FIRST answer of the source, before a byte of it is written
//! and before the mark that reads go by is raised (`download_source`): the download ends
//! there and fetches no further, its waiters hear `ESTALE`, nothing lands, nothing of its
//! file is kept, and the download states remember it
//! (`ItemDownloadStates::finish_as_another_content`). So a read is answered from a
//! running download's file only once that download is known to be of the content the
//! handle was opened as. What is remembered answers nothing by itself: the next download
//! of that item and content asks OneDrive anew — a read of such a content asks for
//! exactly that (`item_kernel_requests`), and the plan's next step for a pinned file
//! does — and it is forgotten at the first answer that download's source accepts
//! (`download_source`), not at its start.
//!
//! **A download of a content that is remembered so** is shown and told from OneDrive's
//! answer on, not from its asking: it is not announced as being downloaded at its start,
//! and where its first answer states another content again, its end is no news
//! ([`decide_end_news`]) — no activity event, no count of the space, no announcement.
//! The first time a content is found to be another one, everything is told. (`Transfers`
//! shows its entry from the fetch on, as for every download: `hydration::tracked`.)
//!
//! **An item OneDrive does not have** (`DownloadRefusal::ItemGone`: the source's "no such
//! item"), and an answer that tells neither a size nor a version, state no other content:
//! the download failed, as by any other refusal. Its waiters hear the errno, nothing is
//! remembered, and its unfinished file is kept to be gone on from.
//!
//! **The class a download runs in** is the starter's ([`decide_pool_class`]) — until
//! anything but the plan asks for a download the plan started and that still waits for
//! its slot: it gives that wait up and asks for an open's slot, which a pause does not
//! hold back. A program is never kept in `open()` for as long as syncing is paused.
//!
//! **After the stop** (`stop`) nothing is started: an asking answers its waiter `EIO` and
//! fetches nothing (`ItemDownloadStates::stop`).
//!
//! **The plan and another asker of one download.** The landing's condition is the
//! starter's. Where the plan started it and a stop aborted the plan's step, the landing
//! answers `StepStopped`; the download, when it still runs then, was asked for by
//! somebody else too (the plan's cancel aborts one it alone asked for), and its file is
//! landed for them, as a read's is: by the content alone. Where a read started it and
//! the plan joined, the landing does not commit as the plan's step: the plan's step is
//! answered that the rows changed, which they did — the row says on disk — and the next
//! plan has no download to plan.
//!
//! **What is shown.** A download is shown under its file's path, and announced by its
//! item and place; the downloader holds no store, so both are the landing's to read
//! (`ItemDownloadLanding::find_shown_place`) and to answer (`ItemLandingOutcome::Landed`).
//! How far the plan's download got is not told to `RunningSteps`: the downloader does not
//! hold them; `Transfers` shows it, as it shows every download.
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
// test: the_plans_download_is_cancelled_when_the_plan_alone_asked_for_it
// test: a_download_a_read_joined_goes_on_after_the_plans_cancel
// test: a_download_aborted_and_asked_again_is_not_ended_by_the_aborted_tasks_guard
// test: the_pools_class_is_open_for_every_asker_but_the_plans_pin
// test: the_landing_asks_every_version_for_the_plans_pin_and_the_content_alone_for_anyone_else
// test: a_stop_aborts_every_download
// test: a_refusal_of_the_source_is_typed_by_what_the_plans_task_does_about_it
// test: content_of_another_size_or_version_than_was_asked_for_is_not_the_asked_content
// test: a_download_of_content_onedrive_no_longer_has_lands_nothing
// test: a_plan_that_joins_a_reads_download_is_answered_that_the_rows_changed
// test: an_answer_of_the_source_is_used_by_its_content_and_its_place_case_by_case
// test: an_answer_of_another_content_ends_the_download_before_the_mark_is_raised_or_a_byte_read
// test: a_refusal_is_kept_until_the_same_range_is_answered_and_the_gravest_is_told
// test: a_download_asked_for_after_the_stop_answers_eio_and_fetches_nothing
// test: an_open_for_writing_that_joins_the_plans_download_does_not_wait_for_the_pause
// test: a_download_whose_landing_removed_what_it_named_starts_the_next_with_a_new_file
// test: a_download_of_an_item_onedrive_does_not_have_fails_and_keeps_its_unfinished_file
// test: a_size_the_source_does_not_tell_is_no_other_content
// test: the_mark_is_taken_back_the_moment_the_download_does_not_trust_what_is_on_disk
// test: what_was_remembered_of_a_key_is_void_at_the_first_answer_its_next_download_accepts
// test: the_end_of_a_download_is_news_per_case
// test: a_content_onedrive_keeps_stating_another_one_for_is_told_once
#![allow(dead_code)]

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use konedrive_graph::pool::{Class, Size, Slot, TransferPool};
use konedrive_graph::quickxor::decode_base64;
use konedrive_tree::{ContentVersion, ItemVersions, LocalItemId, OneDriveItemReference};
use tokio::sync::oneshot;

use super::backing_directory::BackingDirectory;
use super::download_states::Waiter;
use super::item_announcements::ItemAnnouncement;
use super::item_content_files::{to_content_time, write_whole_copy_marks};
use super::item_download_landing::{ItemDownloadLanding, ItemLandingOutcome, LandingCondition};
use super::item_download_states::{DownloadAsker, DownloadKey, DownloadRun, ItemDownloadStart, ItemDownloadStates};
use super::item_inotify_bridge::ItemInotifyBridge;
use super::write_operation_thread::WriteOperationThread;
use crate::hydration::source::{download_staged, ContentSource, Share, Split, Version};
use crate::hydration::tracked::Tracked;
use crate::status::activity::{self, Kind as EventKind};
use crate::status::report::Report;

mod download_source;

use download_source::DownloadSource;

/// A file of OneDrive that is to be downloaded whole. Who asks, and for the plan the
/// versions its step was planned from, is not said here: `DownloadAsker` says it, once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ItemToDownload {
    pub(super) item: LocalItemId,
    /// What the content source is asked by.
    pub(super) onedrive_item: OneDriveItemReference,
    /// The content to download: the item's local content, which is OneDrive's while the
    /// file is not on disk.
    pub(super) content: ContentVersion,
}

/// Why the content source did not give a file, by what the plan's task does about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadRefusal {
    /// The account is signed out (401): all downloads wait for the sign-in.
    SignedOut,
    /// 429 or 503, with the time the source named.
    Throttled { retry_after: Option<std::time::Duration> },
    /// OneDrive does not have the item (the source's "no such item"): this one asking
    /// failed, and nothing is said of the content.
    ItemGone,
    /// OneDrive stated another content of the item than the asked one: a size or a
    /// version that disagrees, in an answer to this download.
    AnotherContent,
    /// The network, or the download was aborted: tried again later.
    Transient { detail: String },
}

/// How one download ended: what its landing answered, or what the source refused.
pub(super) type DownloadEnd = Result<ItemLandingOutcome, DownloadRefusal>;

/// The pool's class a download of `asker` runs in: `Open` for a read, an open for writing
/// and `Files.Hydrate`, `Download` for the plan's pin. Pure.
pub(super) fn decide_pool_class(asker: &DownloadAsker) -> Class {
    match asker {
        DownloadAsker::Read | DownloadAsker::OpenForWriting | DownloadAsker::FilesInterface => Class::Open,
        DownloadAsker::PlanPin { .. } => Class::Download,
    }
}

/// What the landing of a download of `file` that `asker` started asks of the row: every
/// version for the plan's pin, the content alone for anyone else. Pure.
pub(super) fn decide_landing_condition(file: &ItemToDownload, asker: &DownloadAsker) -> LandingCondition {
    match asker {
        DownloadAsker::PlanPin { planned_from } => LandingCondition::VersionsUnchanged { item: planned_from.clone() },
        DownloadAsker::Read | DownloadAsker::OpenForWriting | DownloadAsker::FilesInterface => LandingCondition::ContentUnchanged { item: file.item, downloaded: file.content.clone() },
    }
}

/// Whether what OneDrive told — `size` bytes, when a size was told, of `version`, when the
/// source said one — is `content`, the content that was asked for. Pure. What was not
/// told disagrees with nothing: the content is told by what else was.
///
/// - Another size: no.
/// - The source said a version: it is the asked one when both have a QuickXorHash and the
///   hashes are equal; when either has none, when the asked content has no cTag or the
///   cTags are equal.
/// - The source said none: the size alone says it.
/// - Neither was told: nothing disagrees.
pub(super) fn decide_is_asked_content(content: &ContentVersion, size: Option<u64>, version: Option<&Version>) -> bool {
    let (asked_hash, asked_ctag) = match content {
        ContentVersion::Hashed { hash, ctag, .. } => (hash.as_deref().and_then(decode_base64), ctag.as_deref()),
        ContentVersion::ChangedHere { .. } => (None, None),
    };
    size.is_none_or(|size| content.size() == size)
        && version.is_none_or(|version| match (asked_hash, version.quick_xor) {
            (Some(asked), Some(downloaded)) => asked == downloaded,
            (None, _) | (_, None) => asked_ctag.is_none_or(|ctag| ctag == version.ctag),
        })
}

/// What a download that failed with `errno` is answered as: by what the source refused
/// (`DownloadSource::find_refusal`), when it refused; a failure of the disk or of the
/// check otherwise. Pure.
pub(super) fn decide_download_failure(errno: i32, refusal: Option<DownloadRefusal>) -> DownloadEnd {
    match refusal {
        Some(refusal) => Err(refusal),
        None => Ok(ItemLandingOutcome::Failed { errno }),
    }
}

/// Whether the end of a download is news to whoever is told of downloads: the activity,
/// the count of the space, the folder's watchers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DownloadEndNews {
    /// It is: the download landed, or failed, or OneDrive stated another content than
    /// anything known of the key said.
    Changed,
    /// It is not: OneDrive stated another content in the first answer to a download of a
    /// key the download states remembered so already. Nothing changed since that was
    /// told.
    AnotherContentAgain,
}

/// Whether a download that ended as `end` is news, by whether the download states still
/// remember its key as another content then (`is_another_content_remembered`:
/// `ItemDownloadStates::is_another_content`, asked before the end is told to them — an
/// answer the download accepted would have voided it). Pure.
pub(super) fn decide_end_news(end: &DownloadEnd, is_another_content_remembered: bool) -> DownloadEndNews {
    match end {
        Err(DownloadRefusal::AnotherContent) if is_another_content_remembered => DownloadEndNews::AnotherContentAgain,
        Ok(_) | Err(_) => DownloadEndNews::Changed,
    }
}

/// What whoever waits for a download is answered when it ended as `end`: nothing for a
/// landed file, else an errno. `ESTALE` when the row, or OneDrive's content, is not what
/// was asked for any more, and when OneDrive does not have the item. Pure.
pub(super) fn decide_waiters_answer(end: &DownloadEnd) -> Result<(), i32> {
    match end {
        Ok(ItemLandingOutcome::Landed { .. }) => Ok(()),
        Ok(ItemLandingOutcome::RowsChanged) | Err(DownloadRefusal::ItemGone | DownloadRefusal::AnotherContent) => Err(libc::ESTALE),
        Ok(ItemLandingOutcome::PlaceHeld { .. }) => Err(libc::EEXIST),
        Ok(ItemLandingOutcome::Failed { errno }) => Err(*errno),
        Ok(ItemLandingOutcome::StepStopped) | Err(DownloadRefusal::SignedOut | DownloadRefusal::Throttled { .. } | DownloadRefusal::Transient { .. }) => Err(libc::EIO),
    }
}

/// What the plan's step is answered when it joined a download somebody else started, by
/// what its waiter heard: the landing did not commit as the step. Pure.
///
/// - Landed, by the content alone: the row says on disk, which is not what the step was
///   planned from: the rows changed, and the next plan has nothing to download.
/// - `ESTALE`: the rows changed. `EEXIST`: the place is held (where is not known here).
/// - Anything else: tried again later.
pub(super) fn decide_joined_end(heard: Result<(), i32>) -> DownloadEnd {
    match heard {
        Ok(()) | Err(libc::ESTALE) => Ok(ItemLandingOutcome::RowsChanged),
        Err(libc::EEXIST) => Ok(ItemLandingOutcome::PlaceHeld { path: PathBuf::new() }),
        Err(errno) => Err(DownloadRefusal::Transient { detail: format!("the download failed: {}", std::io::Error::from_raw_os_error(errno)) }),
    }
}

/// Abandons a download in the download states when its task is dropped, whatever drops
/// it: for one that ended, or was aborted there, this changes nothing.
struct RunningDownload {
    download_states: Arc<ItemDownloadStates>,
    run: Arc<DownloadRun>,
}

impl Drop for RunningDownload {
    fn drop(&mut self) {
        self.download_states.abandon(&self.run);
    }
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
        Self { backing_directory, download_states, download_landing, inotify_bridge, write_operation_thread, source, pool, split_download_share, report, folder, runtime }
    }

    /// Downloads `file` for `asker` unless a download of the same item and content runs
    /// already, which `asker` then joins (`ItemDownloadStates::start_or_join`, which keeps
    /// who asked); `waiter` is answered when the download ends, however it ends. The
    /// pool's class follows from who asks ([`decide_pool_class`]). Spawns on the runtime
    /// and returns at once.
    ///
    /// The spawned task holds the `DownloadRun` its start answered and says every word of
    /// its download with it: the abort handle kept (`keep_abort_handle`), the file
    /// (`begin_writing`), its end (`finish`), and — by a guard it holds from its first
    /// line, whatever drops it — `abandon`. None of them touches a later download of the
    /// same item and content.
    ///
    /// Called by read at the threshold of bytes read as ranges (`DownloadAsker::Read`, no
    /// waiter), by a read of a content OneDrive stated another one for (`Read`, with a
    /// waiter: `ItemKernelRequests::fetch_range_after_download`), by the open for writing and the truncation of a file that is not on disk
    /// (`OpenForWriting`), by `ItemFilesInterface::download` (`FilesInterface`), and by
    /// [`download_and_land`](Self::download_and_land) (`PlanPin`, which carries the
    /// versions the step was planned from). The landing's condition follows from the
    /// asker that STARTED the download ([`decide_landing_condition`]); one that joins
    /// takes the landing it finds.
    pub(super) fn download(self: &Arc<Self>, file: ItemToDownload, asker: DownloadAsker, waiter: Option<Waiter>) {
        self.start_or_join(file, asker, waiter, None);
    }

    /// [`download`](Self::download), with who is told how the download ended, typed, when
    /// this asking starts it; an asking that joins hears its waiter alone, and `answer`
    /// is dropped.
    fn start_or_join(self: &Arc<Self>, file: ItemToDownload, asker: DownloadAsker, waiter: Option<Waiter>, answer: Option<oneshot::Sender<DownloadEnd>>) {
        let key = DownloadKey { item: file.item, content: file.content.clone() };
        let (run, unfinished_file) = match self.download_states.start_or_join(&key, &asker, waiter) {
            ItemDownloadStart::Started { run, unfinished_file } => (Arc::new(run), unfinished_file),
            // The waiter hears the download's end, or heard `EIO` after the stop.
            ItemDownloadStart::Joined | ItemDownloadStart::Stopped => return,
        };
        // Made before the spawn and moved into the task: a task that is aborted before
        // its first line still says its download is over.
        let running = RunningDownload { download_states: Arc::clone(&self.download_states), run: Arc::clone(&run) };
        let downloader = Arc::clone(self);
        let task = self.runtime.spawn(async move {
            let running = running;
            let end = downloader.run_download(&file, &asker, &running.run, unfinished_file).await;
            if let Some(answer) = answer {
                let _ = answer.send(end.clone());
            }
            // The task's last act: after it the key is free for another download. Only
            // an answer that stated another content is remembered as one.
            match &end {
                Err(DownloadRefusal::AnotherContent) => downloader.download_states.finish_as_another_content(&running.run),
                end => downloader.download_states.finish(&running.run, decide_waiters_answer(end)),
            }
        });
        self.download_states.keep_abort_handle(&run, task.abort_handle());
    }

    /// One download from its start to what it is shown and announced as: the place and
    /// the path the folder shows the file at are read, the change announced (the file is
    /// shown as being downloaded), the content fetched and landed
    /// ([`fetch_and_land`](Self::fetch_and_land)), the activity told — but for the plan's
    /// pin, whose step is told by the plan's task —, the space counted again, and the
    /// change announced.
    ///
    /// A download of a key remembered as another content
    /// (`ItemDownloadStates::is_another_content`) is not announced at its start: it asks
    /// OneDrive anew, and nothing is known to change yet. Where it ends as
    /// [`DownloadEndNews::AnotherContentAgain`], nothing is told at its end either.
    async fn run_download(self: &Arc<Self>, file: &ItemToDownload, asker: &DownloadAsker, run: &DownloadRun, unfinished_file: Option<Arc<File>>) -> DownloadEnd {
        let Some((place, path)) = self.download_landing.find_shown_place(file.item).await else { return Ok(ItemLandingOutcome::RowsChanged) };
        let shown_path = self.folder.join(&path).display().to_string();
        if !self.download_states.is_another_content(run.key()) {
            self.inotify_bridge.announce(vec![ItemAnnouncement::ContentChanged { item: file.item, place: place.clone() }]);
        }
        let end = self.fetch_and_land(file, asker, run, unfinished_file, &shown_path).await;
        // Asked before the end is told to the states, which remember the key anew then.
        if decide_end_news(&end, self.download_states.is_another_content(run.key())) == DownloadEndNews::AnotherContentAgain {
            return end;
        }
        if !matches!(asker, DownloadAsker::PlanPin { .. }) {
            let event = match (&end, decide_waiters_answer(&end)) {
                (Ok(ItemLandingOutcome::Landed { path, .. }), _) => activity::event(EventKind::Downloaded, self.folder.join(path).display().to_string(), activity::human_size(file.content.size())),
                (_, Err(errno)) => activity::event(EventKind::Failed, shown_path, activity::failure_reason(errno)),
                (_, Ok(())) => activity::event(EventKind::Downloaded, shown_path, activity::human_size(file.content.size())),
            };
            self.report.activity.record(vec![event]).await;
        }
        self.report.space.kick();
        let place = match &end {
            Ok(ItemLandingOutcome::Landed { place, .. }) => place.clone(),
            _ => place,
        };
        self.inotify_bridge.announce(vec![ItemAnnouncement::ContentChanged { item: file.item, place }]);
        end
    }

    /// The pool's slot the download `run`, which `asker` started, fetches in: of the
    /// asker's class ([`decide_pool_class`]). A download the plan started that still
    /// waits for its slot when anything else asks for it
    /// (`ItemDownloadStates::wait_for_asker_beyond_plan`) gives that wait up and asks for
    /// an open's slot: a pause holds back everything but opens, and a program waits for
    /// this one. A slot that is held already is kept as it is.
    async fn acquire_slot(&self, asker: &DownloadAsker, run: &DownloadRun, size: Size) -> Slot {
        match decide_pool_class(asker) {
            Class::Open => self.pool.acquire_sized(Class::Open, size).await,
            class => tokio::select! {
                slot = self.pool.acquire_sized(class, size) => slot,
                () = self.download_states.wait_for_asker_beyond_plan(run) => self.pool.acquire_sized(Class::Open, size).await,
            },
        }
    }

    /// Fetches the content of `file` into a file that has no name — the unfinished file
    /// of an earlier try, gone on from its checkpoint, or a new one —, in the pool's slot
    /// ([`acquire_slot`](Self::acquire_slot)), through a source that lets through no
    /// answer of another content (`download_source`); checks it is the content asked
    /// for; gives it the content's time and the marks of a whole copy; syncs it, with no
    /// lock held; and lands it as one job of the write-operation thread.
    async fn fetch_and_land(self: &Arc<Self>, file: &ItemToDownload, asker: &DownloadAsker, run: &DownloadRun, unfinished_file: Option<Arc<File>>, shown_path: &str) -> DownloadEnd {
        let fail = |error: std::io::Error| ItemLandingOutcome::Failed { errno: error.raw_os_error().unwrap_or(libc::EIO) };
        let size = file.content.size();
        let mut slot = self.acquire_slot(asker, run, Size::of(size)).await;
        let class = slot.class();
        // The file a download is built in has no name: it is in the backing directory's
        // filesystem and in no directory, so nothing lists a file that is not whole, and
        // one the daemon leaves behind goes by itself.
        let staged = match unfinished_file {
            Some(staged) => staged,
            None => match self.backing_directory.make_unnamed_file() {
                Ok(staged) => Arc::new(staged),
                Err(error) => return Ok(fail(error)),
            },
        };
        let tracked = match class {
            Class::Open => Tracked::opening(Arc::clone(&self.source), self.report.transfers.clone(), shown_path),
            _ => Tracked::new(Arc::clone(&self.source), self.report.transfers.clone(), shown_path),
        };
        // What of the download is on disk in order, for the reads that come meanwhile:
        // raised by the source alone, and only for answers of the asked content. The
        // source also tells the states of the first such answer.
        let written_up_to = self.download_states.begin_writing(run, Arc::clone(&staged));
        let source = DownloadSource::new(&tracked, file.content.clone(), written_up_to, &self.download_states, run);
        // A large file that nothing waits to open (a pinned one) goes in parallel parts,
        // the slot held here being its first stream's.
        let split = (class != Class::Open && Size::of(size) == Size::Large).then(|| Split::new(Arc::clone(&self.pool), Arc::clone(&self.split_download_share)));
        let downloaded = download_staged(&staged, file.onedrive_item.item_id.as_str(), size, &source, split.as_ref()).await;
        let refusal = source.find_refusal();
        drop(tracked);
        let downloaded = match downloaded {
            Ok(downloaded) => downloaded,
            Err(errno) => return decide_download_failure(errno, refusal),
        };
        // Every answer was held against the asked content already; asked once more of
        // what the download says it checked the whole file as.
        if !decide_is_asked_content(&file.content, Some(downloaded.size), downloaded.version.as_ref()) {
            tracing::info!("{shown_path}: OneDrive has another content than the one that was asked for; nothing lands");
            return Err(DownloadRefusal::AnotherContent);
        }
        // Whole from here on: it may be given a name, and is never gone on from.
        self.download_states.end_writing(run);
        // The time, the marks and the data synced here, by the downloader, with no lock
        // held: the landing's job, under the tree lock, waits for no file to reach the
        // disk, and a copy that has its name after a power loss has its marks too.
        let (to_sync, onedrive_item_id, content) = (Arc::clone(&staged), file.onedrive_item.item_id.clone(), file.content.clone());
        let synced = tokio::task::spawn_blocking(move || {
            konedrive_fs::placeholder::set_mtime(&to_sync, to_content_time(&content))?;
            write_whole_copy_marks(&to_sync, onedrive_item_id.as_str(), &content)?;
            to_sync.sync_all()
        });
        match synced.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Ok(fail(error)),
            Err(_) => return Ok(ItemLandingOutcome::Failed { errno: libc::EIO }),
        }
        let mut outcome = self.land(&staged, decide_landing_condition(file, asker)).await;
        if outcome == ItemLandingOutcome::StepStopped {
            // The plan's step was stopped and this download still runs: somebody else
            // asked for it too. Their file lands as a read's does.
            outcome = self.land(&staged, LandingCondition::ContentUnchanged { item: file.item, downloaded: file.content.clone() }).await;
        }
        if matches!(outcome, ItemLandingOutcome::Landed { .. }) {
            slot.succeeded();
        }
        Ok(outcome)
    }

    /// The landing of `staged` as one job of the write-operation thread, which is no
    /// thread of the runtime. A thread that is stopped drops the job: the folder is no
    /// longer served, and nothing lands.
    async fn land(&self, staged: &Arc<File>, condition: LandingCondition) -> ItemLandingOutcome {
        let (landing, staged) = (Arc::clone(&self.download_landing), Arc::clone(staged));
        self.write_operation_thread.run_and_wait(move || landing.land(&staged, &condition)).await.unwrap_or(ItemLandingOutcome::Failed { errno: libc::EIO })
    }

    /// As [`download`](Self::download) for the plan's pin (`DownloadAsker::PlanPin {
    /// planned_from }`), awaited: what the landing answered, or the source's refusal
    /// typed, for the plan's step (`FolderSteps::run_download_content`, which turns it
    /// into `StepRefusal::{SignedOut, Throttled, ItemGone, ItemChanged, Transient}`). Where the plan's
    /// asking joined a download somebody else started, the answer is made of what the
    /// plan's waiter heard ([`decide_joined_end`]); a download that was aborted, and one
    /// asked for after the stop — the mount stops — is `DownloadRefusal::Transient`.
    pub(super) async fn download_and_land(self: &Arc<Self>, file: ItemToDownload, planned_from: ItemVersions) -> Result<ItemLandingOutcome, DownloadRefusal> {
        let ((waiter, heard), (answer, answered)) = (oneshot::channel(), oneshot::channel());
        self.start_or_join(file, DownloadAsker::PlanPin { planned_from }, Some(waiter), Some(answer));
        // The waiter is answered by the download's last act: awaited also where the typed
        // answer came, so that the download is over, and its key free, when this returns.
        let (answered, heard) = (answered.await, heard.await);
        match (answered, heard) {
            (Ok(end), _) => end,
            (Err(_), Ok(heard)) => decide_joined_end(heard),
            (Err(_), Err(_)) => Err(DownloadRefusal::Transient { detail: "the download was given up".into() }),
        }
    }

    /// The plan gives the download of `item`'s `content` up — a pause, a sign-out, a stop
    /// of the sync: aborted when the plan's pin alone asked for it
    /// (`ItemDownloadStates::abort_if_plan_alone_asked`); one a read, an open for writing
    /// or `Files.Hydrate` asked for too goes on. Called when the task that awaits
    /// `download_and_land` is aborted (`RunningSteps::abort_requests`): by a guard that
    /// task holds.
    pub(super) fn cancel(&self, item: LocalItemId, content: &ContentVersion) {
        self.download_states.abort_if_plan_alone_asked(&DownloadKey { item, content: content.clone() });
    }

    /// Every download is aborted and whoever waits for one answered `EIO`, and none is
    /// started from here on: an asking answers its waiter `EIO` and fetches nothing
    /// (`ItemDownloadStates::stop`): the mount stops. After it no spawned download holds
    /// a clone of the store. The one path of the mount's stop to the downloads.
    pub(super) fn stop(&self) {
        self.download_states.stop();
    }
}

#[cfg(test)]
mod tests;
