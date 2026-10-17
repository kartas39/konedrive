use konedrive_tree::{DriveId, OneDriveItemId};

use super::*;

fn item(number: i64) -> LocalItemId {
    LocalItemId::new(number)
}

fn step(class: StepClass, number: i64, other_items: &[i64]) -> RunningStep {
    RunningStep { class, item: item(number), other_items: other_items.iter().copied().map(item).collect(), destination: None, sent: 0, total: 0 }
}

fn directory(id: &str) -> OneDriveItemReference {
    OneDriveItemReference { drive_id: DriveId::new("drive").unwrap(), item_id: OneDriveItemId::new(id) }
}

fn items(numbers: &[i64]) -> BTreeSet<LocalItemId> {
    numbers.iter().copied().map(item).collect()
}

#[test]
fn every_row_a_step_carries_has_a_running_step_until_it_ends() {
    let running_steps = RunningSteps::new();
    assert!(running_steps.list_items().is_empty() && running_steps.list_steps().is_empty());
    // A directory's removal carries the rows below it.
    assert!(running_steps.begin(step(StepClass::Metadata, 5, &[6, 7])));
    assert!(running_steps.begin(step(StepClass::Row, 9, &[])));
    assert_eq!(running_steps.list_items(), items(&[5, 6, 7, 9]));
    assert!([5, 6, 7, 9].into_iter().all(|row| running_steps.has_running_step(item(row))));
    assert!(!running_steps.has_running_step(item(8)));
    assert_eq!(running_steps.list_steps(), [step(StepClass::Metadata, 5, &[6, 7]), step(StepClass::Row, 9, &[])]);
    running_steps.end(item(5));
    assert_eq!(running_steps.list_items(), items(&[9]));
    assert!(!running_steps.has_running_step(item(6)));
    // An end of what does not run changes nothing.
    running_steps.end(item(5));
    running_steps.end(item(6));
    assert_eq!(running_steps.list_steps(), [step(StepClass::Row, 9, &[])]);
}

#[test]
fn a_step_is_not_begun_for_an_item_that_has_one() {
    let running_steps = RunningSteps::new();
    assert!(running_steps.begin(step(StepClass::Metadata, 5, &[6])));
    assert!(!running_steps.begin(step(StepClass::Content, 5, &[])), "the item itself");
    assert!(!running_steps.begin(step(StepClass::Content, 6, &[])), "a row another step carries");
    assert!(!running_steps.begin(step(StepClass::Folder, 8, &[9, 6])), "a row both steps would carry");
    assert_eq!(running_steps.list_items(), items(&[5, 6]), "nothing of a step that was not begun runs");
    assert_eq!(running_steps.list_steps(), [step(StepClass::Metadata, 5, &[6])]);
    running_steps.end(item(5));
    assert!(running_steps.begin(step(StepClass::Folder, 8, &[9, 6])));
}

#[test]
fn progress_told_for_an_item_with_no_running_step_is_dropped() {
    let running_steps = RunningSteps::new();
    running_steps.tell_progress(item(5), 10, 100);
    assert!(running_steps.list_steps().is_empty());
    assert!(running_steps.begin(step(StepClass::Content, 5, &[])));
    running_steps.tell_progress(item(5), 10, 100);
    running_steps.tell_progress(item(6), 99, 100);
    assert_eq!(running_steps.list_steps(), [RunningStep { sent: 10, total: 100, ..step(StepClass::Content, 5, &[]) }]);
    running_steps.end(item(5));
    running_steps.tell_progress(item(5), 20, 100);
    assert!(running_steps.list_steps().is_empty());
}

#[test]
fn the_directories_the_running_steps_send_into_are_listed_once_each() {
    let running_steps = RunningSteps::new();
    for (number, destination) in [(1, Some("D1")), (2, None), (3, Some("D2")), (4, Some("D1"))] {
        assert!(running_steps.begin(RunningStep { destination: destination.map(directory), ..step(StepClass::Metadata, number, &[]) }));
    }
    assert_eq!(running_steps.list_destinations(), [directory("D1"), directory("D2")]);
}

