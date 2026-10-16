use std::collections::BTreeMap;
use std::sync::Arc;

use konedrive_tree::{Reason, WaitReason};

use super::*;
use crate::account_folder::folder::{Sync, View, Why};
use crate::account_folder::staged_folder::{make_kept_parts, make_service, publish_running_view, stage_folder, the_cause};
use crate::mount::item_fixture::ItemFixture;
use crate::sync_parts::running_steps::{RunningStep, StepClass};

/// The scene's end: the view and the folder let go of the mount, which is dropped where a
/// thread may be joined.
async fn end_scene(service: &SyncService, mount: Arc<crate::mount::ItemMount>) {
    let kept = service.folder.write().await.record_mut().and_then(|record| record.kept.parts.take());
    service.view.send_replace(View::default());
    tokio::task::spawn_blocking(move || drop((kept, mount))).await.unwrap();
}

fn waiting_for_the_user() -> ItemAnswer {
    ItemAnswer::Waiting(WaitReason::User(Reason::NameCharacters))
}

/// With a running sync an answer of the bus is made over the last plan's answers, the
/// steps that run and the state of sending now.
#[test]
fn the_sources_of_a_running_sync_are_the_plans_answers_the_running_steps_and_the_holds() {
    let fixture = ItemFixture::new();
    let directory = fixture.show_onedrive_directory("docs");
    let file = fixture.show_onedrive_file("a.txt", b"content");
    fixture.runtime().block_on(async {
        let (service, _clock) = make_service();
        let kept = make_kept_parts(&fixture);
        let mount = Arc::clone(&kept.mount);
        publish_running_view(&service, &fixture, kept);

        let answers = BTreeMap::from([(directory.local_item_id, waiting_for_the_user()), (file.local_item_id, ItemAnswer::Planned(StepClass::Content))]);
        fixture.published_plan.publish(answers.clone());
        assert!(fixture.running_steps.begin(RunningStep { class: StepClass::Content, item: file.local_item_id, other_items: Vec::new(), destination: None, sent: 3, total: 7 }));
        fixture.sending_holds.set_paused(Some(None));

        let sources = service.read_queue_sources().expect("the folder's sync runs");
        assert_eq!(sources.answers, answers);
        assert_eq!(sources.running.iter().map(|step| (step.item, step.sent, step.total)).collect::<Vec<_>>(), [(file.local_item_id, 3, 7)]);
        assert!(sources.conditions.is_sync_running);
        assert_eq!(sources.conditions.hold, Some(crate::sync_parts::sending_holds::SendingHold::Paused { until: None }));
        assert_eq!(sources.folder, fixture.folder());

        // The next plan replaces them.
        fixture.published_plan.publish(BTreeMap::new());
        let sources = service.read_queue_sources().unwrap();
        assert!(sources.answers.is_empty());
        drop(sources);
        end_scene(&service, mount).await;
    });
}

/// With no running sync the store is read all the same, with no answer, no step and the
/// conditions of a stopped sync: what the last plan published is not taken for what holds now.
#[test]
fn the_sources_of_a_stopped_sync_are_its_store_alone() {
    let fixture = ItemFixture::new();
    let file = fixture.show_onedrive_file("a.txt", b"content");
    fixture.runtime().block_on(async {
        let (service, _clock) = make_service();
        let kept = make_kept_parts(&fixture);
        let mount = Arc::clone(&kept.mount);
        fixture.published_plan.publish(BTreeMap::from([(file.local_item_id, waiting_for_the_user())]));
        stage_folder(&service, &fixture.folder(), Sync::Stopped(Why::NotStarted), Some(kept)).await;

        let sources = service.read_queue_sources().expect("the folder keeps an open store");
        assert!(sources.answers.is_empty() && sources.running.is_empty());
        assert_eq!(sources.conditions, upload_queue::describe_stopped_sync());
        drop(sources);
        assert!(service.upload_queue(0).await.unwrap().is_empty());
        end_scene(&service, mount).await;
    });
}

#[tokio::test]
async fn the_queue_is_refused_with_no_folder_and_with_no_open_store() {
    let (service, _clock) = make_service();
    assert!(matches!(service.upload_queue(0).await, Err(SyncError::NoRoot)));

    let folder = tempfile::tempdir().unwrap();
    stage_folder(&service, folder.path(), Sync::Stopped(Why::CannotStart(the_cause())), None).await;
    for refused in [service.upload_queue(0).await.map(drop), service.not_uploaded().await.map(drop), service.not_uploaded_summary().await.map(drop)] {
        assert!(matches!(&refused, Err(SyncError::NotUp(why)) if why.contains("the cause")), "{refused:?}");
    }
}
