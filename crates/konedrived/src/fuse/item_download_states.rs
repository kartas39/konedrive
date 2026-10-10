//! The state of each file's download, by the item and the content being downloaded: what
//! `download_states` is today by OneDrive id.
//!
//! **State owned:** per (item, content version): a running download with who waits for
//! it, how far it was written and its file; or a failed one with its unfinished file kept
//! to go on from; the handle each running download's task is aborted by, which download
//! of its key it is, and whether anything but the plan asked for it; and how many
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
// test: a_download_only_the_plan_asked_for_is_aborted_by_its_cancel_and_keeps_its_unfinished_file
// test: a_download_anything_but_the_plan_asked_for_is_not_aborted_by_the_plans_cancel
// test: an_end_told_by_an_older_download_of_the_key_changes_nothing_of_the_newer
#![allow(dead_code, unused_variables)]

use std::collections::HashMap;
use std::fs::File;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use konedrive_tree::{ContentVersion, LocalItemId};

use super::download_states::{reaches_download_threshold, RunningDownloadFile, Waiter};

/// How many downloads that failed keep their unfinished file, to be gone on from: the
/// latest ones.
const UNFINISHED_FILES_KEPT: usize = 16;

/// What a download is of.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct DownloadKey {
    pub(super) item: LocalItemId,
    pub(super) content: ContentVersion,
}

/// Who asks for a download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DownloadAsker {
    /// A read, at the threshold of bytes read as ranges.
    Read,
    /// An open for writing, or a truncation, of a file that is not on disk.
    OpenForWriting,
    /// `Files.Hydrate`.
    FilesInterface,
    /// The plan's step for a pinned file: the one asker whose asking is taken back
    /// (`ItemDownloadStates::abort_if_plan_alone_asked`).
    PlanPin,
}

/// One download that runs, told from every other — also from an earlier and a later one
/// of the same key. Handed out when a download is started, and given back with every
/// word of that download: what an older download says changes nothing of a newer one.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct DownloadRun {
    key: DownloadKey,
    number: u64,
}

impl DownloadRun {
    /// What the download is of.
    pub(super) fn key(&self) -> &DownloadKey {
        &self.key
    }
}

/// What became of an asking for a download.
pub(super) enum ItemDownloadStart {
    /// A download of this item and content runs: the asker, and its waiter, were added
    /// to it.
    Joined,
    /// This asking starts it: the download it starts, and the unfinished file of an
    /// earlier try, when one was kept.
    Started { run: DownloadRun, unfinished_file: Option<Arc<File>> },
}

enum State {
    Running {
        /// Which download it is (`DownloadRun`).
        number: u64,
        waiters: Vec<Waiter>,
        written_up_to: Arc<AtomicU64>,
        file: Option<Arc<File>>,
        abort: Option<tokio::task::AbortHandle>,
        /// Whether anything but the plan's pin asked for it, with a waiter or without.
        is_asked_beyond_plan: bool,
    },
    Failed { unfinished_file: Arc<File>, failure_number: u64 },
}

/// A running download taken out of the states: who waited for it, the file it wrote
/// into, and what its task is aborted by.
struct TakenDownload {
    waiters: Vec<Waiter>,
    file: Option<Arc<File>>,
    abort: Option<tokio::task::AbortHandle>,
}

impl TakenDownload {
    /// Aborts the task and answers the waiters `EIO`. Called with no lock held.
    fn abort(self) {
        if let Some(abort) = self.abort {
            abort.abort();
        }
        for waiter in self.waiters {
            let _ = waiter.send(Err(libc::EIO));
        }
    }
}

/// The running download of `key`, taken out of the states. `None` when no download of it
/// runs, and nothing is changed.
fn take_running(states: &mut HashMap<DownloadKey, State>, key: &DownloadKey) -> Option<TakenDownload> {
    if !matches!(states.get(key), Some(State::Running { .. })) {
        return None;
    }
    match states.remove(key) {
        Some(State::Running { waiters, file, abort, .. }) => Some(TakenDownload { waiters, file, abort }),
        Some(State::Failed { .. }) | None => None,
    }
}

/// Whether `run` is the download that runs now for its key: not one that ended, was
/// aborted or abandoned, whatever runs for the key since.
fn is_running(states: &HashMap<DownloadKey, State>, run: &DownloadRun) -> bool {
    matches!(states.get(&run.key), Some(State::Running { number, .. }) if *number == run.number)
}

/// The download of `key` ended with `unfinished_file` written in part: the file is kept,
/// and the oldest one beyond [`UNFINISHED_FILES_KEPT`] is let go.
fn keep_unfinished_file(states: &mut HashMap<DownloadKey, State>, key: &DownloadKey, unfinished_file: Arc<File>) {
    let failure_number = list_failure_numbers(states).map(|(_, number)| number + 1).max().unwrap_or(0);
    states.insert(key.clone(), State::Failed { unfinished_file, failure_number });
    if list_failure_numbers(states).count() <= UNFINISHED_FILES_KEPT {
        return;
    }
    let Some(oldest) = list_failure_numbers(states).min_by_key(|(_, number)| *number).map(|(key, _)| key.clone()) else { return };
    states.remove(&oldest);
}

