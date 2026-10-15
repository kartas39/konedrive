use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::*;
use crate::account_folder::folder::TakeDownTry;
use crate::account_folder::staged_folder::{make_service, read_sync, stage_folder, wait_for_folder, START};
use crate::account_folder::SyncService;

const MINUTE: i64 = 60;

/// The sync of a folder a fault took down, with `attempt` starts tried and the next due at
/// `next_try_at`.
fn taken_down(attempt: u32, next_try_at: Option<i64>) -> Sync {
    Sync::Stopped(Why::TakenDown { cause: "the cause".into(), attempt, next_try_at })
}

/// The count and the time of the next try of a folder that is taken down; `None` for any
/// other state.
async fn read_take_down(service: &SyncService) -> Option<(u32, Option<i64>)> {
    read_sync(service, |folder| match folder.up().map(|up| &up.sync) {
        Some(Sync::Stopped(Why::TakenDown { cause, attempt, next_try_at })) => {
            assert_eq!(cause, "the cause", "the cause is kept as it was");
            Some((*attempt, *next_try_at))
        }
        _ => None,
    })
    .await
}

#[test]
fn the_pause_before_a_try_grows_from_a_minute_to_an_hour_and_stays_there() {
    let cases = [(0, 1), (1, 5), (2, 30), (3, 60), (4, 60), (u32::MAX, 60)];
    for (attempt, minutes) in cases {
        assert_eq!(decide_retry_wait(attempt), std::time::Duration::from_secs(minutes * 60), "attempt {attempt}");
    }
}

#[test]
fn a_folder_taken_down_for_the_first_time_is_tried_as_the_first() {
    assert_eq!(decide_take_down_attempt(None, START), 1);
}

#[test]
fn a_try_that_was_taken_down_again_soon_is_followed_by_the_next() {
    let stayed_up = STAYED_UP_AFTER.as_secs() as i64;
    for (attempt, up_for) in [(1, 0), (1, 5), (3, stayed_up - 1), (0, 30)] {
        let last = TakeDownTry { attempt, started_at: START };
        assert_eq!(decide_take_down_attempt(Some(last), START + up_for), attempt + 1, "attempt {attempt}, up for {up_for} s");
    }
}

#[test]
fn a_try_that_stayed_up_starts_the_count_over() {
    let stayed_up = STAYED_UP_AFTER.as_secs() as i64;
    for up_for in [stayed_up, stayed_up + 1, 24 * 3600] {
        let last = TakeDownTry { attempt: 4, started_at: START };
        assert_eq!(decide_take_down_attempt(Some(last), START + up_for), 1, "up for {up_for} s");
    }
}

#[test]
fn the_sentence_of_a_take_down_names_the_folder_the_cause_and_what_the_user_can_do() {
    let folder = Path::new("/home/u/OneDrive");
    let cases = [
        (TakeDownCause::UndoFailed { path: PathBuf::from("/home/u/OneDrive/a.txt") }, "/home/u/OneDrive/a.txt"),
        (TakeDownCause::BackingFileBehindRow { path: PathBuf::from("/home/u/OneDrive/b.txt") }, "/home/u/OneDrive/b.txt"),
    ];
    let mut sentences = Vec::new();
    for (cause, named) in cases {
        let sentence = describe_take_down(&cause, folder);
        assert!(sentence.contains("the sync folder /home/u/OneDrive was stopped"), "{sentence}");
        assert!(sentence.contains(named), "{sentence}");
        assert!(sentence.contains("sync now"), "{sentence}");
        sentences.push(sentence);
    }
    sentences.dedup();
    assert_eq!(sentences.len(), 2, "each cause has a sentence of its own");
}

/// The owner's timer: each try that is refused is counted, and the next is due after the
/// pause of that count — 1, 5 and 30 minutes, then hourly. The service has no drive, so
/// every start is refused before anything is opened.
#[tokio::test]
async fn a_folder_taken_down_is_tried_again_after_a_growing_pause_and_each_refused_try_is_counted() {
    let (service, clock) = make_service();
    let folder = tempfile::tempdir().unwrap();
    stage_folder(&service, folder.path(), taken_down(0, Some(START + MINUTE)), None).await;
    let timer = tokio::spawn(SyncService::retry_after_take_down(Arc::downgrade(&service)));

    let mut now = START;
    for (tried, pause_before, pause_after) in [(1, 1, 5), (2, 5, 30), (3, 30, 60), (4, 60, 60)] {
        // A moment before the try is due nothing was tried.
        clock.advance(pause_before * MINUTE - 1);
        tokio::task::yield_now().await;
        assert_eq!(read_take_down(&service).await, Some((tried - 1, Some(now + pause_before * MINUTE))), "before try {tried}");
        clock.advance(1);
        now += pause_before * MINUTE;
        wait_for_folder(&service, |folder| matches!(folder.up().map(|up| &up.sync), Some(Sync::Stopped(Why::TakenDown { attempt, .. })) if *attempt == tried)).await;
        assert_eq!(read_take_down(&service).await, Some((tried, Some(now + pause_after * MINUTE))), "after try {tried}");
        assert_eq!(read_sync(&service, |folder| folder.up().and_then(|up| up.take_down_try)).await, None, "a refused try is over");
    }
    assert!(!timer.is_finished(), "the timer waits for the next try");
    timer.abort();
}

