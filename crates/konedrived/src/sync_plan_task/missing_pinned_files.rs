//! Whether a pinned file may be missing its content: what the plan's task asks before it
//! has a plan's reading scan all below the pinned directories
//! (`ItemReader::list_pinned_files_to_download`, the one reader of a plan that no index
//! bounds).
//!
//! **State owned:** two flags, for the life of the task's run. The first is set when the
//! task starts and by whatever can leave a pinned file with no content on disk
//! (`can_cause_leave_pinned_file_missing`, `can_step_leave_pinned_file_missing`);
//! cleared when a reading that asked the reader answered that none is missing. Changed by
//! the plan's loop alone, which also makes the plans: what sets it is taken before a plan
//! is made, and a cause that comes while a plan is read is taken by the next round. The
//! second is whether a download of pinned files is under way as the journal was told: set
//! by the first reading that finds some missing, cleared by the one that finds none
//! (`decide_download_news`), so that the journal has one line for each and none per file.
//!
//! While it is set the reader is asked with every plan — a plan that left the pinned
//! files out would drop the downloads the last one planned —, so the scans go on until
//! every pinned file has its content, and stop then: a plan made for a change saved
//! here, for an upload that ended or for a time reached scans nothing.
//!
//! What it does not see: a pinned file whose content goes off the disk by a way that
//! wakes the plan with none of the causes below and ends no step. Such a file is found at
//! the next page of a listing, which sets the flag.
//!
//! **Tested without the rest:**
// test: the_reader_is_asked_from_the_start_until_a_reading_answers_that_none_is_missing
// test: a_reading_that_was_not_asked_or_found_some_missing_changes_nothing
// test: each_cause_that_can_leave_a_pinned_file_missing_has_the_reader_asked_again
// test: each_end_of_a_step_that_can_leave_a_pinned_file_missing_has_the_reader_asked_again
// test: the_download_of_pinned_files_begins_and_ends_once_each

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use konedrive_tree::{FolderStep, PinnedDownloadTotal, SyncStep};

use super::plan_input::PinnedFilesReading;
use crate::sync_parts::step_outcome::StepOutcome;
use crate::sync_parts::sync_plan_wake::WakeCause;

/// Whether a pinned file may be missing its content, for one folder's plan's task.
pub(crate) struct MissingPinnedFiles {
    may_be_missing: AtomicBool,
    is_download_under_way: AtomicBool,
}

/// What a plan's reading says of the download of the pinned files that the one before
/// did not: what the journal is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PinnedDownloadNews {
    /// Some are missing and none was before: so many are to be downloaded.
    Began(PinnedDownloadTotal),
    /// None is missing any more.
    Ended,
    Nothing,
}

/// What `reading` says anew, when a download of pinned files was under way before it
/// (`was_under_way`) or was not: it begins with the first reading that finds some missing
/// and ends with the one that finds none; a reading that did not ask says nothing. Pure.
pub(crate) fn decide_download_news(was_under_way: bool, reading: PinnedFilesReading) -> PinnedDownloadNews {
    match (was_under_way, reading) {
        (false, PinnedFilesReading::SomeMissing(total)) => PinnedDownloadNews::Began(total),
        (true, PinnedFilesReading::NoneMissing) => PinnedDownloadNews::Ended,
        (true, PinnedFilesReading::SomeMissing(_)) | (false, PinnedFilesReading::NoneMissing) | (_, PinnedFilesReading::NotAsked) => PinnedDownloadNews::Nothing,
    }
}

/// Whether what woke the plan can have left a pinned file with no content on disk: a page
/// of a listing written or a listing ended (an item listed, moved or changed), the user's
/// answer (a pin set), and a refresh the user asked for, which looks at everything again.
/// A local version written by the mount cannot by itself: what it leaves is unsent, and
/// the step that settles it is asked ([`can_step_leave_pinned_file_missing`]). Pure.
pub(crate) fn can_cause_leave_pinned_file_missing(cause: WakeCause) -> bool {
    match cause {
        WakeCause::ListingPageWritten | WakeCause::ListingEnded | WakeCause::UserAnswered | WakeCause::RefreshAsked => true,
        WakeCause::LocalVersionWritten | WakeCause::StepFinished | WakeCause::QuotaRead | WakeCause::SendingHoldChanged | WakeCause::RequestRecordSettled | WakeCause::NextCleaningTryChanged => false,
    }
}