/// The key and the failure number of every `Failed` state.
fn list_failure_numbers(states: &HashMap<DownloadKey, State>) -> impl Iterator<Item = (&DownloadKey, u64)> {
    states.iter().filter_map(|(key, state)| match state {
        State::Failed { failure_number, .. } => Some((key, *failure_number)),
        State::Running { .. } => None,
    })
}

/// The download states of one mount.
pub(super) struct ItemDownloadStates {
    states: std::sync::Mutex<HashMap<DownloadKey, State>>,
    bytes_read_as_ranges: std::sync::Mutex<HashMap<LocalItemId, u64>>,
    /// `[transfers] download_after`.
    download_after: u64,
    /// The number of the download started last.
    last_run_number: AtomicU64,
}

impl ItemDownloadStates {
    /// No download runs. Made by the mount's build.
    pub(super) fn new(download_after: u64) -> Self {
        ItemDownloadStates { states: std::sync::Mutex::default(), bytes_read_as_ranges: std::sync::Mutex::default(), download_after, last_run_number: AtomicU64::new(0) }
    }

    /// A download of `key` is asked for by `asker`: joined when one runs, started
    /// otherwise. `waiter`, when one is given, is answered when the download ends, however
    /// it ends. Who asked is kept: a download anything but the plan's pin asked for, with
    /// a waiter or without, is not aborted for the plan. Called by
    /// `ItemDownloader::download` alone.
    pub(super) fn start_or_join(&self, key: &DownloadKey, asker: DownloadAsker, waiter: Option<Waiter>) -> ItemDownloadStart {
        let is_asker_beyond_plan = asker != DownloadAsker::PlanPin;
        let mut states = crate::panic::lock(&self.states);
        if let Some(State::Running { waiters, is_asked_beyond_plan, .. }) = states.get_mut(key) {
            waiters.extend(waiter);
            *is_asked_beyond_plan |= is_asker_beyond_plan;
            return ItemDownloadStart::Joined;
        }
        let number = self.last_run_number.fetch_add(1, Ordering::SeqCst) + 1;
        let running = State::Running { number, waiters: waiter.into_iter().collect(), written_up_to: Arc::default(), file: None, abort: None, is_asked_beyond_plan: is_asker_beyond_plan };
        let unfinished_file = match states.insert(key.clone(), running) {
            Some(State::Failed { unfinished_file, .. }) => Some(unfinished_file),
            Some(State::Running { .. }) | None => None,
        };
        ItemDownloadStart::Started { run: DownloadRun { key: key.clone(), number }, unfinished_file }
    }

    /// The task of the download `run` is aborted by `abort`. Called by the downloader
    /// right after it spawned the task.
    ///
    /// When `run` runs no more, the task is aborted at once: it was cancelled, or the
    /// mount stopped, between its asking and this call — or it has ended already, and the
    /// abort does nothing. A newer download of the same key is not touched.
    pub(super) fn keep_abort_handle(&self, run: &DownloadRun, abort: tokio::task::AbortHandle) {
        let mut states = crate::panic::lock(&self.states);
        match states.get_mut(&run.key) {
            Some(State::Running { number, abort: kept, .. }) if *number == run.number => *kept = Some(abort),
            _ => abort.abort(),
        }
    }

    /// The plan gives the download of `key` up: aborted when the plan's pin alone asked
    /// for it — nothing else started or joined it, with a waiter or without — its waiters
    /// answered `EIO` and its unfinished file kept as after a failure. Whether it was
    /// aborted. A download a read, an open for writing or `Files.Hydrate` asked for too
    /// goes on, whoever still listens.
    ///
    /// The state is changed here, at once. What the aborted task still says of its
    /// download — its guard's [`abandon`](Self::abandon), a late
    /// [`finish`](Self::finish) — is of a `DownloadRun` that runs no more, and changes
    /// nothing, whatever runs for the key by then.
    pub(super) fn abort_if_plan_alone_asked(&self, key: &DownloadKey) -> bool {
        let taken = {
            let mut states = crate::panic::lock(&self.states);
            if !matches!(states.get(key), Some(State::Running { is_asked_beyond_plan: false, .. })) {
                return false;
            }
            let Some(taken) = take_running(&mut states, key) else { return false };
            if let Some(file) = &taken.file {
                keep_unfinished_file(&mut states, key, Arc::clone(file));
            }
            taken
        };
        taken.abort();
        true
    }

    /// Aborts every running download and answers its waiters `EIO`: the mount stops.
    /// Nothing of them is kept; what the failed ones left stays until the states go.
    pub(super) fn abort_all(&self) {
        let taken: Vec<TakenDownload> = {
            let mut states = crate::panic::lock(&self.states);
            let running: Vec<DownloadKey> = states.iter().filter(|(_, state)| matches!(state, State::Running { .. })).map(|(key, _)| key.clone()).collect();
            running.iter().filter_map(|key| take_running(&mut states, key)).collect()
        };
        taken.into_iter().for_each(TakenDownload::abort);
    }

