//! The state of each file's download, by the item and the content being downloaded.
//!
//! **State owned:** per (item, content version): a running download with who waits for
//! it, how far it was written, its file and whether that file is whole; or a failed one
//! with its unfinished file kept to go on from; or the word that OneDrive stated another
//! content in its last answer to a download of it, which the next download of it carries
//! until it is answered; the handle each running download's task is aborted by, which
//! download of its key it is, and whether anything but the plan asked for it; whether
//! the mount stopped; and how many bytes of each item were read as ranges since the last
//! download a read started. Changed by the downloader and by read alone, through the
//! functions here; no I/O.
//!
//! **Answers for:** who dedups (a download of the same
//! item and content that runs is joined, whoever asks: a read, an open for writing,
//! `Files.Hydrate`, the plan's pin); who answers "is downloading" (the state attribute,
//! free-up); from which file a read is answered while its download runs. A download of
//! another content of the same item is another download: the older one's landing finds
//! the row changed and drops its file.
//!
//! **Another content, remembered.** That OneDrive stated another content than the key's
//! is a fact from one answer, and is void when OneDrive has answered again, not when it
//! is asked: the next download of the key carries it while it runs, until its source
//! accepts an answer as of the key's content
//! ([`forget_another_content`](ItemDownloadStates::forget_another_content)); a download
//! that ends with no such answer leaves it as it was, and where it failed, the errno it
//! failed with is kept with it ([`AnotherContentFact`]): the failure that stands
//! for the key, which whoever tells of downloads tells once; and `forget_item` forgets
//! it. It
//! answers no request by itself: a read of a remembered key asks for the whole download,
//! or joins the one that runs and has no answer yet, which asks OneDrive anew
//! (`item_kernel_requests`). Only an answer that states a version or a size
//! that disagrees is remembered so; a file OneDrive does not have and a size it did not
//! say are failures like any other ([`finish`](ItemDownloadStates::finish)). The latest
//! [`ANOTHER_CONTENTS_REMEMBERED`] are kept.
//!
//! **Tested without the rest:**
// test: a_second_asking_for_the_same_item_and_content_joins_the_running_download
// test: an_asking_for_another_content_of_the_item_starts_another_download
// test: every_waiter_is_answered_when_the_download_ends_however_it_ends
// test: a_failed_download_keeps_its_unfinished_file_for_the_next_asking_of_that_content
// test: the_bytes_read_as_ranges_reach_the_threshold_once_and_are_forgotten_with_the_item
// test: every_download_is_aborted_and_its_waiters_answered_when_the_mount_stops
// test: a_download_only_the_plan_asked_for_is_aborted_by_its_cancel_and_keeps_its_unfinished_file
// test: a_download_anything_but_the_plan_asked_for_is_not_aborted_by_the_plans_cancel
// test: an_end_told_by_an_older_download_of_the_key_changes_nothing_of_the_newer
// test: a_download_whose_file_is_whole_keeps_no_unfinished_file_however_it_ends
// test: a_download_onedrive_answered_with_another_content_is_remembered_until_the_next_download_of_the_key_is_answered
// test: a_download_asked_for_after_the_stop_is_not_started
// test: a_download_of_the_plans_is_told_when_anything_else_asks_for_it
// test: the_count_of_bytes_read_as_ranges_starts_over_once_it_reached_the_threshold
// test: no_more_than_the_latest_contents_onedrive_answered_with_another_are_remembered
// test: the_failure_of_a_download_onedrive_did_not_answer_is_kept_with_what_is_remembered_of_its_key
// test: a_handle_kept_after_the_abort_aborts_its_own_task_and_leaves_the_next_download_of_the_key
// test: the_unfinished_files_are_those_of_the_running_downloads_that_are_not_whole_and_of_the_failed_ones

use std::collections::HashMap;
use std::fs::File;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::oneshot;

use konedrive_tree::{ContentVersion, ItemVersions, LocalItemId};