/// A folder the user started during the pause is not the timer's
/// try any more, and the timer changes nothing of it — it takes no change of the folder,
/// which would stop a sync that runs.
#[tokio::test]
async fn a_timer_that_finds_the_folder_started_by_the_user_changes_nothing() {
    let (service, clock) = make_service();
    let folder = tempfile::tempdir().unwrap();
    stage_folder(&service, folder.path(), taken_down(0, Some(START + MINUTE)), None).await;
    let timer = tokio::spawn(SyncService::retry_after_take_down(Arc::downgrade(&service)));
    // The timer has read its try and waits.
    tokio::task::yield_now().await;

    // "Sync now" during the pause: the folder is no longer taken down.
    stage_folder(&service, folder.path(), Sync::Stopped(Why::NotStarted), None).await;
    let mut published = service.view.subscribe();
    published.borrow_and_update();

    clock.advance(MINUTE);
    timer.await.unwrap();
    assert!(!published.has_changed().unwrap(), "no change of the folder was made: nothing was stopped");
    assert!(read_sync(&service, |folder| matches!(folder.up().map(|up| &up.sync), Some(Sync::Stopped(Why::NotStarted)))).await);
}

/// A folder taken down again while a timer of an earlier take-down waits: the earlier timer
/// tells its try from the new one by the time it is due, and leaves the new one alone.
#[tokio::test]
async fn a_timer_of_an_earlier_take_down_leaves_a_later_one_to_its_own_timer() {
    let (service, clock) = make_service();
    let folder = tempfile::tempdir().unwrap();
    stage_folder(&service, folder.path(), taken_down(0, Some(START + MINUTE)), None).await;
    let earlier = tokio::spawn(SyncService::retry_after_take_down(Arc::downgrade(&service)));
    tokio::task::yield_now().await;

    // Started by the user and taken down again 30 s later: the same count, another time.
    stage_folder(&service, folder.path(), taken_down(0, Some(START + 30 + MINUTE)), None).await;
    clock.advance(MINUTE);
    earlier.await.unwrap();
    assert_eq!(read_take_down(&service).await, Some((0, Some(START + 30 + MINUTE))), "not tried by the earlier timer");
}

/// The daemon stops during the pause: no try is due any more, the timer ends without a
/// start, and nothing starts the folder afterwards.
#[tokio::test]
async fn the_daemons_end_during_the_pause_ends_the_timer_without_a_try() {
    let (service, clock) = make_service();
    let folder = tempfile::tempdir().unwrap();
    stage_folder(&service, folder.path(), taken_down(2, Some(START + 30 * MINUTE)), None).await;
    let timer = tokio::spawn(SyncService::retry_after_take_down(Arc::downgrade(&service)));
    tokio::task::yield_now().await;

    assert_eq!(service.end_at_daemon_stop().await, None, "no mount to tell the desktop of");
    assert_eq!(read_take_down(&service).await, Some((2, None)));
    clock.advance(30 * MINUTE);
    timer.await.unwrap();
    assert_eq!(read_take_down(&service).await, Some((2, None)), "not tried, not counted");

    // "Sync now" after the daemon's end starts nothing either: the state is as it was.
    assert!(service.refresh().await.is_err());
    assert_eq!(read_take_down(&service).await, Some((2, None)));
}

/// A timer whose service is gone ends by itself.
#[tokio::test]
async fn a_timer_ends_when_its_service_is_gone() {
    let (service, clock) = make_service();
    let folder = tempfile::tempdir().unwrap();
    stage_folder(&service, folder.path(), taken_down(0, Some(START + MINUTE)), None).await;
    let timer = tokio::spawn(SyncService::retry_after_take_down(Arc::downgrade(&service)));
    tokio::task::yield_now().await;
    drop(service);
    clock.advance(MINUTE);
    timer.await.unwrap();
}

