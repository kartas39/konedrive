use std::path::Path;

use konedrive_tree::RebuildLoss;

use super::*;
use crate::account_folder::staged_folder::{make_service, read_sync, stage_folder, START};
use crate::account_folder::SyncError;

const FOLDER: &str = "/home/u/OneDrive";

fn taken_down(attempt: u32, next_try_at: Option<i64>) -> Sync {
    Sync::Stopped(Why::TakenDown { cause: "the cause".into(), attempt, next_try_at })
}

fn refusals() -> [StartRefusal; 3] {
    [StartRefusal::Store("the store".into()), StartRefusal::DriveNotKnown, StartRefusal::Mount("the mount".into())]
}

#[test]
fn a_refused_try_after_a_take_down_keeps_the_cause_and_counts_the_try() {
    // The owner's own try: one more than the starts tried before. The next is due after the pause of the new count.
    for (tried_before, wait) in [(0, 5 * 60), (1, 30 * 60), (2, 3600), (7, 3600)] {
        for refusal in refusals() {
            let own_try = TakeDownTry { attempt: tried_before + 1, started_at: START };
            let why = decide_refused_sync(&taken_down(tried_before, Some(START)), Some(own_try), &refusal, Path::new(FOLDER), START + 2);
            assert_eq!(why, Why::TakenDown { cause: "the cause".into(), attempt: tried_before + 1, next_try_at: Some(START + 2 + wait) }, "{refusal:?}");
        }
    }
}

#[test]
fn a_refused_refresh_of_a_taken_down_folder_leaves_its_count_and_its_next_try() {
    for next_try_at in [Some(START + 40), None] {
        for refusal in refusals() {
            // The user's try carries the count as it was; a start that is no try at all changes nothing either.
            for user_try in [Some(TakeDownTry { attempt: 2, started_at: START + 10 }), None] {
                let why = decide_refused_sync(&taken_down(2, next_try_at), user_try, &refusal, Path::new(FOLDER), START + 10);
                assert_eq!(why, Why::TakenDown { cause: "the cause".into(), attempt: 2, next_try_at }, "{refusal:?}");
            }
        }
    }
}

#[test]
fn a_start_refused_for_no_drive_waits_for_the_account() {
    for before in [Sync::Stopped(Why::NotStarted), Sync::Stopped(Why::Interrupted), Sync::Stopped(Why::CannotStart("earlier".into())), Sync::Stopped(Why::DriveNotKnown("earlier".into()))] {
        let why = decide_refused_sync(&before, None, &StartRefusal::DriveNotKnown, Path::new(FOLDER), START);
        assert!(matches!(&why, Why::DriveNotKnown(said) if said.contains(FOLDER)), "{why:?}");
    }
}

#[test]
fn any_other_refusal_of_a_folder_that_was_not_taken_down_is_said_with_its_cause() {
    for (refusal, named) in [(StartRefusal::Store("the store is locked".into()), "the store is locked"), (StartRefusal::Mount("no fuse".into()), "no fuse")] {
        let why = decide_refused_sync(&Sync::Stopped(Why::NotStarted), None, &refusal, Path::new(FOLDER), START);
        assert!(matches!(&why, Why::CannotStart(said) if said.contains(FOLDER) && said.contains(named)), "{why:?}");
    }
}

#[test]
fn what_a_rebuild_lost_is_said_only_when_something_the_user_misses_was_lost() {
    let none = RebuildLoss::default();
    assert_eq!(SyncService::describe_rebuild_loss(&none), None);
    // The activity log and the conflicts' list are not said: nothing of the user's work goes with them.
    assert_eq!(SyncService::describe_rebuild_loss(&RebuildLoss { activity_events: 9, conflicts: 2, ..none }), None);
    let cases = [
        (RebuildLoss { pins: 3, ..none }, vec!["3 pin(s)"]),
        (RebuildLoss { unsent_changes_not_on_disk: 2, ..none }, vec!["2 change(s)"]),
        (RebuildLoss { open_upload_sessions: 1, ..none }, vec!["1 upload(s)"]),
        (RebuildLoss { pins: 1, unsent_changes_not_on_disk: 4, open_upload_sessions: 5, ..none }, vec!["1 pin(s)", "4 change(s)", "5 upload(s)"]),
    ];
    for (loss, named) in cases {
        let said = SyncService::describe_rebuild_loss(&loss).expect("something was lost");
        for words in named {
            assert!(said.contains(words), "{said}");
        }
    }
}

/// `Refresh()` of a folder a fault took down tries at once; refused, the folder is taken
/// down still, with the cause, the count and the time of its next try as they were, and the
/// refusal says the cause.
#[tokio::test]
async fn a_refresh_of_a_taken_down_folder_that_is_refused_leaves_it_as_it_was() {
    let (service, _clock) = make_service();
    let folder = tempfile::tempdir().unwrap();
    stage_folder(&service, folder.path(), taken_down(2, Some(START + 1800)), None).await;
    let refused = service.refresh().await.unwrap_err();
    assert!(matches!(&refused, SyncError::NotUp(why) if why.contains("the cause")), "{refused:?}");
    let (sync, take_down_try) = read_sync(&service, |folder| {
        let up = folder.up().expect("the folder is up");
        (matches!(&up.sync, Sync::Stopped(Why::TakenDown { cause, attempt: 2, next_try_at: Some(due) }) if cause == "the cause" && *due == START + 1800), up.take_down_try)
    })
    .await;
    assert!(sync, "the count and the next try are as they were");
    assert_eq!(take_down_try, None, "the user's try is over");
    assert_eq!(service.last_error(), "the cause", "the trouble shown is the take-down's cause");
}

/// A start that is refused says why as the folder's blocking trouble, and `Refresh()`
/// answers it.
#[tokio::test]
async fn a_refused_start_is_the_folders_trouble_and_the_refusal_of_a_refresh() {
    let (service, _clock) = make_service();
    let folder = tempfile::tempdir().unwrap();
    stage_folder(&service, folder.path(), Sync::Stopped(Why::NotStarted), None).await;
    let refused = service.refresh().await.unwrap_err();
    assert!(matches!(&refused, SyncError::NotUp(why) if why.contains("no drive is configured")), "{refused:?}");
    assert!(service.last_error().contains("no drive is configured"), "{}", service.last_error());
    assert!(read_sync(&service, |folder| matches!(folder.up().map(|up| &up.sync), Some(Sync::Stopped(Why::CannotStart(_))))).await);
}

#[tokio::test]
async fn a_refresh_with_no_folder_is_refused() {
    let (service, _clock) = make_service();
    assert!(matches!(service.refresh().await, Err(SyncError::NoRoot)));
}