/// How many downloads that failed keep their unfinished file, to be gone on from: the
/// latest ones.
const UNFINISHED_FILES_KEPT: usize = 16;

/// How many contents OneDrive stated another one for are remembered: the latest ones. One
/// goes with the first answer the next download of its key accepts and with its item;
/// what stays is of handles nobody reads through any more, and of rows a program wrote
/// since. A read of a content that is remembered no more fetches ranges, as of any
/// content, until the next whole download tells again.
pub(super) const ANOTHER_CONTENTS_REMEMBERED: usize = 256;

/// What a download is of.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DownloadKey {
    pub(crate) item: LocalItemId,
    pub(crate) content: ContentVersion,
}

/// Who asks for a download, with what only that asker has: the one place that says a
/// download is the plan's, and from which versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DownloadAsker {
    /// A read: at the threshold of bytes read as ranges, with no waiter; and of a content
    /// OneDrive stated another one for in its answer to the last download, with one.
    Read,
    /// An open for writing, or a truncation, of a file that is not on disk.
    OpenForWriting,
    /// `Files.Hydrate`.
    FilesInterface,
    /// The plan's step for a pinned file, with the versions it was planned from: its
    /// landing commits as the step (`ItemStore::commit_download_content`), where every
    /// other asker's asks the content alone. The one asker whose asking is taken back
    /// (`ItemDownloadStates::abort_if_plan_alone_asked`).
    PlanPin { planned_from: ItemVersions },
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
    /// The mount stopped: nothing is started, and the waiter was answered `EIO`.
    Stopped,
}

enum State {
    Running {
        /// Which download it is (`DownloadRun`).
        number: u64,
        waiters: Vec<Waiter>,
        written_up_to: Arc<AtomicU64>,
        file: Option<Arc<File>>,
        /// Whether `file` holds the whole content: it went to its landing, where it may
        /// be given a name, and is never kept to be gone on from.
        is_file_whole: bool,
        abort: Option<tokio::task::AbortHandle>,
        /// Whether anything but the plan's pin asked for it, with a waiter or without;
        /// the download's task waits on it while it waits for the plan's slot.
        is_asked_beyond_plan: tokio::sync::watch::Sender<bool>,
        /// What was remembered of the key when this download started, while its source
        /// has accepted no answer yet: the key is remembered as before
        /// (`State::AnotherContent`) until OneDrive has answered again.
        another_content: Option<AnotherContentFact>,
    },
    Failed { unfinished_file: Arc<File>, failure_number: u64 },
    /// OneDrive stated another content than the key's in the last answer it gave to a
    /// download of the key: a read through a handle opened as this content asks for the
    /// whole download, until the next download of the key accepts an answer or the item
    /// is forgotten.
    AnotherContent { fact: AnotherContentFact },
}

/// The fact that OneDrive stated another content than a key's, as it is remembered of
/// the key: from which answer, and what became of the askings since.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AnotherContentFact {
    /// The download of the key OneDrive answered so, the last it gave an answer about
    /// the content to: the latest [`ANOTHER_CONTENTS_REMEMBERED`] are kept by it.
    number: u64,
    /// The errno the last download of the key since then ended with, having got no
    /// answer of OneDrive about the content (it was offline, signed out, held back; the
    /// item was not there): the failure that stands for the key. `None` when OneDrive
    /// answered the last download of the key, with another content.
    pub(super) failed_asking: Option<i32>,
}

/// The downloads by what each is of, and whether the mount stopped.
#[derive(Default)]
struct States {
    by_key: HashMap<DownloadKey, State>,
    /// No download is started any more.
    is_stopped: bool,
}

