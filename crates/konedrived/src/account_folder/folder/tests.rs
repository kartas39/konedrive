use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::FutureExt;

use super::*;
use crate::account_folder::staged_folder::{make_kept_parts, make_service, read_sync, stage_folder, wait_for_folder};
use crate::conditions::running::Clock;
use crate::mount::item_fixture::ItemFixture;
use crate::sync_tasks::start_order::StartRefusal;

/// The answer of a start that was refused for `refusal`, with nothing kept.
fn refused(refusal: StartRefusal) -> SyncChange {
    SyncChange::Refused(RefusedStart { refusal, rebuild_loss: None, kept: None })
}

/// What tells whether a caller's cut had its answer adopted.
fn adoption() -> (Arc<AtomicBool>, impl FnOnce() + Send + 'static) {
    let is_adopted = Arc::new(AtomicBool::new(false));
    let adopted = Arc::clone(&is_adopted);
    (is_adopted, move || adopted.store(true, Ordering::SeqCst))
}

#[tokio::test]
async fn a_start_or_a_stop_that_is_waited_for_answers_its_caller_and_is_not_adopted() {
    let task = SyncChangeTask::default();
    let (is_adopted, adopt) = adoption();
    let answer = task.run(async { refused(StartRefusal::DriveNotKnown) }, adopt).await;
    assert!(matches!(answer, Some(SyncChange::Refused(RefusedStart { refusal: StartRefusal::DriveNotKnown, .. }))));
    assert!(!is_adopted.load(Ordering::SeqCst));
    assert!(task.wait().await.is_none(), "the caller took the answer: nothing is left");
}

/// A caller cut while it waits: the start or the stop goes on in its task, its answer is
/// left for whoever waits next, and the adoption is asked for at the cut.
#[tokio::test]
async fn a_caller_that_is_cut_leaves_the_task_running_and_its_answer_for_the_next_wait() {
    let task = SyncChangeTask::default();
    let (is_adopted, adopt) = adoption();
    let (go_on, may_go_on) = tokio::sync::oneshot::channel::<()>();
    let work = async move {
        let _ = may_go_on.await;
        refused(StartRefusal::Mount("answered after the cut".into()))
    };
    // Polled once, which spawns the task, and dropped: the caller is cut.
    assert!(task.run(work, adopt).now_or_never().is_none());
    assert!(is_adopted.load(Ordering::SeqCst), "the cut asks for the adoption");
    assert!(crate::panic::lock(&task.left).is_none(), "the task has not answered yet");

    go_on.send(()).unwrap();
    // The next waiter waits for the task and takes what it left.
    let left = task.wait().await;
    assert!(matches!(left, Some(SyncChange::Refused(RefusedStart { refusal: StartRefusal::Mount(why), .. })) if why == "answered after the cut"));
    assert!(task.wait().await.is_none());
}

#[tokio::test]
async fn a_start_that_panics_answers_nothing_and_frees_the_gate() {
    let task = SyncChangeTask::default();
    let (is_adopted, adopt) = adoption();
    let answer = task.run(async { panic!("the start failed (a test's panic)") }, adopt).await;
    assert!(answer.is_none());
    assert!(!is_adopted.load(Ordering::SeqCst), "the caller was not cut");
    assert!(task.wait().await.is_none(), "the next change does not wait for ever");
}

/// What a cut start left is taken into the state by the next change of the folder, as the
/// start's own caller would have said it.
#[tokio::test]
async fn the_next_change_says_a_refusal_that_a_cut_start_left() {
    let (service, _clock) = make_service();
    let folder = tempfile::tempdir().unwrap();
    stage_folder(&service, folder.path(), Sync::Stopped(Why::NotStarted), None).await;
    assert!(service.sync_change.run(async { refused(StartRefusal::DriveNotKnown) }, || {}).now_or_never().is_none());

    let stopped = service.change().await;
    assert!(matches!(stopped.folder().up().map(|up| &up.sync), Some(Sync::Stopped(Why::DriveNotKnown(_)))));
    assert!(!stopped.ran(), "a start that was refused left nothing running");
}

/// The cut start was the owner's own try after a take-down: its refusal is counted as that
/// try, and the folder stays taken down with its cause.
#[tokio::test]
async fn a_refusal_left_by_a_cut_try_after_a_take_down_is_counted_and_keeps_the_cause() {
    let (service, clock) = make_service();
    let folder = tempfile::tempdir().unwrap();
    stage_folder(&service, folder.path(), Sync::Stopped(Why::TakenDown { cause: "the cause".into(), attempt: 1, next_try_at: Some(clock.now()) }), None).await;
    service.folder.write().await.up_mut().unwrap().take_down_try = Some(TakeDownTry { attempt: 2, started_at: clock.now() });
    assert!(service.sync_change.run(async { refused(StartRefusal::Mount("no".into())) }, || {}).now_or_never().is_none());

    let stopped = service.change().await;
    let up = stopped.folder().up().unwrap();
    assert!(matches!(&up.sync, Sync::Stopped(Why::TakenDown { cause, attempt: 2, next_try_at: Some(due) }) if cause == "the cause" && *due == clock.now() + 30 * 60), "counted, and the next try due");
    assert_eq!(up.take_down_try, None);
}

