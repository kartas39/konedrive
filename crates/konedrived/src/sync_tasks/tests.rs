//! What the tests of a folder's start, stop and switch of the access mode share: the
//! input of a start and the parts of a folder over the fixture (`mount::item_fixture`),
//! whose mount is built and never served — a test mounts nothing —, and the tests of the
//! tasks' holder itself.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use konedrive_graph::drive::DriveClient;

use super::start_order::{start_folder_sync, FolderStartInput, KeptFolderParts, StandingSendingHolds, StartedFolder};
use super::stop_order::stop_folder_sync;
use crate::account::quota::Quota;
use crate::conditions::running::Running;
use crate::config::Mode;
use crate::folder::root::SyncRoot;
use crate::listing::listing_poller::Schedule;
use crate::mount::item_fixture::{ItemFixture, ROOT_ONEDRIVE_ID};
use crate::onedrive_steps::content_upload::Limits;
use crate::status::report::Report;
use crate::status::snapshot::{SyncSnapshot, SyncStateHandle};
use crate::sync_parts::lease::Lease;
use crate::sync_plan_task::plan_input::PlanSettings;

/// Longer than any test: a poller with this interval runs its first cycle and no other.
const LONG: Duration = Duration::from_secs(3600);

/// What a start over the fixture's folder is given: the fixture's store file, tree lock,
/// ignore list, pool and clock, `drive` for OneDrive, a lease on a lock nobody holds,
/// read-write wanted, nothing held, nothing kept, no thumbnail cache.
pub(super) fn make_start_input(fixture: &ItemFixture, drive: DriveClient) -> FolderStartInput {
    FolderStartInput {
        root: SyncRoot { path: fixture.folder(), root_id: ROOT_ONEDRIVE_ID.into() },
        tree_db: fixture.store_path.clone(),
        drive,
        recorded_drive: None,
        account_drive: None,
        drive_record: None,
        neighbours: None,
        source: fixture.content_source(),
        report: Report::nowhere(),
        pool: Arc::clone(&fixture.pool),
        split_download_share: Arc::clone(&fixture.split_download_share),
        download_after: crate::mount::item_fixture::DOWNLOAD_AFTER,
        tree_lock: Arc::clone(&fixture.tree_lock),
        lease: Lease::on(&Arc::new(tokio::sync::RwLock::new(()))),
        ignore: Arc::clone(&fixture.ignore),
        state: SyncStateHandle::new(SyncSnapshot::default()),
        quota: Quota::detached(),
        limits: Limits::default(),
        settings: PlanSettings::of("this-computer".into()),
        schedule: Schedule::polled(LONG, Vec::new()),
        running: Arc::new(Running::default()),
        thumbnail_cache: None,
        standing_holds: StandingSendingHolds::default(),
        clock: fixture.clock(),
        wanted: Mode::ReadWrite,
        take_down_report: fixture.take_down_report(),
        unsent_count_report: Arc::new(|_| {}),
        kept: None,
    }
}

/// The parts of the fixture's folder as a stopped sync keeps them: the fixture's store
/// and leaf state, and a mount built over them now and not served.
pub(super) fn make_kept_parts(fixture: &ItemFixture) -> KeptFolderParts {
    KeptFolderParts {
        store: fixture.store.clone(),
        mount: fixture.build_mount(),
        sync_plan_wake: Arc::clone(&fixture.sync_plan_wake),
        running_steps: Arc::clone(&fixture.running_steps),
        published_plan: Arc::clone(&fixture.published_plan),
        drive_id: fixture.drive_id(),
    }
}

/// Starts the sync of the fixture's folder over the parts it keeps, as `input` says.
pub(super) async fn start_over_kept_parts(fixture: &ItemFixture, mut input: FolderStartInput) -> StartedFolder {
    input.kept = Some(make_kept_parts(fixture));
    match start_folder_sync(input).await {
        Ok(started) => started,
        Err(refused) => panic!("the start is refused: {:?}", refused.refusal),
    }
}

/// Stops a started sync as its owner does: the token first, then the stop.
pub(super) async fn stop(started: StartedFolder) -> KeptFolderParts {
    started.tasks.stop_handle().cancel();
    stop_folder_sync(started).await
}

/// Waits until `is_so` answers yes, for ten seconds at most: for what a task of the sync
/// does by itself, which a test can only wait for.
pub(super) async fn wait_until(what: &str, mut is_so: impl FnMut() -> bool) {
    let waited = tokio::time::timeout(Duration::from_secs(10), async {
        while !is_so() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    waited.await.unwrap_or_else(|_| panic!("never: {what}"));
}

/// Runs `future` on a runtime of its own whose clock is paused and jumps to the next
/// timer when nothing else can run: for a bounded wait, which then costs no time.
pub(super) fn block_on_paused_clock<T>(future: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().expect("a runtime").block_on(future)
}

#[test]
fn a_folder_started_and_stopped_leaves_no_task_running() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let thumbnails = tempfile::tempdir().unwrap();
        let mut input = make_start_input(&fixture, graph.client());
        input.thumbnail_cache = Some(thumbnails.path().to_path_buf());
        let started = start_over_kept_parts(&fixture, input).await;
        let stop_handle = started.tasks.stop_handle();
        assert!(started.tasks.thumbnail_filler.is_some() && started.tasks.listing_poller.is_some());
        assert!(!started.tasks.plan_loop.is_finished(), "the plan's loop runs");

        let kept = stop(started).await;

        assert!(stop_handle.is_cancelled());
        // The parts are the ones the sync was started over, and the store answers.
        assert!(Arc::ptr_eq(&kept.sync_plan_wake, &fixture.sync_plan_wake) && Arc::ptr_eq(&kept.running_steps, &fixture.running_steps));
        assert!(kept.store.call(|store| store.drive_id()).await.is_ok());
        assert!(fixture.running_steps.list_steps().is_empty(), "no step is left running");
    });
}

#[test]
fn a_sync_dropped_unstopped_has_its_stop_token_cancelled() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let started = start_over_kept_parts(&fixture, make_start_input(&fixture, graph.client())).await;
        let stop_handle = started.tasks.stop_handle();
        let StartedFolder { tasks, .. } = started;
        assert!(!stop_handle.is_cancelled());

        drop(tasks);

        assert!(stop_handle.is_cancelled(), "every task is told to stop");
        // Told, the plan's loop ends by itself: nobody waited for it.
        wait_until("the dropped sync's steps are settled", || fixture.running_steps.list_steps().is_empty()).await;
    });
}

#[test]
fn the_holder_hands_out_the_holds_the_wake_the_poll_and_the_stops_token_of_its_tasks() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let started = start_over_kept_parts(&fixture, make_start_input(&fixture, graph.client())).await;

        assert!(Arc::ptr_eq(started.tasks.sync_plan_wake(), &fixture.sync_plan_wake), "the wake the mount was built with");
        assert!(Arc::ptr_eq(started.tasks.sending_holds(), &started.tasks.sending_holds));
        // The poll that is handed out reaches the poller: a refresh runs a cycle.
        wait_until("the first cycle asked for a page", || graph.with(|cloud| cloud.count("GET", "root/delta")) >= 1).await;
        wait_until("the first cycle ended", || started.tasks.sending_holds().find((fixture.clock())()).is_none()).await;
        let asked = graph.with(|cloud| cloud.count("GET", "root/delta"));
        started.tasks.poll().refresh();
        wait_until("the refresh ran a cycle", || graph.with(|cloud| cloud.count("GET", "root/delta")) > asked).await;

        stop(started).await;
    });
}