/// A running download taken out of the states: who waited for it, the file it wrote
/// into, and what its task is aborted by.
struct TakenDownload {
    waiters: Vec<Waiter>,
    /// The file it wrote into, when that file is not whole: what may be gone on from.
    unfinished_file: Option<Arc<File>>,
    abort: Option<tokio::task::AbortHandle>,
    /// The number of the download OneDrive answered with another content of the key,
    /// when this one carried that word to its end: no answer of its own was accepted.
    another_content: Option<AnotherContentFact>,
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
        Some(State::Running { waiters, file, is_file_whole, abort, another_content, .. }) => Some(TakenDownload { waiters, unfinished_file: file.filter(|_| !is_file_whole), abort, another_content }),
        Some(State::Failed { .. } | State::AnotherContent { .. }) | None => None,
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

/// A download of `key` failed with `failed_asking`, or was given up (`None`), and `taken`
/// is what it left: the word that OneDrive stated another content, when the download
/// carried it to its end — no answer was accepted, so its file holds nothing to go on
/// from —, is remembered as before, with the errno of this failure where it failed;
/// else its unfinished file is kept.
fn keep_left_by_download(states: &mut HashMap<DownloadKey, State>, key: &DownloadKey, taken: &TakenDownload, failed_asking: Option<i32>) {
    match (taken.another_content, &taken.unfinished_file) {
        // A download that was given up failed with nothing: the failure before it stands.
        (Some(fact), _) => remember_another_content(states, key, AnotherContentFact { failed_asking: failed_asking.or(fact.failed_asking), ..fact }),
        (None, Some(file)) => keep_unfinished_file(states, key, Arc::clone(file)),
        (None, None) => {}
    }
}

/// The key and the failure number of every `Failed` state.
fn list_failure_numbers(states: &HashMap<DownloadKey, State>) -> impl Iterator<Item = (&DownloadKey, u64)> {
    states.iter().filter_map(|(key, state)| match state {
        State::Failed { failure_number, .. } => Some((key, *failure_number)),
        State::Running { .. } | State::AnotherContent { .. } => None,
    })
}

/// OneDrive stated another content than `key`'s, as `fact` says: remembered, and
/// the oldest such word beyond [`ANOTHER_CONTENTS_REMEMBERED`] is forgotten.
fn remember_another_content(states: &mut HashMap<DownloadKey, State>, key: &DownloadKey, fact: AnotherContentFact) {
    states.insert(key.clone(), State::AnotherContent { fact });
    if list_another_content_numbers(states).count() <= ANOTHER_CONTENTS_REMEMBERED {
        return;
    }
    let Some(oldest) = list_another_content_numbers(states).min_by_key(|(_, number)| *number).map(|(key, _)| key.clone()) else { return };
    states.remove(&oldest);
}

/// The key of every `AnotherContent` state, and the number of the download that was
/// answered so.
fn list_another_content_numbers(states: &HashMap<DownloadKey, State>) -> impl Iterator<Item = (&DownloadKey, u64)> {
    states.iter().filter_map(|(key, state)| match state {
        State::AnotherContent { fact } => Some((key, fact.number)),
        State::Running { .. } | State::Failed { .. } => None,
    })
}

/// The download states of one mount.
pub(crate) struct ItemDownloadStates {
    states: std::sync::Mutex<States>,
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
    /// a waiter or without, is not aborted for the plan, and one the plan started is told
    /// of it ([`wait_for_asker_beyond_plan`](Self::wait_for_asker_beyond_plan)). After
    /// the stop nothing is started: `waiter` is answered `EIO`. A download that is started
    /// asks OneDrive anew: what was remembered of the key from the answer to its last
    /// download ([`is_another_content`](Self::is_another_content)) stays remembered
    /// while this one runs, until an answer to it is accepted
    /// ([`forget_another_content`](Self::forget_another_content)).
    /// Called by `ItemDownloader::download` alone.
    pub(super) fn start_or_join(&self, key: &DownloadKey, asker: &DownloadAsker, waiter: Option<Waiter>) -> ItemDownloadStart {
        let is_asker_beyond_plan = !matches!(asker, DownloadAsker::PlanPin { .. });
        let mut states = crate::panic::lock(&self.states);
        if states.is_stopped {
            drop(states);
            if let Some(waiter) = waiter {
                let _ = waiter.send(Err(libc::EIO));
            }
            return ItemDownloadStart::Stopped;
        }
        if let Some(State::Running { waiters, is_asked_beyond_plan, .. }) = states.by_key.get_mut(key) {
            waiters.extend(waiter);
            if is_asker_beyond_plan {
                is_asked_beyond_plan.send_replace(true);
            }
            return ItemDownloadStart::Joined;
        }
        let number = self.last_run_number.fetch_add(1, Ordering::SeqCst) + 1;
        let (unfinished_file, another_content) = match states.by_key.remove(key) {
            Some(State::Failed { unfinished_file, .. }) => (Some(unfinished_file), None),
            Some(State::AnotherContent { fact }) => (None, Some(fact)),
            Some(State::Running { .. }) | None => (None, None),
        };
        let running = State::Running {
            number,
            waiters: waiter.into_iter().collect(),
            written_up_to: Arc::default(),
            file: None,
            is_file_whole: false,
            abort: None,
            is_asked_beyond_plan: tokio::sync::watch::Sender::new(is_asker_beyond_plan),
            another_content,
        };
        states.by_key.insert(key.clone(), running);
        ItemDownloadStart::Started { run: DownloadRun { key: key.clone(), number }, unfinished_file }
    }

