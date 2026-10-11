//! Test code: an account's folder put into a state by hand, with no real start of its sync
//! and no mount over a directory — what the tests of the owner's files stage their scenes
//! with.
//!
//! A real start (`start_order::start_folder_sync`) mounts the folder, which no test does.
//! So a scene is staged in two ways: the folder's state is written as a change of the
//! folder would leave it ([`stage_folder`]) — a service made with no drive then refuses
//! every start before anything is opened, which is the refusal the tests of the retries
//! count on —, and the view the readers of the bus read is published with the handles of a
//! running sync over the leaves of the mount's fixture ([`publish_running_view`]).

use std::path::Path;
use std::sync::Arc;

use super::folder::{Folder, Is, Kept, Record, Sync, Up, View};
use super::testing::{wiring, ManualClock};
use super::SyncService;
use crate::folder::root::SyncRoot;
use crate::mount::item_fixture::ItemFixture;
use crate::sync_tasks::start_order::KeptFolderParts;

/// The time a scene starts at, Unix seconds.
pub(super) const START: i64 = 1_700_000_000;

/// A service with no drive, on a clock the test moves: every start of its folder is
/// refused before anything is opened.
pub(super) fn make_service() -> (Arc<SyncService>, Arc<ManualClock>) {
    let clock = ManualClock::at(START);
    (wiring().clock(&clock).build(), clock)
}

/// The record of a folder at `path`, with what a stopped sync kept (`parts`).
pub(super) fn make_record(path: &Path, parts: Option<KeptFolderParts>) -> Record {
    Record { root: SyncRoot { path: path.to_owned(), root_id: "root-id".into() }, baloo: false, kept: Kept { parts, ..Kept::default() } }
}

/// Puts the folder of `service` up at `path` with `sync`, keeping `parts`, and publishes
/// it: as a change of the folder would leave it.
pub(super) async fn stage_folder(service: &SyncService, path: &Path, sync: Sync, parts: Option<KeptFolderParts>) {
    let mut folder = service.folder.write().await;
    folder.is = Is::Up(Up { record: make_record(path, parts), sync, take_down_try: None });
    service.publish(&folder, |_| {});
}

/// The parts a stopped sync keeps, over `fixture`: a mount that is built and not served,
/// with the fixture's store and the leaves the mount was built with.
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

/// Publishes the view of a folder whose sync RUNS, over `fixture`'s leaves: the parts, what
/// holds sending, the space and a poll that counts what it is asked. No task runs behind
/// it. The count of the cycles the poll was asked for.
pub(super) fn publish_running_view(service: &SyncService, fixture: &ItemFixture, parts: KeptFolderParts) -> Arc<std::sync::atomic::AtomicUsize> {
    let (poll, asked_cycle_count) = fixture.poll_handle();
    let record = make_record(&fixture.folder(), None);
    service.view.send_replace(View {
        tree_lock: Some(Arc::clone(&fixture.tree_lock)),
        record: Some(record),
        parts: Some(parts),
        poll: Some(poll),
        sending_holds: Some(Arc::clone(&fixture.sending_holds)),
        onedrive_space: Some(Arc::clone(&fixture.onedrive_space)),
        sync: super::folder::SyncView::Running(tokio_util::sync::CancellationToken::new()),
        ..View::default()
    });
    asked_cycle_count
}

/// What the folder's sync is now, read under the folder's lock: `None` for a folder that
/// is not up.
pub(super) async fn read_sync<T>(service: &SyncService, read: impl FnOnce(&Folder) -> T) -> T {
    read(&*service.folder.read().await)
}

/// Waits until the folder's state is as `is_reached` says, looking again at every publish
/// of the view: by events, not by time.
pub(super) async fn wait_for_folder(service: &SyncService, mut is_reached: impl FnMut(&Folder) -> bool) {
    let mut published = service.view.subscribe();
    loop {
        published.borrow_and_update();
        if is_reached(&*service.folder.read().await) {
            return;
        }
        published.changed().await.expect("the service lives");
    }
}
