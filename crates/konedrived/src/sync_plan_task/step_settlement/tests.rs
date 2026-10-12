//! The settlement of a step: whose its failed tries are, what a step from a row that is
//! not unsent keeps, and what is kept when the store could not be written.

use std::ffi::OsString;

use konedrive_tree::{ContentPresence, ContentVersion, DriveId, FolderStep, ItemPlace, ItemVersion, OneDriveItemId, OneDriveItemReference};

use super::*;
use crate::mount::item_fixture::ItemFixture;

const NOW: i64 = 1_000;

fn id(number: i64) -> LocalItemId {
    LocalItemId::new(number)
}

fn place(name: &str) -> ItemPlace {
    ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: OsString::from(name) }
}

fn content(hash: &str) -> ContentVersion {
    ContentVersion::Hashed { size: 3, mtime: 100, mtime_nsec: 0, hash: Some(hash.to_owned()), ctag: Some(format!("c-{hash}")) }
}

fn reference(number: i64) -> OneDriveItemReference {
    OneDriveItemReference { drive_id: DriveId::new("d1").unwrap(), item_id: OneDriveItemId::new(format!("I{number}")) }
}

/// A file the two sides agree on but for its place in OneDrive, which is another name: the
/// row is not unsent.
fn not_unsent(number: i64) -> ItemVersions {
    let version = ItemVersion { place: Some(place("report")), content: Some(content("agreed")) };
    ItemVersions {
        local_item_id: id(number),
        onedrive_item: Some(reference(number)),
        local: Some(version.clone()),
        onedrive: Some(ItemVersion { place: Some(place(".konedrive-temporary")), ..version }),
        onedrive_etag: Some("e1".to_owned()),
        synced_place: Some(place("report")),
        synced_content: Some(content("agreed")),
        content_presence: ContentPresence::OnDisk,
        unsent_since: None,
    }
}

fn unsent(number: i64, stamp: u64) -> ItemVersions {
    let mut item = not_unsent(number);
    item.local.as_mut().unwrap().content = Some(content("mine"));
    item.unsent_since = Some(stamp);
    item
}

fn send_move(item: ItemVersions) -> SyncStep {
    SyncStep::OneDrive(OneDriveStep::SendMove { item, parent: reference(1), name: OsString::from("report") })
}

fn send_content(item: ItemVersions) -> SyncStep {
    SyncStep::OneDrive(OneDriveStep::SendContent { item })
}

fn transient() -> StepOutcome {
    StepOutcome::Refused(StepRefusal::Transient { detail: "no route".to_owned() })
}

#[test]
fn the_next_try_is_a_second_after_the_first_failure_and_doubles_to_an_hour() {
    assert_eq!(decide_next_try(1, NOW), NOW + 1);
    assert_eq!(decide_next_try(2, NOW), NOW + 2);
    assert_eq!(decide_next_try(5, NOW), NOW + 16);
    assert_eq!(decide_next_try(40, NOW), NOW + BACKOFF_MAX.as_secs() as i64);
}

#[test]
fn a_failed_try_of_a_step_towards_onedrive_is_the_stores_and_any_other_the_tasks() {
    let towards_onedrive = decide_step_settlement(&send_content(unsent(2, 7)), &transient(), 2, NOW);
    assert_eq!(towards_onedrive.memory, vec![MemoryWrite::FailedTry { reason: Reason::Network, next_try_at: Some(NOW + 4) }]);
    assert_eq!((towards_onedrive.unsent_since, towards_onedrive.local_step_tries), (Some(7), LocalStepTriesChange::Nothing));

    let towards_folder = SyncStep::Folder(FolderStep::DownloadContent { item: not_unsent(2) });
    let settled = decide_step_settlement(&towards_folder, &StepOutcome::Failed(StepFailure::Disk { errno: 28, path: "/folder/report".into() }), 0, NOW);
    assert_eq!((settled.memory, settled.local_step_tries), (Vec::new(), LocalStepTriesChange::FailedTry { reason: Reason::LocalIo, next_try_at: Some(NOW + 1) }));
}

