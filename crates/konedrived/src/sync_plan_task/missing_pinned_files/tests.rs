use std::ffi::OsString;

use konedrive_tree::{ContentPresence, ContentVersion, DriveId, ItemPlace, ItemVersion, ItemVersions, LocalItemId, OneDriveItemId, OneDriveItemReference, OneDriveStep, PinnedDownloadTotal, RowStep};

use super::*;
use crate::sync_parts::step_outcome::{RowsChangedFollowUp, StepFailure, StepObstacle, StepRefusal};

fn item() -> ItemVersions {
    let place = ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: OsString::from("report") };
    let content = ContentVersion::Hashed { size: 3, mtime: 100, mtime_nsec: 0, hash: Some("h".to_owned()), ctag: Some("c".to_owned()) };
    let version = ItemVersion { place: Some(place.clone()), content: Some(content.clone()) };
    ItemVersions {
        local_item_id: LocalItemId::new(2),
        onedrive_item: Some(OneDriveItemReference { drive_id: DriveId::new("d1").unwrap(), item_id: OneDriveItemId::new("I2".to_owned()) }),
        local: Some(version.clone()),
        onedrive: Some(version),
        onedrive_etag: Some("e1".to_owned()),
        synced_place: Some(place),
        synced_content: Some(content),
        content_presence: ContentPresence::NotOnDisk,
        unsent_since: None,
    }
}

fn some_missing() -> PinnedFilesReading {
    PinnedFilesReading::SomeMissing(TOTAL)
}

const TOTAL: PinnedDownloadTotal = PinnedDownloadTotal { count: 2500, bytes: 5 << 30 };

fn written() -> StepOutcome {
    StepOutcome::Written { events: Vec::new() }
}

#[test]
fn the_reader_is_asked_from_the_start_until_a_reading_answers_that_none_is_missing() {
    let missing = MissingPinnedFiles::new();
    assert!(missing.is_reader_asked(), "the task started");
    missing.note_reading(PinnedFilesReading::NoneMissing);
    assert!(!missing.is_reader_asked());
}

#[test]
fn a_reading_that_was_not_asked_or_found_some_missing_changes_nothing() {
    let missing = MissingPinnedFiles::new();
    missing.note_reading(some_missing());
    missing.note_reading(PinnedFilesReading::NotAsked);
    assert!(missing.is_reader_asked(), "the downloads the last plan planned are planned again");
    missing.note_reading(PinnedFilesReading::NoneMissing);
    missing.note_reading(PinnedFilesReading::NotAsked);
    assert!(!missing.is_reader_asked());
}

#[test]
fn each_cause_that_can_leave_a_pinned_file_missing_has_the_reader_asked_again() {
    let causes = [
        (WakeCause::ListingPageWritten, true),
        (WakeCause::ListingEnded, true),
        (WakeCause::UserAnswered, true),
        (WakeCause::RefreshAsked, true),
        (WakeCause::LocalVersionWritten, false),
        (WakeCause::StepFinished, false),
        (WakeCause::QuotaRead, false),
        (WakeCause::SendingHoldChanged, false),
        (WakeCause::RequestRecordSettled, false),
        (WakeCause::NextCleaningTryChanged, false),
    ];
    for (cause, is_asked_again) in causes {
        assert_eq!(can_cause_leave_pinned_file_missing(cause), is_asked_again, "{cause:?}");
        let missing = MissingPinnedFiles::new();
        missing.note_reading(PinnedFilesReading::NoneMissing);
        missing.note_wake_causes(&[WakeCause::StepFinished, cause].into());
        assert_eq!(missing.is_reader_asked(), is_asked_again, "{cause:?}");
    }
}

#[test]
fn each_end_of_a_step_that_can_leave_a_pinned_file_missing_has_the_reader_asked_again() {
    let download = SyncStep::Folder(FolderStep::DownloadContent { item: item() });
    let take_content = SyncStep::Folder(FolderStep::TakeOneDriveContent { item: item() });
    let send_move = SyncStep::OneDrive(OneDriveStep::SendMove { item: item(), parent: item().onedrive_item.unwrap(), name: OsString::from("report") });
    let agree = SyncStep::Row(RowStep::AgreeOnPlace { item: item() });
    let not_written = [
        StepOutcome::RowsChanged(RowsChangedFollowUp::Nothing),
        StepOutcome::Refused(StepRefusal::Transient { detail: "no route".to_owned() }),
        StepOutcome::NotRun(StepObstacle::SendingStopped),
        StepOutcome::Failed(StepFailure::Store { detail: "busy".to_owned() }),
    ];
    let ends = |step: &SyncStep, is_asked_when_written: bool| std::iter::once((step.clone(), written(), is_asked_when_written)).chain(not_written.iter().map(|outcome| (step.clone(), outcome.clone(), !is_asked_when_written))).collect::<Vec<_>>();
    let cases = [ends(&download, false), ends(&take_content, true), ends(&send_move, true), ends(&agree, true)].concat();
    for (step, outcome, is_asked_again) in cases {
        assert_eq!(can_step_leave_pinned_file_missing(&step, &outcome), is_asked_again, "{step:?} ended {outcome:?}");
        let missing = MissingPinnedFiles::new();
        missing.note_reading(PinnedFilesReading::NoneMissing);
        missing.note_ended_steps(&[(step.clone(), outcome.clone())]);
        assert_eq!(missing.is_reader_asked(), is_asked_again, "{step:?} ended {outcome:?}");
    }
}

/// One line for the journal when the first reading finds pinned files missing, one when a
/// reading finds none; nothing while they go on being missing, and nothing of a reading
/// that did not ask.
#[test]
fn the_download_of_pinned_files_begins_and_ends_once_each() {
    let fewer = PinnedFilesReading::SomeMissing(PinnedDownloadTotal { count: 7, bytes: 6 << 20 });
    let cases = [
        (false, some_missing(), PinnedDownloadNews::Began(TOTAL)),
        (true, fewer, PinnedDownloadNews::Nothing),
        (true, PinnedFilesReading::NoneMissing, PinnedDownloadNews::Ended),
        (false, PinnedFilesReading::NoneMissing, PinnedDownloadNews::Nothing),
        (true, PinnedFilesReading::NotAsked, PinnedDownloadNews::Nothing),
        (false, PinnedFilesReading::NotAsked, PinnedDownloadNews::Nothing),
    ];
    for (was_under_way, reading, news) in cases {
        assert_eq!(decide_download_news(was_under_way, reading), news, "{was_under_way} {reading:?}");
    }

    let missing = MissingPinnedFiles::new();
    assert_eq!(missing.note_reading(PinnedFilesReading::NoneMissing), PinnedDownloadNews::Nothing, "a start with nothing to download");
    assert_eq!(missing.note_reading(some_missing()), PinnedDownloadNews::Began(TOTAL));
    assert_eq!(missing.note_reading(fewer), PinnedDownloadNews::Nothing);
    assert_eq!(missing.note_reading(PinnedFilesReading::NotAsked), PinnedDownloadNews::Nothing);
    assert_eq!(missing.note_reading(PinnedFilesReading::NoneMissing), PinnedDownloadNews::Ended);
    assert_eq!(missing.note_reading(PinnedFilesReading::NoneMissing), PinnedDownloadNews::Nothing);
    assert_eq!(missing.note_reading(some_missing()), PinnedDownloadNews::Began(TOTAL), "pinned again");
}