    /// Whether anything but the plan's pin has asked for the download `run`, which runs:
    /// a program waits for it.
    pub(super) fn is_asked_beyond_plan(&self, run: &DownloadRun) -> bool {
        matches!(crate::panic::lock(&self.states).by_key.get(&run.key), Some(State::Running { number, is_asked_beyond_plan, .. }) if *number == run.number && *is_asked_beyond_plan.borrow())
    }

    /// Waits until anything but the plan's pin asked for the download `run`: at once for
    /// one a read, an open for writing or `Files.Hydrate` started or has joined. For ever
    /// for a download that runs no more: its task is aborted, or ends by itself. Awaited
    /// by the downloader's task while it waits for a slot of the plan's class, which a
    /// pause holds back: what a program waits for is given an open's slot.
    pub(super) async fn wait_for_asker_beyond_plan(&self, run: &DownloadRun) {
        let asked = match crate::panic::lock(&self.states).by_key.get(&run.key) {
            Some(State::Running { number, is_asked_beyond_plan, .. }) if *number == run.number => Some(is_asked_beyond_plan.subscribe()),
            _ => None,
        };
        let is_asked = match asked {
            Some(mut asked) => asked.wait_for(|is_asked| *is_asked).await.is_ok(),
            None => false,
        };
        if !is_asked {
            std::future::pending::<()>().await;
        }
    }

    /// The task of the download `run` is aborted by `abort`. Called by the downloader
    /// right after it spawned the task.
    ///
    /// When `run` runs no more, the task is aborted at once: it was cancelled, or the
    /// mount stopped, between its asking and this call — or it has ended already, and the
    /// abort does nothing. A newer download of the same key is not touched.
    pub(super) fn keep_abort_handle(&self, run: &DownloadRun, abort: tokio::task::AbortHandle) {
        let mut states = crate::panic::lock(&self.states);
        match states.by_key.get_mut(&run.key) {
            Some(State::Running { number, abort: kept, .. }) if *number == run.number => *kept = Some(abort),
            _ => abort.abort(),
        }
    }