/// The adoption a cut caller asks for: a task of the service's own makes the change, with
/// nobody calling it.
#[tokio::test]
async fn an_answer_nobody_waited_for_is_adopted_by_a_task_of_the_service() {
    let (service, _clock) = make_service();
    let folder = tempfile::tempdir().unwrap();
    stage_folder(&service, folder.path(), Sync::Stopped(Why::NotStarted), None).await;
    assert!(service.sync_change.run(async { refused(StartRefusal::DriveNotKnown) }, service.adopt_left_sync_change()).now_or_never().is_none());
    wait_for_folder(&service, |folder| matches!(folder.up().map(|up| &up.sync), Some(Sync::Stopped(Why::DriveNotKnown(_))))).await;
}

/// The parts a stop left whose caller was cut go into the folder's record, whole, at the
/// next change, and the readers find them in the view: the mount is still held, and the
/// change knows that a sync ran.
#[test]
fn the_parts_a_cut_stop_left_are_kept_with_the_folder_at_the_next_change() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let (service, _clock) = make_service();
        stage_folder(&service, &fixture.folder(), Sync::Stopped(Why::Interrupted), None).await;
        let kept = make_kept_parts(&fixture);
        let mount = Arc::clone(&kept.mount);
        // The stop answers only once its caller was cut: on this runtime of several threads it could answer first.
        let (go_on, may_go_on) = tokio::sync::oneshot::channel::<()>();
        let stop = async move {
            let _ = may_go_on.await;
            SyncChange::Stopped(kept)
        };
        assert!(service.sync_change.run(stop, || {}).now_or_never().is_none());
        go_on.send(()).unwrap();
        {
            let stopped = service.change().await;
            assert!(stopped.ran(), "the sync that was stopped is one the change starts again");
            let parts = stopped.folder().parts().expect("the parts are in the record");
            assert!(Arc::ptr_eq(&parts.mount, &mount), "the very mount the stop answered");
        }
        assert!(service.mount().is_some_and(|published| Arc::ptr_eq(&published, &mount)), "and the view has it");
        assert!(service.tree_store().is_some());
        // The scene's end: the folder's parts go where a thread may be joined.
        let parts = service.folder.write().await.record_mut().and_then(|record| record.kept.parts.take());
        service.view.send_replace(View::default());
        drop(mount);
        tokio::task::spawn_blocking(move || drop(parts)).await.unwrap();
    });
}

/// The daemon's end is marked before anything of it is waited for, and nothing starts the
/// folder afterwards: a `Refresh()` leaves the sync as it was, not even refused.
#[tokio::test]
async fn nothing_starts_a_folder_after_the_daemons_end_reached_its_account() {
    let (service, _clock) = make_service();
    let folder = tempfile::tempdir().unwrap();
    stage_folder(&service, folder.path(), Sync::Stopped(Why::NotStarted), None).await;
    service.end_at_daemon_stop().await;
    assert!(service.refresh().await.is_err());
    assert!(read_sync(&service, |folder| matches!(folder.up().map(|up| &up.sync), Some(Sync::Stopped(Why::NotStarted)))).await, "no start was tried");
}

/// The mark stands also when the call of the daemon's end is cut before it has the folder's
/// lock: here a reader holds the lock, the call is cut while it waits, and a start made
/// afterwards starts nothing.
#[tokio::test]
async fn the_daemons_end_cut_before_it_has_the_lock_still_keeps_the_folder_from_starting() {
    let (service, _clock) = make_service();
    let folder = tempfile::tempdir().unwrap();
    stage_folder(&service, folder.path(), Sync::Stopped(Why::NotStarted), None).await;
    {
        let _held = service.folder.read().await;
        assert!(service.end_at_daemon_stop().now_or_never().is_none(), "it waits for the lock, and is cut");
    }
    assert!(service.refresh().await.is_err());
    assert!(read_sync(&service, |folder| matches!(folder.up().map(|up| &up.sync), Some(Sync::Stopped(Why::NotStarted)))).await);
}

#[tokio::test]
async fn the_second_door_changes_the_wanted_mode_and_publishes_it() {
    let (service, _clock) = make_service();
    let folder = tempfile::tempdir().unwrap();
    stage_folder(&service, folder.path(), Sync::Stopped(Why::NotStarted), None).await;
    assert_eq!(service.mode(), crate::config::Mode::ReadOnly);
    service.hold_folder().await.set_wanted(crate::config::Mode::ReadWrite);
    assert_eq!(service.mode(), crate::config::Mode::ReadWrite, "published when the door is let go of");
    assert!(read_sync(&service, |folder| matches!(folder.up().map(|up| &up.sync), Some(Sync::Stopped(Why::NotStarted)))).await, "the sync is not touched");
}