/// A move owed to OneDrive after a killed exchange of names, and a content that is the
/// folder's own upload sent again: the store keeps no try of a row with no stamp.
#[test]
fn a_failed_try_of_a_step_towards_onedrive_from_a_row_that_is_not_unsent_is_the_tasks() {
    for step in [send_move(not_unsent(2)), send_content(not_unsent(2))] {
        let settled = decide_step_settlement(&step, &transient(), 3, NOW);
        assert_eq!(settled.memory, Vec::new(), "{step:?}: nothing the store would not keep");
        assert_eq!(settled.local_step_tries, LocalStepTriesChange::FailedTry { reason: Reason::Network, next_try_at: Some(NOW + 8) });
        assert!(has_changed_anything(&transient(), &settled));

        let mut tries = LocalStepTries::default();
        change_local_step_tries(&mut tries, &[(step.clone(), settled)]);
        assert_eq!(count_failed_tries(&step, &tries, &BTreeMap::new()), 1);
        assert_eq!(tries.list_next_tries(), [((id(2), konedrive_tree::TriedStep::TowardsOneDrive), konedrive_tree::NextTry { not_before: NOW + 8, reason: Reason::Network })].into(), "the plan is told, and answers the wait");
    }
}

#[test]
fn what_onedrive_refuses_of_a_row_that_is_not_unsent_is_a_failed_try_of_the_tasks() {
    let refusals = [
        (StepRefusal::Forbidden, Reason::Forbidden, CycleRequest::None),
        (StepRefusal::RefusedForGood { message: "bad".to_owned() }, Reason::Refused(Some("bad".to_owned())), CycleRequest::None),
        (StepRefusal::NameTaken { place: place("report"), holder_name: OsString::from("Report") }, Reason::NameHeld, CycleRequest::Changes),
        (StepRefusal::NoSpace { free_space: Some(1) }, Reason::Quota, CycleRequest::None),
    ];
    for (refusal, reason, cycle) in refusals {
        let settled = decide_step_settlement(&send_content(not_unsent(2)), &StepOutcome::Refused(refusal.clone()), 0, NOW);
        assert_eq!((settled.memory.clone(), settled.cycle), (Vec::new(), cycle), "{refusal:?}");
        assert_eq!(settled.local_step_tries, LocalStepTriesChange::FailedTry { reason, next_try_at: Some(NOW + 1) }, "{refusal:?}");
    }
    // From an unsent row the store keeps the fact itself, under the stamp.
    let kept_by_store = decide_step_settlement(&send_content(unsent(2, 7)), &StepOutcome::Refused(StepRefusal::Forbidden), 0, NOW);
    assert!(matches!(kept_by_store.memory.as_slice(), [MemoryWrite::Refusal(_)]));
    assert_eq!(kept_by_store.local_step_tries, LocalStepTriesChange::Nothing);
}

#[test]
fn a_step_that_is_written_forgets_the_tasks_tries_of_it() {
    let written = StepOutcome::Written { events: Vec::new() };
    for (step, went_through) in [(send_move(not_unsent(2)), true), (send_content(unsent(2, 7)), true), (SyncStep::Folder(FolderStep::DownloadContent { item: not_unsent(2) }), false)] {
        let mut tries = LocalStepTries::default();
        tries.remember_failed_try(&LocalStep::describe(&step), Reason::Network, Some(NOW + 60));
        let settled = decide_step_settlement(&step, &written, 1, NOW);
        assert_eq!((settled.local_step_tries.clone(), settled.went_through), (LocalStepTriesChange::Forgotten, went_through));
        change_local_step_tries(&mut tries, &[(step, settled)]);
        assert_eq!(tries, LocalStepTries::default());
    }
}