    /// The plan gives the download of `key` up: aborted when the plan's pin alone asked
    /// for it — nothing else started or joined it, with a waiter or without — its waiters
    /// answered `EIO` and its unfinished file kept as after a failure, or the key
    /// remembered as before where no answer was accepted yet. Whether it was
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
            if !matches!(states.by_key.get(key), Some(State::Running { is_asked_beyond_plan, .. }) if !*is_asked_beyond_plan.borrow()) {
                return false;
            }
            let Some(taken) = take_running(&mut states.by_key, key) else { return false };
            keep_left_by_download(&mut states.by_key, key, &taken, None);
            taken
        };
        taken.abort();
        true
    }

    /// The mount stops: every running download is aborted and its waiters answered `EIO`,
    /// and from here on no download is started
    /// ([`start_or_join`](Self::start_or_join) answers `Stopped`) — under the one lock, so
    /// that no asking comes between the two. Nothing of the aborted ones is kept; what
    /// the failed ones left stays until the states go.
    pub(super) fn stop(&self) {
        let taken: Vec<TakenDownload> = {
            let mut states = crate::panic::lock(&self.states);
            states.is_stopped = true;
            let running: Vec<DownloadKey> = states.by_key.iter().filter(|(_, state)| matches!(state, State::Running { .. })).map(|(key, _)| key.clone()).collect();
            running.iter().filter_map(|key| take_running(&mut states.by_key, key)).collect()
        };
        taken.into_iter().for_each(TakenDownload::abort);
    }

    /// The download `run` writes into `file` from now on; how far it was written, raised
    /// by the downloader as bytes come. A download that runs no more keeps nothing of it.
    pub(super) fn begin_writing(&self, run: &DownloadRun, file: Arc<File>) -> Arc<AtomicU64> {
        let mut states = crate::panic::lock(&self.states);
        match states.by_key.get_mut(&run.key) {
            Some(State::Running { number, written_up_to, file: writes_into, .. }) if *number == run.number => {
                *writes_into = Some(file);
                Arc::clone(written_up_to)
            }
            // Abandoned or aborted meanwhile: nobody reads what it writes.
            _ => Arc::default(),
        }
    }

    /// The file of the download `run` holds the whole content, checked: it goes to its
    /// landing, where it may be given a name — and a file that had a name and lost it can
    /// never be named again. However the download ends from here on, its file is not
    /// kept to be gone on from: the next download of the key starts with a new one. A
    /// read is still answered from it while the download runs.
    pub(super) fn end_writing(&self, run: &DownloadRun) {
        if let Some(State::Running { number, is_file_whole, .. }) = crate::panic::lock(&self.states).by_key.get_mut(&run.key) {
            if *number == run.number {
                *is_file_whole = true;
            }
        }
    }

    /// The file and the watermark of the running download of `key`: a read of a range it
    /// has written already is answered from it. Called by read.
    ///
    /// The watermark is raised by the downloader's source alone, and only for bytes of
    /// an answer it has told to be of the key's content
    /// (`item_downloader::download_source`): what is below it is of that content.
    pub(super) fn find_running_download(&self, key: &DownloadKey) -> Option<RunningDownloadFile> {
        match crate::panic::lock(&self.states).by_key.get(key) {
            Some(State::Running { written_up_to, file: Some(file), .. }) => Some(RunningDownloadFile { file: Arc::clone(file), written_up_to: written_up_to.load(Ordering::SeqCst) }),
            _ => None,
        }
    }

    /// The download `run` ended: its waiters are answered `outcome` (the errno of a
    /// failure); a failure keeps the unfinished file, unless it is whole
    /// ([`end_writing`](Self::end_writing)) — or, where the download accepted no answer
    /// of OneDrive, what was remembered of the key before it
    /// ([`is_another_content`](Self::is_another_content)), with this failure's errno as
    /// the failure that stands for the key
    /// ([`find_another_content`](Self::find_another_content)). Nothing for a download
    /// that runs no more — it was aborted, or this is said twice.
    pub(super) fn finish(&self, run: &DownloadRun, outcome: Result<(), i32>) {
        let key = &run.key;
        let waiters = {
            let mut states = crate::panic::lock(&self.states);
            if !is_running(&states.by_key, run) {
                return;
            }
            let Some(taken) = take_running(&mut states.by_key, key) else { return };
            if let Err(errno) = outcome {
                keep_left_by_download(&mut states.by_key, key, &taken, Some(errno));
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

    /// The download `run` ended because OneDrive stated another content of the item than
    /// the key's in an answer to it: its waiters are answered `ESTALE`, nothing of its
    /// file is kept, and the key is remembered as such
    /// ([`is_another_content`](Self::is_another_content)), among the latest
    /// [`ANOTHER_CONTENTS_REMEMBERED`]. Nothing for a download that runs no more. A
    /// download OneDrive answered with no such file, or with nothing to tell the content
    /// by, did not end so: it failed ([`finish`](Self::finish)).
    pub(super) fn finish_as_another_content(&self, run: &DownloadRun) {
        let waiters = {
            let mut states = crate::panic::lock(&self.states);
            if !is_running(&states.by_key, run) {
                return;
            }
            let Some(taken) = take_running(&mut states.by_key, &run.key) else { return };
            remember_another_content(&mut states.by_key, &run.key, AnotherContentFact { number: run.number, failed_asking: None });
            taken.waiters
        };
        for waiter in waiters {
            let _ = waiter.send(Err(libc::ESTALE));
        }
    }

    /// Whether OneDrive stated another content than the key's in the last answer it
    /// gave to a download of `key`, and has given none since: also while the next
    /// download of the key runs and its source has accepted no answer. A read through a
    /// handle opened as that content asks for the whole download then, or joins the one
    /// that runs, which asks OneDrive anew; what reads it decides what else is asked
    /// (`item_kernel_requests`). Forgotten when a download of the key accepts an answer
    /// ([`forget_another_content`](Self::forget_another_content)), and with the item
    /// ([`forget_item`](Self::forget_item)); a key beyond the latest
    /// [`ANOTHER_CONTENTS_REMEMBERED`] is remembered no more. Called by read, and by the
    /// downloader for what it tells of a download.
    pub(crate) fn is_another_content(&self, key: &DownloadKey) -> bool {
        self.find_another_content(key).is_some()
    }

    /// What is remembered of `key` when OneDrive stated another content for it
    /// ([`is_another_content`](Self::is_another_content)): with the failure that stands
    /// for the key since, when its last download got no answer of OneDrive. Called by
    /// the downloader, before it tells the states of a download's end, for whether
    /// that end is news (`item_downloader::decide_end_news`).
    pub(super) fn find_another_content(&self, key: &DownloadKey) -> Option<AnotherContentFact> {
        match crate::panic::lock(&self.states).by_key.get(key) {
            Some(State::AnotherContent { fact }) => Some(*fact),
            Some(State::Running { another_content, .. }) => *another_content,
            Some(State::Failed { .. }) | None => None,
        }
    }

    /// OneDrive answered the download `run` with the content of its key: what was
    /// remembered of the key from the answer to the download before it
    /// ([`is_another_content`](Self::is_another_content)) is void from here on. Called
    /// by the download's source for every answer it accepts
    /// (`item_downloader::download_source`). Nothing for a download that runs no more,
    /// whatever runs for the key since.
    pub(super) fn forget_another_content(&self, run: &DownloadRun) {
        if let Some(State::Running { number, another_content, .. }) = crate::panic::lock(&self.states).by_key.get_mut(&run.key) {
            if *number == run.number {
                *another_content = None;
            }
        }
    }

    /// The download `run` was dropped without an end — a panic, an abort of its task from
    /// outside the states: its waiters get `EIO`, and nothing of it is kept; the key is
    /// remembered as before it where it accepted no answer. What a guard
    /// the task holds calls when it is dropped, whatever dropped it: for a download that
    /// ended or was aborted here it changes nothing.
    pub(super) fn abandon(&self, run: &DownloadRun) {
        let taken = {
            let mut states = crate::panic::lock(&self.states);
            if !is_running(&states.by_key, run) {
                return;
            }
            let taken = take_running(&mut states.by_key, &run.key);
            if let Some(fact) = taken.as_ref().and_then(|taken| taken.another_content) {
                remember_another_content(&mut states.by_key, &run.key, fact);
            }
            taken
        };
        for waiter in taken.into_iter().flat_map(|taken| taken.waiters) {
            let _ = waiter.send(Err(libc::EIO));
        }
    }

    /// The files of the downloads that began and did not land, each with its item: the
    /// file a running download writes into, unless it is whole already and on its way to
    /// its landing ([`end_writing`](Self::end_writing)), and the unfinished file kept of
    /// a download that failed, to be gone on from. None has a name in the backing
    /// directory: they are held here alone. In no order. For the test support of the
    /// daemon (`ItemFilesInterface::list_unfinished_downloads`).
    #[cfg(any(test, feature = "fault-injection"))]
    pub(super) fn list_unfinished_files(&self) -> Vec<(LocalItemId, Arc<File>)> {
        let states = crate::panic::lock(&self.states);
        states
            .by_key
            .iter()
            .filter_map(|(key, state)| match state {
                State::Running { file: Some(file), is_file_whole: false, .. } => Some((key.item, Arc::clone(file))),
                State::Running { file: None, .. } | State::Running { is_file_whole: true, .. } => None,
                State::Failed { unfinished_file, .. } => Some((key.item, Arc::clone(unfinished_file))),
                State::AnotherContent { .. } => None,
            })
            .collect()
    }

    /// Whether a download of `item` runs, of any content: the state attribute, free-up.
    pub(super) fn is_downloading(&self, item: LocalItemId) -> bool {
        crate::panic::lock(&self.states).by_key.iter().any(|(key, state)| key.item == item && matches!(state, State::Running { .. }))
    }

    /// `bytes` more of `item` were read as ranges; whether that reaches the threshold at
    /// which the file is downloaded whole ([`reaches_download_threshold`]).
    /// The count starts over once it answered yes: a download that ends without landing
    /// is not started again by the next read, but by as many bytes read again.
    pub(super) fn count_bytes_read(&self, item: LocalItemId, bytes: u64, file_size: u64) -> bool {
        let mut bytes_read_as_ranges = crate::panic::lock(&self.bytes_read_as_ranges);
        let bytes_read = bytes_read_as_ranges.entry(item).or_default();
        *bytes_read += bytes;
        let is_reached = reaches_download_threshold(*bytes_read, self.download_after, file_size);
        if is_reached {
            bytes_read_as_ranges.remove(&item);
        }
        is_reached
    }

    /// Nothing is kept of `item` any more: its row is gone, or it shows another content.
    /// Called by the steps towards the folder that remove an item or take OneDrive's
    /// content: no count of bytes read outlives its item.
    pub(crate) fn forget_item(&self, item: LocalItemId) {
        // The unfinished files of every content of it, and what was remembered of each; a
        // download of it that runs is left to its landing, which finds the row changed.
        crate::panic::lock(&self.states).by_key.retain(|key, state| match state {
            State::Running { another_content, .. } => {
                if key.item == item {
                    *another_content = None;
                }
                true
            }
            State::Failed { .. } | State::AnotherContent { .. } => key.item != item,
        });
        crate::panic::lock(&self.bytes_read_as_ranges).remove(&item);
    }

    /// How many wait for the running download of `key`: for a test that must know an
    /// asking has joined before it goes on.
    #[cfg(test)]
    pub(super) fn count_waiters(&self, key: &DownloadKey) -> usize {
        match crate::panic::lock(&self.states).by_key.get(key) {
            Some(State::Running { waiters, .. }) => waiters.len(),
            _ => 0,
        }
    }
}

/// A caller in the daemon that waits for a download to end: told that it ended well, or the
/// errno it failed with.
pub(super) type Waiter = oneshot::Sender<Result<(), i32>>;

/// The file a running download writes into, and how far it is written in order.
#[derive(Debug, Clone)]
pub(super) struct RunningDownloadFile {
    pub(super) file: Arc<File>,
    pub(super) written_up_to: u64,
}

/// Whether `bytes_read` bytes of a file `file_size` long, read as ranges, are enough for the
/// whole file to be downloaded. A file smaller than `download_after` is downloaded once all
/// of it was read: it would be fetched again at every read otherwise.
pub(super) fn reaches_download_threshold(bytes_read: u64, download_after: u64, file_size: u64) -> bool {
    bytes_read >= download_after.min(file_size)
}

#[cfg(test)]
mod tests;