    /// The download `run` writes into `file` from now on; how far it was written, raised
    /// by the downloader as bytes come. A download that runs no more keeps nothing of it.
    pub(super) fn begin_writing(&self, run: &DownloadRun, file: Arc<File>) -> Arc<AtomicU64> {
        let mut states = crate::panic::lock(&self.states);
        match states.get_mut(&run.key) {
            Some(State::Running { number, written_up_to, file: writes_into, .. }) if *number == run.number => {
                *writes_into = Some(file);
                Arc::clone(written_up_to)
            }
            // Abandoned or aborted meanwhile: nobody reads what it writes.
            _ => Arc::default(),
        }
    }

    /// The file and the watermark of the running download of `key`: a read of a range it
    /// has written already is answered from it. Called by read.
    pub(super) fn find_running_download(&self, key: &DownloadKey) -> Option<RunningDownloadFile> {
        match crate::panic::lock(&self.states).get(key) {
            Some(State::Running { written_up_to, file: Some(file), .. }) => Some(RunningDownloadFile { file: Arc::clone(file), written_up_to: written_up_to.load(Ordering::SeqCst) }),
            _ => None,
        }
    }

    /// The download `run` ended: its waiters are answered `outcome` (the errno of a
    /// failure); a failure keeps the unfinished file. Nothing for a download that runs no
    /// more — it was aborted, or this is said twice.
    pub(super) fn finish(&self, run: &DownloadRun, outcome: Result<(), i32>) {
        let key = &run.key;
        let waiters = {
            let mut states = crate::panic::lock(&self.states);
            if !is_running(&states, run) {
                return;
            }
            let Some(taken) = take_running(&mut states, key) else { return };
            if let (Err(_), Some(file)) = (outcome, taken.file) {
                keep_unfinished_file(&mut states, key, file);
            }
            taken.waiters
        };
        if outcome.is_ok() {
            // Downloaded: what was read of it as ranges counts no more.
            crate::panic::lock(&self.bytes_read_as_ranges).remove(&key.item);
        }
        for waiter in waiters {
            let _ = waiter.send(outcome);
        }
    }

    /// The download `run` was dropped without an end — a panic, an abort of its task from
    /// outside the states: its waiters get `EIO`, and nothing of it is kept. What a guard
    /// the task holds calls when it is dropped, whatever dropped it: for a download that
    /// ended or was aborted here it changes nothing.
    pub(super) fn abandon(&self, run: &DownloadRun) {
        let taken = {
            let mut states = crate::panic::lock(&self.states);
            if !is_running(&states, run) {
                return;
            }
            take_running(&mut states, &run.key)
        };
        for waiter in taken.into_iter().flat_map(|taken| taken.waiters) {
            let _ = waiter.send(Err(libc::EIO));
        }
    }

    /// Whether a download of `item` runs, of any content: the state attribute, free-up.
    pub(super) fn is_downloading(&self, item: LocalItemId) -> bool {
        crate::panic::lock(&self.states).iter().any(|(key, state)| key.item == item && matches!(state, State::Running { .. }))
    }

    /// `bytes` more of `item` were read as ranges; whether that reaches the threshold at
    /// which the file is downloaded whole (`download_states::reaches_download_threshold`).
    pub(super) fn count_bytes_read(&self, item: LocalItemId, bytes: u64, file_size: u64) -> bool {
        let mut bytes_read_as_ranges = crate::panic::lock(&self.bytes_read_as_ranges);
        let bytes_read = bytes_read_as_ranges.entry(item).or_default();
        *bytes_read += bytes;
        reaches_download_threshold(*bytes_read, self.download_after, file_size)
    }

    /// Nothing is kept of `item` any more: its row is gone, or it shows another content.
    /// Called by the steps towards the folder that remove an item or take OneDrive's
    /// content (what "the bytes-read count never forgetting a file" lacked: design §14).
    pub(super) fn forget_item(&self, item: LocalItemId) {
        // The unfinished files of every content of it; a download of it that runs is left
        // to its landing, which finds the row changed.
        crate::panic::lock(&self.states).retain(|key, state| key.item != item || matches!(state, State::Running { .. }));
        crate::panic::lock(&self.bytes_read_as_ranges).remove(&item);
    }

    /// Every waiter gets `EIO`: the mount stops.
    pub(super) fn fail_waiters(&self) {
        let waiters: Vec<Waiter> = {
            let mut states = crate::panic::lock(&self.states);
            let running = states.values_mut().filter_map(|state| match state {
                State::Running { waiters, .. } => Some(std::mem::take(waiters)),
                State::Failed { .. } => None,
            });
            running.flatten().collect()
        };
        for waiter in waiters {
            let _ = waiter.send(Err(libc::EIO));
        }
    }
}

#[cfg(test)]
mod tests;