/// A Forget is never refused, whatever the folder's sync is: a folder a fault took down is
/// forgotten, `config.toml` records none, and its timer finds nothing to try.
#[tokio::test]
async fn a_forget_of_a_folder_that_is_taken_down_is_made_and_its_timer_tries_nothing() {
    let (service, clock) = make_service();
    let folder = tempfile::tempdir().unwrap();
    stage_folder(&service, folder.path(), taken_down(1, Some(START + 5 * MINUTE)), None).await;
    let timer = tokio::spawn(SyncService::retry_after_take_down(Arc::downgrade(&service)));
    tokio::task::yield_now().await;

    service.unregister_root().await.expect("a Forget is not refused");
    assert!(service.root().is_none());
    assert_eq!(service.root_state(), "none");
    clock.advance(5 * MINUTE);
    timer.await.unwrap();
    assert!(read_sync(&service, |folder| folder.record().is_none()).await);
    assert!(matches!(service.unregister_root().await, Err(SyncError::NoRoot)), "nothing is left to forget");
}

/// `Accounts.Remove`'s first step is a Forget that is not refused either, and retires the
/// account: with a folder it answers the folder, with none it answers none.
#[tokio::test]
async fn a_retirement_forgets_the_folder_and_takes_no_registration_afterwards() {
    let (service, _clock) = make_service();
    let folder = tempfile::tempdir().unwrap();
    stage_folder(&service, folder.path(), Sync::Stopped(Why::CannotStart("it could not".into())), None).await;
    assert_eq!(service.retire().await.unwrap(), Some(folder.path().to_owned()));
    assert!(matches!(service.register_root(folder.path()).await, Err(SyncError::Removing)));
    assert_eq!(service.retire().await.unwrap(), None, "an account with no folder is retired all the same");
}

#[tokio::test]
async fn nothing_is_unsent_for_an_account_with_no_folder() {
    let (service, _clock) = make_service();
    assert_eq!(service.count_unsent().await.unwrap(), 0);
}

/// The warning before a Forget counts through the folder's open store, whether or not its
/// sync runs: every change made here that is not sent.
#[test]
fn what_is_unsent_is_counted_through_the_open_store_of_a_folder_whose_sync_does_not_run() {
    use crate::account_folder::staged_folder::make_kept_parts;
    use crate::mount::item_fixture::ItemFixture;

    let fixture = ItemFixture::new();
    fixture.make_own_file("a.txt", b"one");
    fixture.make_own_file("b.txt", b"two");
    let sent = fixture.show_onedrive_file("sent.txt", b"as in onedrive");
    fixture.remove_here(sent.local_item_id).expect("a removal to send");
    fixture.runtime().block_on(async {
        let (service, _clock) = make_service();
        let kept = make_kept_parts(&fixture);
        let mount = Arc::clone(&kept.mount);
        stage_folder(&service, &fixture.folder(), Sync::Stopped(Why::CannotStart("it cannot".into())), Some(kept)).await;
        assert_eq!(service.count_unsent().await.unwrap(), 3, "two new files and a removal");

        let kept = service.folder.write().await.record_mut().and_then(|record| record.kept.parts.take());
        service.view.send_replace(crate::account_folder::folder::View::default());
        tokio::task::spawn_blocking(move || drop((kept, mount))).await.unwrap();
    });
}

/// A report of a take-down that reaches the owner for a mount that did not ask for one —
/// the folder's mount is another by now — takes nothing down: the folder keeps its parts
/// and its sync is what it was.
#[test]
fn a_report_for_a_mount_that_did_not_ask_takes_nothing_down() {
    use crate::account_folder::staged_folder::make_kept_parts;
    use crate::mount::item_fixture::ItemFixture;

    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let (service, _clock) = make_service();
        let kept = make_kept_parts(&fixture);
        let mount = Arc::clone(&kept.mount);
        assert!(!mount.is_take_down_asked());
        stage_folder(&service, &fixture.folder(), Sync::Stopped(Why::NotStarted), Some(kept)).await;

        SyncService::take_folder_down_for_fault(Arc::downgrade(&service), TakeDownCause::UndoFailed { path: "of another mount".into() }).await;
        assert!(read_sync(&service, |folder| matches!(folder.up().map(|up| &up.sync), Some(Sync::Stopped(Why::NotStarted)))).await);
        assert!(service.mount().is_some_and(|kept| Arc::ptr_eq(&kept, &mount)), "the folder keeps its mount");
        assert_eq!(service.last_error(), "");

        let kept = service.folder.write().await.record_mut().and_then(|record| record.kept.parts.take());
        service.view.send_replace(crate::account_folder::folder::View::default());
        tokio::task::spawn_blocking(move || drop((kept, mount))).await.unwrap();
    });
}
