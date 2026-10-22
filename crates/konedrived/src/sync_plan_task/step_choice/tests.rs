//! Which steps of a plan start now: a held class, a row with a running step, the limits
//! of each class, and the order the plan gave.

use std::ffi::OsString;

use konedrive_tree::{ContentPresence, DriveId, ItemVersions, OneDriveItemId, OneDriveItemReference};

use super::*;

fn versions(number: i64) -> ItemVersions {
    ItemVersions {
        local_item_id: LocalItemId::new(number),
        onedrive_item: None,
        local: None,
        onedrive: None,
        onedrive_etag: None,
        synced_place: None,
        synced_content: None,
        content_presence: ContentPresence::OnDisk,
        unsent_since: None,
    }
}

fn reference(number: i64) -> OneDriveItemReference {
    OneDriveItemReference { drive_id: DriveId::new("d1").unwrap(), item_id: OneDriveItemId::new(format!("I{number}")) }
}

fn send_content(number: i64) -> SyncStep {
    SyncStep::OneDrive(OneDriveStep::SendContent { item: versions(number) })
}

fn send_new_file(number: i64) -> SyncStep {
    SyncStep::OneDrive(OneDriveStep::SendNewFile { item: versions(number), parent: reference(1), name: OsString::from("new") })
}

fn send_removal(number: i64) -> SyncStep {
    SyncStep::OneDrive(OneDriveStep::SendFileRemoval { item: versions(number) })
}

fn send_directory_removal(number: i64, below: &[i64]) -> SyncStep {
    SyncStep::OneDrive(OneDriveStep::SendDirectoryRemoval { item: versions(number), below: below.iter().map(|&n| versions(n)).collect() })
}

fn download(number: i64) -> SyncStep {
    SyncStep::Folder(FolderStep::DownloadContent { item: versions(number) })
}

fn show(number: i64) -> SyncStep {
    SyncStep::Folder(FolderStep::ShowItem { item: versions(number) })
}

fn remove_here(number: i64) -> SyncStep {
    SyncStep::Folder(FolderStep::RemoveItem { item: versions(number) })
}

fn merge(local: i64, onedrive: i64) -> SyncStep {
    SyncStep::Row(RowStep::MergeItems { local: versions(local), onedrive: versions(onedrive) })
}

fn give_up_binding(number: i64) -> SyncStep {
    SyncStep::Row(RowStep::GiveUpBinding { item: versions(number) })
}

/// Room enough that no limit is reached.
fn no_limit() -> StepLimits {
    StepLimits { content: 100, metadata: 100, download: 100 }
}

fn items(steps: &[SyncStep]) -> Vec<i64> {
    steps.iter().map(|step| step.local_item_id().as_i64()).collect()
}

#[test]
fn each_step_is_of_its_class() {
    let cases = [
        (send_new_file(2), StepClass::Content),
        (send_content(2), StepClass::Content),
        (send_removal(2), StepClass::Metadata),
        (send_directory_removal(2, &[]), StepClass::Metadata),
        (download(2), StepClass::Download),
        (show(2), StepClass::Folder),
        (remove_here(2), StepClass::Folder),
        (merge(2, 3), StepClass::Row),
        (give_up_binding(2), StepClass::Row),
    ];
    for (step, class) in cases {
        assert_eq!(decide_step_class(&step), class, "{step:?}");
    }
}

/// What holds all sending holds both classes towards OneDrive; the pause and the sign-out
/// hold downloads too. A step towards the folder and a step of the rows are never held.
#[test]
fn nothing_of_a_held_class_starts_and_every_other_step_does() {
    let planned = vec![send_content(2), send_removal(3), download(4), remove_here(5), give_up_binding(6)];

    let sending_held = decide_steps_to_start(planned.clone(), &[], &no_limit(), &BTreeSet::from([StepClass::Content, StepClass::Metadata]));
    assert_eq!(items(&sending_held.steps), [4, 5, 6]);
    assert_eq!(items(&sending_held.left), [2, 3]);

    let all_held = decide_steps_to_start(planned, &[], &no_limit(), &BTreeSet::from([StepClass::Content, StepClass::Metadata, StepClass::Download]));
    assert_eq!(items(&all_held.steps), [5, 6]);
    assert_eq!(items(&all_held.left), [2, 3, 4]);
}

/// A row is in one step at a time: the item of a running step, a row below a directory whose
/// removal runs, the second row of a merge that runs, and a row a step of this round took.
#[test]
fn no_step_starts_for_a_row_that_a_running_step_or_a_step_started_before_it_carries() {
    let running = [describe_running_step(&send_content(2)), describe_running_step(&send_directory_removal(3, &[4])), describe_running_step(&merge(5, 6))];
    let planned = vec![remove_here(2), send_removal(4), give_up_binding(6), download(7), show(7), merge(8, 9), send_removal(9)];

    let choice = decide_steps_to_start(planned, &running, &no_limit(), &BTreeSet::new());

    assert_eq!(items(&choice.steps), [7, 8]);
    assert_eq!(items(&choice.left), [2, 4, 6, 7, 9]);
}

/// The steps that run count against the limits: one step towards OneDrive that is not
/// content at a time, and as many uploads and downloads as the pool's slots and four more.
#[test]
fn of_a_bounded_class_as_many_start_as_the_limits_leave_beside_what_runs() {
    let limits = decide_step_limits(2);
    assert_eq!(limits, StepLimits { content: 6, metadata: 1, download: 6 });

    let running = [describe_running_step(&send_content(1)), describe_running_step(&download(2))];
    let planned: Vec<SyncStep> = (10..17).map(send_content).chain((20..27).map(download)).chain([send_removal(30), send_removal(31)]).chain((40..43).map(remove_here)).collect();

    let choice = decide_steps_to_start(planned, &running, &limits, &BTreeSet::new());

    assert_eq!(items(&choice.steps), [10, 11, 12, 13, 14, 20, 21, 22, 23, 24, 30, 40, 41, 42]);
    assert_eq!(items(&choice.left), [15, 16, 25, 26, 31]);

    let metadata_runs = decide_steps_to_start(vec![send_removal(30)], &[describe_running_step(&send_removal(3))], &limits, &BTreeSet::new());
    assert_eq!(items(&metadata_runs.left), [30]);
}

/// The steps that show an item go as one job, apart; every other step that starts, and
/// every one left, keeps the plan's order.
#[test]
fn the_show_steps_go_apart_and_the_others_keep_the_plans_order() {
    let planned = vec![show(2), send_content(3), show(4), remove_here(5), give_up_binding(6), show(7)];

    let choice = decide_steps_to_start(planned, &[], &no_limit(), &BTreeSet::from([StepClass::Content]));

    assert_eq!(items(&choice.show_steps), [2, 4, 7]);
    assert_eq!(items(&choice.steps), [5, 6]);
    assert_eq!(items(&choice.left), [3]);
}
