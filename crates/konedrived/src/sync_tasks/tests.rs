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
pub(crate) fn make_start_input(fixture: &ItemFixture, drive: DriveClient) -> FolderStartInput {
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
pub(crate) async fn start_over_kept_parts(fixture: &ItemFixture, mut input: FolderStartInput) -> StartedFolder {
    input.kept = Some(make_kept_parts(fixture));
    match start_folder_sync(input).await {
        Ok(started) => started,
        Err(refused) => panic!("the start is refused: {:?}", refused.refusal),
    }
}

/// A folder whose sync RUNS over `fixture`, for the tests of the layers above this one
/// (the owner of the folder's state holds a `StartedFolder` in `Sync::Running`): the
/// start's stages 5 to 7 over the parts of the fixture's folder — its store and leaf
/// state, and a mount built now and NOT served, so nothing is mounted. Read-write, with
/// nothing held but the first listing, and no thumbnail cache.
///
/// **What runs,** on the runtime this is awaited on: the listing's poller (its first
/// cycle at once, then none for an hour unless asked: `tasks.poll().refresh()`) and the
/// plan's loop with the REAL runners of its steps — towards the folder over the built
/// mount, towards OneDrive through `drive`, of the rows. No thumbnail filler; no task
/// watches the account (the owner's does). The quota is read once from `drive` before
/// the tasks start.
///
/// **`drive`** is where every request goes: a fake OneDrive's client
/// (`fixture.start_fake_onedrive().await.client()`), with which a cycle ends and steps
/// send; or a client that reaches nothing, with which the first listing never ends, so
/// nothing is sent and only steps towards the folder and of the rows run.
///
/// **A start of another shape** (read-only wanted, holds that stand, a lease on the
/// test's own lock, a thumbnail cache): [`make_start_input`], change its fields, then
/// [`start_over_kept_parts`].
///
/// **At the end of the test the caller stops it** — [`stop`]`(started).await`, which
/// answers the parts the folder keeps — before the fixture is dropped: a sync that is
/// only dropped is told to stop and not waited for, and its tasks may still touch the
/// fixture's store and folder while the fixture goes. Whoever keeps the `StartedFolder`
/// inside a value of its own stops it by what that value's owner calls
/// (`stop_order::stop_folder_sync` after `tasks.stop_handle().cancel()`).
pub(crate) async fn start_sync_over_fixture(fixture: &ItemFixture, drive: DriveClient) -> StartedFolder {
    start_over_kept_parts(fixture, make_start_input(fixture, drive)).await
}

/// Stops a started sync as its owner does: the token first, then the stop.
pub(crate) async fn stop(started: StartedFolder) -> KeptFolderParts {
    started.tasks.stop_handle().cancel();
    stop_folder_sync(started).await
}

/// Waits until `is_so` answers yes, for ten seconds at most: for what a task of the sync
/// does by itself, which a test can only wait for.
pub(crate) async fn wait_until(what: &str, mut is_so: impl FnMut() -> bool) {
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
fn a_sync_started_over_the_fixture_runs_its_tasks_until_it_is_stopped() {
    let fixture = ItemFixture::new();
    let listed = fixture.list_onedrive_file("listed.txt", b"listed");
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;

        let started = start_sync_over_fixture(&fixture, graph.client()).await;

        assert!(started.mount.access_mode_switch().is_read_write());
        // The plan's loop runs with its real runners: what OneDrive has is shown. And the poller: its first cycle ends.
        wait_until("the listed file is shown", || fixture.find_item(listed.local_item_id).is_some_and(|item| item.local.is_some())).await;
        wait_until("the first listing ended", || started.tasks.sending_holds().find((fixture.clock())()).is_none()).await;
        let kept = stop(started).await;
        assert!(fixture.running_steps.list_steps().is_empty());
        drop(kept);
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
fn the_holder_hands_out_the_holds_the_poll_and_the_stops_token_of_its_tasks() {
    let fixture = ItemFixture::new();
    fixture.runtime().block_on(async {
        let graph = fixture.start_fake_onedrive().await;
        let started = start_over_kept_parts(&fixture, make_start_input(&fixture, graph.client())).await;

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
