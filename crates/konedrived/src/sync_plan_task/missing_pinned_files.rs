//! Whether a pinned file may be missing its content: what the plan's task asks before it
//! has a plan's reading scan all below the pinned directories
//! (`ItemReader::list_pinned_files_to_download`, the one reader of a plan that no index
//! bounds).
//!
//! **State owned:** one flag, for the life of the task's run. Set when the task starts
//! and by whatever can leave a pinned file with no content on disk
//! ([`can_cause_leave_pinned_file_missing`], [`can_step_leave_pinned_file_missing`]);
//! cleared when a reading that asked the reader answered that none is missing. Changed by
//! the plan's loop alone, which also makes the plans: what sets it is taken before a plan
//! is made, and a cause that comes while a plan is read is taken by the next round.
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

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};

use konedrive_tree::{FolderStep, SyncStep};

use super::plan_input::PinnedFilesReading;
use crate::sync_parts::step_outcome::StepOutcome;
use crate::sync_parts::sync_plan_wake::WakeCause;

/// Whether a pinned file may be missing its content, for one folder's plan's task.
pub struct MissingPinnedFiles {
    may_be_missing: AtomicBool,
}

/// Whether what woke the plan can have left a pinned file with no content on disk: a page
/// of a listing written or a listing ended (an item listed, moved or changed), the user's
/// answer (a pin set), and a refresh the user asked for, which looks at everything again.
/// A local version written by the mount cannot by itself: what it leaves is unsent, and
/// the step that settles it is asked ([`can_step_leave_pinned_file_missing`]). Pure.
pub fn can_cause_leave_pinned_file_missing(cause: WakeCause) -> bool {
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
pub fn can_step_leave_pinned_file_missing(step: &SyncStep, outcome: &StepOutcome) -> bool {
    match (step, outcome) {
        (SyncStep::Folder(FolderStep::DownloadContent { .. }), StepOutcome::Written { .. }) => false,
        (SyncStep::Folder(FolderStep::DownloadContent { .. }), StepOutcome::RowsChanged(_) | StepOutcome::Refused(_) | StepOutcome::NotRun(_) | StepOutcome::Failed(_)) => true,
        (SyncStep::Folder(_) | SyncStep::OneDrive(_) | SyncStep::Row(_), StepOutcome::Written { .. }) => true,
        (SyncStep::Folder(_) | SyncStep::OneDrive(_) | SyncStep::Row(_), StepOutcome::RowsChanged(_) | StepOutcome::Refused(_) | StepOutcome::NotRun(_) | StepOutcome::Failed(_)) => false,
    }
}

impl MissingPinnedFiles {
    /// The task started: nothing is known, and the reader is asked.
    pub fn new() -> Self {
        MissingPinnedFiles { may_be_missing: AtomicBool::new(true) }
    }

    /// Whether the next plan's reading asks the reader of pinned files.
    pub fn is_reader_asked(&self) -> bool {
        self.may_be_missing.load(Ordering::SeqCst)
    }

    /// The causes a round was woken by, taken before its plan is made.
    pub fn note_wake_causes(&self, causes: &BTreeSet<WakeCause>) {
        if causes.iter().any(|cause| can_cause_leave_pinned_file_missing(*cause)) {
            self.may_be_missing.store(true, Ordering::SeqCst);
        }
    }

    /// The steps that ended, as they are settled.
    pub fn note_ended_steps(&self, ended: &[(SyncStep, StepOutcome)]) {
        if ended.iter().any(|(step, outcome)| can_step_leave_pinned_file_missing(step, outcome)) {
            self.may_be_missing.store(true, Ordering::SeqCst);
        }
    }

    /// What a plan's reading answered of the pinned files: one that asked the reader and
    /// found none missing ends the asking.
    pub fn note_reading(&self, reading: PinnedFilesReading) {
        match reading {
            PinnedFilesReading::NoneMissing => self.may_be_missing.store(false, Ordering::SeqCst),
            PinnedFilesReading::NotAsked | PinnedFilesReading::SomeMissing => {}
        }
    }
}

impl Default for MissingPinnedFiles {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