/// Whether `step` ended as `outcome` can have left a pinned file with no content on disk:
/// a download that ended otherwise than landed; and any other step that was written — an
/// item shown, a content of OneDrive's taken (the copy goes), an item moved into a pinned
/// directory here or its move confirmed, two rows agreeing: after it the item may agree
/// on everything and be given to no plan but by the reader of pinned files. Pure.
pub(crate) fn can_step_leave_pinned_file_missing(step: &SyncStep, outcome: &StepOutcome) -> bool {
    match (step, outcome) {
        (SyncStep::Folder(FolderStep::DownloadContent { .. }), StepOutcome::Written { .. }) => false,
        (SyncStep::Folder(FolderStep::DownloadContent { .. }), StepOutcome::RowsChanged(_) | StepOutcome::Refused(_) | StepOutcome::NotRun(_) | StepOutcome::Failed(_)) => true,
        (SyncStep::Folder(_) | SyncStep::OneDrive(_) | SyncStep::Row(_), StepOutcome::Written { .. }) => true,
        (SyncStep::Folder(_) | SyncStep::OneDrive(_) | SyncStep::Row(_), StepOutcome::RowsChanged(_) | StepOutcome::Refused(_) | StepOutcome::NotRun(_) | StepOutcome::Failed(_)) => false,
    }
}

/// Says `news` of the pinned files of `folder` in the journal: one line as their download
/// begins and one as it ends, none per file.
pub(crate) fn log_download_news(news: PinnedDownloadNews, folder: &Path) {
    // TODO: wording
    match news {
        PinnedDownloadNews::Began(total) => tracing::info!("the download of the pinned files of {} began: {} file(s), {}", folder.display(), total.count, crate::status::activity::human_size(total.bytes)),
        PinnedDownloadNews::Ended => tracing::info!("every pinned file of {} is on this computer", folder.display()),
        PinnedDownloadNews::Nothing => {}
    }
}

impl MissingPinnedFiles {
    /// The task started: nothing is known, and the reader is asked.
    pub fn new() -> Self {
        MissingPinnedFiles { may_be_missing: AtomicBool::new(true), is_download_under_way: AtomicBool::new(false) }
    }

    /// Whether the next plan's reading asks the reader of pinned files.
    pub(crate) fn is_reader_asked(&self) -> bool {
        self.may_be_missing.load(Ordering::SeqCst)
    }

    /// The causes a round was woken by, taken before its plan is made.
    pub(crate) fn note_wake_causes(&self, causes: &BTreeSet<WakeCause>) {
        if causes.iter().any(|cause| can_cause_leave_pinned_file_missing(*cause)) {
            self.may_be_missing.store(true, Ordering::SeqCst);
        }
    }

    /// The steps that ended, as they are settled.
    pub(crate) fn note_ended_steps(&self, ended: &[(SyncStep, StepOutcome)]) {
        if ended.iter().any(|(step, outcome)| can_step_leave_pinned_file_missing(step, outcome)) {
            self.may_be_missing.store(true, Ordering::SeqCst);
        }
    }

    /// What a plan's reading answered of the pinned files: one that asked the reader and
    /// found none missing ends the asking. Answers what the reading says anew of their
    /// download ([`decide_download_news`]), for the journal.
    pub(crate) fn note_reading(&self, reading: PinnedFilesReading) -> PinnedDownloadNews {
        match reading {
            PinnedFilesReading::NoneMissing => self.may_be_missing.store(false, Ordering::SeqCst),
            PinnedFilesReading::NotAsked | PinnedFilesReading::SomeMissing(_) => {}
        }
        let news = decide_download_news(self.is_download_under_way.load(Ordering::SeqCst), reading);
        match news {
            PinnedDownloadNews::Began(_) => self.is_download_under_way.store(true, Ordering::SeqCst),
            PinnedDownloadNews::Ended => self.is_download_under_way.store(false, Ordering::SeqCst),
            PinnedDownloadNews::Nothing => {}
        }
        news
    }
}

impl Default for MissingPinnedFiles {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