#[tokio::test]
async fn the_requests_are_aborted_at_a_stop_and_the_steps_towards_the_folder_are_left_running() {
    let running_steps = RunningSteps::new();
    let classes = [StepClass::Content, StepClass::Metadata, StepClass::Download, StepClass::Row, StepClass::Folder];
    let mut tasks = Vec::new();
    for (number, class) in (1..).zip(classes) {
        assert!(running_steps.begin(step(class, number, &[number + 10])));
        let task = tokio::spawn(std::future::pending::<()>());
        running_steps.keep_abort_handle(item(number), task.abort_handle());
        tasks.push(task);
    }
    assert_eq!(running_steps.count_steps_to_settle(), 1);
    assert_eq!(running_steps.abort_requests(), [item(1), item(2), item(3), item(4)], "every step whose request is in flight, but the one towards the folder");
    let folder_task = tasks.pop().unwrap();
    for (task, class) in tasks.into_iter().zip(classes) {
        assert!(task.await.unwrap_err().is_cancelled(), "{class:?}");
    }
    tokio::task::yield_now().await;
    assert!(!folder_task.is_finished(), "a step towards the folder is never aborted");
    assert_eq!(running_steps.list_items(), items(&[5, 15]), "the rows of what was aborted have no running step");
    assert_eq!(running_steps.count_steps_to_settle(), 1);
    running_steps.end(item(5));
    assert_eq!(running_steps.count_steps_to_settle(), 0);
    folder_task.abort();
    // A handle for a step that runs no more is dropped.
    let late = tokio::spawn(std::future::pending::<()>());
    running_steps.keep_abort_handle(item(1), late.abort_handle());
    assert!(running_steps.abort_requests().is_empty());
    tokio::task::yield_now().await;
    assert!(!late.is_finished());
    late.abort();
}

/// A commit is a job of the store's thread: an abort would drop the task that waits for
/// it, the job would write all the same, and nobody would be told.
#[tokio::test]
async fn a_step_whose_commit_has_begun_is_left_to_a_stop_and_settled() {
    let running_steps = RunningSteps::new();
    let mut tasks = Vec::new();
    for (number, class) in (1..).zip([StepClass::Content, StepClass::Metadata, StepClass::Row]) {
        assert!(running_steps.begin(step(class, number, &[number + 10])));
        let task = tokio::spawn(std::future::pending::<()>());
        running_steps.keep_abort_handle(item(number), task.abort_handle());
        tasks.push(task);
    }
    assert_eq!(running_steps.count_steps_to_settle(), 0, "a request in flight is not waited for");
    assert!(running_steps.begin_commit(item(2)));
    assert!(running_steps.begin_commit(item(3)));
    assert!(running_steps.begin_commit(item(3)), "said twice, the same");
    assert_eq!(running_steps.count_steps_to_settle(), 2);
    assert_eq!(running_steps.abort_requests(), [item(1)]);
    assert!(tasks.remove(0).await.unwrap_err().is_cancelled());
    tokio::task::yield_now().await;
    assert!(tasks.iter().all(|task| !task.is_finished()), "a step that commits is not aborted");
    assert_eq!(running_steps.list_items(), items(&[2, 12, 3, 13]), "and runs until it is settled");
    // One that was aborted is told so when it asks: it does not commit.
    assert!(!running_steps.begin_commit(item(1)));
    assert!(!running_steps.begin_commit(item(12)), "asked by the step's own item");
    running_steps.end(item(2));
    running_steps.end(item(3));
    assert_eq!(running_steps.count_steps_to_settle(), 0);
    tasks.iter().for_each(|task| task.abort());
}

#[test]
fn a_stop_aborts_a_request_in_flight_of_every_class_but_the_one_towards_the_folder() {
    for class in [StepClass::Content, StepClass::Metadata, StepClass::Download, StepClass::Row] {
        assert!(decide_is_aborted_at_stop(class, StepPhase::RequestInFlight), "{class:?}");
        assert!(!decide_is_aborted_at_stop(class, StepPhase::CommitBegun), "{class:?}");
    }
    assert!(!decide_is_aborted_at_stop(StepClass::Folder, StepPhase::RequestInFlight));
    assert!(!decide_is_aborted_at_stop(StepClass::Folder, StepPhase::CommitBegun));
}
