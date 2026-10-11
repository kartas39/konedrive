use std::collections::BTreeMap;
use std::sync::Arc;

use futures_util::FutureExt;
use konedrive_tree::{Reason, WaitReason};

use super::*;
use crate::account_folder::folder::{Sync, View, Why};
use crate::account_folder::staged_folder::{make_kept_parts, make_service, publish_running_view, stage_folder};
use crate::mount::item_fixture::ItemFixture;
use crate::sync_parts::running_steps::{RunningStep, StepClass};

/// The scene's end: the view and the folder let go of the mount, which is dropped where a
/// thread may be joined.
async fn end_scene(service: &SyncService, mount: Arc<crate::mount::ItemMount>) {
    let kept = service.folder.write().await.record_mut().and_then(|record| record.kept.parts.take());
    service.view.send_replace(View::default());
    tokio::task::spawn_blocking(move || drop((kept, mount))).await.unwrap();
}

fn held_removal() -> ItemAnswer {
    ItemAnswer::Waiting(WaitReason::User(Reason::MassDelete))
}

/// With a running sync an answer of the bus is made over the last plan's answers and its
/// counts of held removals, both of ONE plan, the steps that run and the state of sending
/// now.
#[test]
fn the_sources_of_a_running_sync_are_the_plans_answers_with_its_counts_the_running_steps_and_the_holds() {
    let fixture = ItemFixture::new();
    let directory = fixture.show_onedrive_directory("docs");
    let file = fixture.show_onedrive_file("a.txt", b"content");
    fixture.runtime().block_on(async {
        let (service, _clock) = make_service();
        let kept = make_kept_parts(&fixture);
        let mount = Arc::clone(&kept.mount);
        publish_running_view(&service, &fixture, kept);

        let answers = BTreeMap::from([(directory.local_item_id, held_removal()), (file.local_item_id, ItemAnswer::Planned(StepClass::Content))]);
        let counts = BTreeMap::from([(directory.local_item_id, 12)]);
        fixture.published_plan.publish(answers.clone(), counts.clone());
        assert!(fixture.running_steps.begin(RunningStep { class: StepClass::Content, item: file.local_item_id, other_items: Vec::new(), destination: None, sent: 3, total: 7 }));
        fixture.sending_holds.set_paused(Some(None));

        let sources = service.read_queue_sources().expect("the folder's sync runs");
        assert_eq!((&sources.answers, &sources.held_removal_counts), (&answers, &counts));
        assert_eq!(sources.running.iter().map(|step| (step.item, step.sent, step.total)).collect::<Vec<_>>(), [(file.local_item_id, 3, 7)]);
        assert!(sources.conditions.is_sync_running);
        assert_eq!(sources.conditions.hold, Some(crate::sync_parts::sending_holds::SendingHold::Paused { until: None }));
        assert_eq!(sources.folder, fixture.folder());

        // The next plan replaces both in one publish: a reader has both of it.
        fixture.published_plan.publish(BTreeMap::new(), BTreeMap::new());
        let sources = service.read_queue_sources().unwrap();
        assert!(sources.answers.is_empty() && sources.held_removal_counts.is_empty());
        drop(sources);
        end_scene(&service, mount).await;
    });
}

/// With no running sync the store is read all the same, with no answer, no step and the
/// conditions of a stopped sync: what the last plan published is not taken for today's.
#[test]
fn the_sources_of_a_stopped_sync_are_its_store_alone() {
    let fixture = ItemFixture::new();
    let file = fixture.show_onedrive_file("a.txt", b"content");
    fixture.runtime().block_on(async {
        let (service, _clock) = make_service();
        let kept = make_kept_parts(&fixture);
        let mount = Arc::clone(&kept.mount);
        fixture.published_plan.publish(BTreeMap::from([(file.local_item_id, held_removal())]), BTreeMap::from([(file.local_item_id, 4)]));
        stage_folder(&service, &fixture.folder(), Sync::Stopped(Why::NotStarted), Some(kept)).await;

        let sources = service.read_queue_sources().expect("the folder keeps an open store");
        assert!(sources.answers.is_empty() && sources.held_removal_counts.is_empty() && sources.running.is_empty());
        assert_eq!(sources.conditions, upload_queue::describe_stopped_sync());
        drop(sources);
        // Nothing is held for a stopped sync, so nothing is confirmed or taken back; the plan is woken all the same.
        assert_eq!(service.confirm_deletes().await.unwrap(), 0);
        assert_eq!(service.restore_deletes().await.unwrap(), 0);
        assert!(service.outbox(0).await.unwrap().is_empty());
        end_scene(&service, mount).await;
    });
}