#[test]
fn a_settlement_the_store_could_not_be_written_is_a_try_the_task_keeps() {
    let failed = send_content(unsent(2, 7));
    let refused = send_content(unsent(3, 8));
    let written_elsewhere = send_content(unsent(4, 9));
    let nothing_to_keep = send_content(unsent(5, 10));
    let settled = vec![
        (failed.clone(), decide_step_settlement(&failed, &transient(), 0, NOW)),
        (refused.clone(), decide_step_settlement(&refused, &StepOutcome::Refused(StepRefusal::Forbidden), 0, NOW)),
        (written_elsewhere.clone(), decide_step_settlement(&written_elsewhere, &transient(), 0, NOW)),
        (nothing_to_keep.clone(), decide_step_settlement(&nothing_to_keep, &StepOutcome::Refused(StepRefusal::SignedOut), 0, NOW)),
    ];
    let mut tries = LocalStepTries::default();
    keep_unwritten_tries(&mut tries, &settled, &[id(2), id(3), id(5)].into(), NOW);
    let towards_onedrive = konedrive_tree::TriedStep::TowardsOneDrive;
    assert_eq!(
        tries.list_next_tries(),
        [
            ((id(2), towards_onedrive), konedrive_tree::NextTry { not_before: NOW + 1, reason: Reason::Network }),
            ((id(3), towards_onedrive), konedrive_tree::NextTry { not_before: NOW + 1, reason: Reason::Store }),
        ]
        .into(),
        "the step whose try was not written waits; one that was written, and one with nothing to keep, are the store's and nobody's"
    );
    // The next try that cannot be written either waits twice as long.
    keep_unwritten_tries(&mut tries, &settled[..1], &[id(2)].into(), NOW + 1);
    assert_eq!(count_failed_tries(&failed, &tries, &BTreeMap::new()), 2);
    assert_eq!(tries.list_next_tries()[&(id(2), towards_onedrive)].not_before, NOW + 3);
    // The store's own count stands when it is more.
    assert_eq!(count_failed_tries(&failed, &tries, &[(id(2), 5)].into()), 5);
}

/// A step that ran across a plan its item was not given in counts on from what the store
/// keeps, read at the settlement.
#[test]
fn the_count_of_tries_is_the_stores_of_the_stamp_the_step_was_planned_from() {
    let fixture = ItemFixture::new();
    let changed = fixture.make_own_file("report", b"mine");
    let (item, stamp) = (changed.local_item_id, changed.unsent_since);
    assert!(stamp.is_some());
    fixture.write_rows(move |store| {
        for _ in 0..3 {
            let _ = store.step_memory().remember_failed_try(item, stamp, &Reason::Network, Some(NOW))?;
        }
        Ok(())
    });
    let another_stamp = stamp.map(|stamp| stamp + 1);
    let counts = fixture.read_rows(move |items| read_failed_try_counts(items, &[(item, stamp), (id(999), Some(1))]));
    assert_eq!(counts, [(item, 3)].into());
    assert_eq!(fixture.read_rows(move |items| read_failed_try_counts(items, &[(item, another_stamp)])), BTreeMap::new(), "the tries of another unsent change are not counted on");

    let step = send_content(fixture.versions_of(item));
    let settled = decide_step_settlement(&step, &transient(), count_failed_tries(&step, &LocalStepTries::default(), &counts), NOW);
    assert_eq!(settled.memory, vec![MemoryWrite::FailedTry { reason: Reason::Network, next_try_at: Some(NOW + 8) }]);
}

#[test]
fn a_written_step_is_said_in_the_journal_without_the_path_of_its_event() {
    let event = StepEvent { kind: ActivityKind::Uploaded, path: "Private/report.txt".into(), former_path: None };
    let said = describe_outcome(&StepOutcome::Written { events: vec![event] });
    assert_eq!(said, "written, 1 event(s)");
    assert_eq!(describe_outcome(&StepOutcome::NotRun(StepObstacle::SendingStopped)), "not run (SendingStopped)");
    assert_eq!(super::super::step_choice::find_step_name(&send_content(unsent(2, 7))), "SendContent");
    assert_eq!(super::super::step_choice::find_step_name(&SyncStep::Folder(FolderStep::DownloadContent { item: not_unsent(2) })), "DownloadContent");
}

#[test]
fn the_first_filling_logs_no_item_shown_and_every_other_event() {
    let event = |kind| StepEvent { kind, path: "a.txt".into(), former_path: None };
    let events = [event(ActivityKind::Added), event(ActivityKind::Downloaded), event(ActivityKind::Conflict), event(ActivityKind::Added)];
    assert_eq!(decide_events_to_log(&events, 0, true), [event(ActivityKind::Downloaded), event(ActivityKind::Conflict)]);
    assert_eq!(decide_events_to_log(&events, 0, false), events);
    // The cap per listing is of what OneDrive changed alone.
    assert_eq!(decide_events_to_log(&events, crate::status::activity::PER_KIND, false), [event(ActivityKind::Downloaded), event(ActivityKind::Conflict)]);
}