#[tokio::test]
async fn the_queue_is_refused_with_no_folder_and_with_no_open_store() {
    let (service, _clock) = make_service();
    assert!(matches!(service.outbox(0).await, Err(SyncError::NoRoot)));
    assert!(matches!(service.confirm_deletes().await, Err(SyncError::NoRoot)));

    let folder = tempfile::tempdir().unwrap();
    stage_folder(&service, folder.path(), Sync::Stopped(Why::CannotStart("the store is locked".into())), None).await;
    for refused in [service.outbox(0).await.map(drop), service.not_uploaded().await.map(drop), service.not_uploaded_summary().await.map(drop), service.restore_deletes().await.map(drop)] {
        assert!(matches!(&refused, Err(SyncError::NotUp(why)) if why.contains("the store is locked")), "{refused:?}");
    }
}

/// The user confirms a held removal: the confirmation is written and the plan's task woken.
#[test]
fn confirming_a_held_removal_writes_it_and_wakes_the_plan() {
    let fixture = ItemFixture::new();
    let file = fixture.show_onedrive_file("a.txt", b"content");
    fixture.remove_here(file.local_item_id).expect("a removal to send");
    fixture.runtime().block_on(async {
        let (service, _clock) = make_service();
        let kept = make_kept_parts(&fixture);
        let mount = Arc::clone(&kept.mount);
        publish_running_view(&service, &fixture, kept);
        fixture.published_plan.publish(BTreeMap::from([(file.local_item_id, held_removal())]), BTreeMap::new());
        // What woke the plan before the user's answer is taken.
        let _ = fixture.sync_plan_wake.wait().now_or_never();

        assert_eq!(service.confirm_deletes().await.unwrap(), 1);
        assert!(fixture.sync_plan_wake.wait().await.contains(&WakeCause::UserAnswered));
        end_scene(&service, mount).await;
    });
}

/// The coordinator's rule: the plan's task is woken also when the store answered an error —
/// it may have written some rows before it, each valid alone.
#[test]
fn the_plan_is_woken_also_when_confirming_or_taking_back_answers_an_error() {
    for is_confirmed in [true, false] {
        let fixture = ItemFixture::new();
        let file = fixture.show_onedrive_file("a.txt", b"content");
        fixture.remove_here(file.local_item_id).expect("a removal to send");
        fixture.runtime().block_on(async {
            let (service, _clock) = make_service();
            let kept = make_kept_parts(&fixture);
            let mount = Arc::clone(&kept.mount);
            publish_running_view(&service, &fixture, kept);
            fixture.published_plan.publish(BTreeMap::from([(file.local_item_id, held_removal())]), BTreeMap::new());
            let _ = fixture.sync_plan_wake.wait().now_or_never();
            // The steps' memory is gone from under the store: reading or writing it fails.
            fixture.write_rows(|store| store.bench_sql("DROP TABLE step_memory"));

            let answered = if is_confirmed { service.confirm_deletes().await } else { service.restore_deletes().await };
            assert!(matches!(answered, Err(SyncError::Store(_))), "confirmed {is_confirmed}: {answered:?}");
            assert!(fixture.sync_plan_wake.wait().await.contains(&WakeCause::UserAnswered), "confirmed {is_confirmed}");
            end_scene(&service, mount).await;
        });
    }
}
