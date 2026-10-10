//! The one fixture the tests of the mount over the rows, of the steps and of the plan's
//! task share: a store in memory or in a temporary directory, a temporary backing
//! directory, the leaf state, and the fake OneDrive listed into `items`.
//!
//! Test code. Filled in with step 0, before any piece, so that no piece builds its own
//! "store + backing directory + fake OneDrive": each piece's tests make this fixture and
//! the ONE part they test, and reach every neighbour through the fixture's finished leaf
//! state (`SyncPlanWake`, `PublishedPlan`, `RunningSteps`, `SendingHolds`,
//! `OneDriveSpace`, `MountTakeDown`, `ItemHandles`, `ReadConnections`, `PlaceChanges`,
//! `ItemDownloadStates`), never through another piece's unfinished part. What a piece
//! outside `fuse` cannot make itself the fixture makes for it: a mount that is built and
//! not served (`build_mount`: its `FolderSteps`, `ItemContentFiles`, `ItemFilesInterface`
//! through the accessors), a content source and a clock the test holds, a `GraphAccess`
//! over the fake OneDrive, a poll handle that counts what it was asked, and a stand-in
//! for the runners of steps.
#![allow(dead_code, unused_variables)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_tree::{ContentVersion, Item, ItemVersions, LocalItemId, Store};

use super::item_mount::ItemMount;
use crate::hydration::source::ContentSource;
use crate::remote::listing_poller::ListingPollHandle;
use crate::sync::sync_plan_task::step_runners::StepRunners;
use crate::upload::onedrive_space::OneDriveSpace;
use crate::upload::onedrive_steps::GraphAccess;
use crate::upload::sending_holds::SendingHolds;

use super::backing_directory::BackingDirectory;
use super::item_download_states::ItemDownloadStates;
use super::item_handles::ItemHandles;
use super::mount_take_down::{MountTakeDown, TakeDownCause};
use super::place_changes::PlaceChanges;
use super::read_connections::ReadConnections;
use super::upload_rules::UploadRules;
use super::write_operation_thread::WriteOperationThread;
use crate::folder::published_plan::PublishedPlan;
use crate::folder::running_steps::RunningSteps;
use crate::folder::sync_plan_wake::SyncPlanWake;

/// A folder with no mount, no kernel and no daemon.
pub(crate) struct ItemFixture {
    pub(crate) directory: tempfile::TempDir,
    pub(crate) store: Store,
    pub(crate) store_path: PathBuf,
    pub(crate) tree_lock: Arc<tokio::sync::Mutex<()>>,
    pub(in crate::fuse) backing_directory: Arc<BackingDirectory>,
    pub(in crate::fuse) upload_rules: Arc<UploadRules>,
    pub(in crate::fuse) write_operation_thread: Arc<WriteOperationThread>,
    pub(in crate::fuse) place_changes: Arc<PlaceChanges>,
    pub(in crate::fuse) read_connections: Arc<ReadConnections>,
    pub(in crate::fuse) download_states: Arc<ItemDownloadStates>,
    pub(crate) item_handles: Arc<ItemHandles>,
    pub(crate) mount_take_down: Arc<MountTakeDown>,
    pub(crate) sync_plan_wake: Arc<SyncPlanWake>,
    pub(crate) running_steps: Arc<RunningSteps>,
    pub(crate) published_plan: Arc<PublishedPlan>,
    pub(crate) sending_holds: Arc<SendingHolds>,
    pub(crate) onedrive_space: Arc<OneDriveSpace>,
    /// Unix seconds: what `clock()` answers; the test sets it.
    pub(crate) now: Arc<std::sync::atomic::AtomicI64>,
    /// The take-downs asked for, as the report was given them.
    pub(crate) take_downs: Arc<std::sync::Mutex<Vec<TakeDownCause>>>,
}

impl ItemFixture {
    /// A folder whose store knows its drive and whose root is bound, with nothing in it.
    pub(crate) fn new() -> Self {
        todo!()
    }

    /// Lists a file of OneDrive at `path` with `content` into the rows (a full listing of
    /// one page through `write_listed_changes`) and shows it (`commit_show_item`): not on
    /// disk. Its row.
    pub(crate) fn show_onedrive_file(&self, path: &str, content: &[u8]) -> Item {
        todo!()
    }

    /// As [`show_onedrive_file`](Self::show_onedrive_file) for a directory.
    pub(crate) fn show_onedrive_directory(&self, path: &str) -> Item {
        todo!()
    }

    /// Puts the whole copy of the shown file `item` on disk, marked, and says so in its
    /// row, as a landing does.
    pub(crate) fn land_copy(&self, item: LocalItemId, content: &[u8]) {
        todo!()
    }

    /// Makes a file of the user's own at `path` on disk and in the rows, as the mount's
    /// `make` and a close do. Its row.
    pub(crate) fn make_own_file(&self, path: &str, content: &[u8]) -> Item {
        todo!()
    }

    /// The row of `item` now.
    pub(crate) fn find_item(&self, item: LocalItemId) -> Option<Item> {
        todo!()
    }

    /// The versions of `item` now, as a step planned at this moment carries them.
    pub(crate) fn versions_of(&self, item: LocalItemId) -> ItemVersions {
        todo!()
    }

    /// The path in the backing directory of `path` of the folder.
    pub(crate) fn disk_path(&self, path: &str) -> PathBuf {
        todo!()
    }

    /// The content version OneDrive would tell for `content`: its size, a fixed time, its
    /// QuickXorHash and a cTag made of it.
    pub(crate) fn onedrive_content_of(content: &[u8]) -> ContentVersion {
        todo!()
    }

    /// The fake OneDrive (wiremock) with the drive the fixture's store knows, holding
    /// what the rows say OneDrive has: for the steps towards OneDrive and the listing.
    pub(crate) async fn start_fake_onedrive(&self) -> wiremock::MockServer {
        todo!()
    }

    /// The clock every part is made with: answers `now`.
    pub(crate) fn clock(&self) -> Arc<dyn Fn() -> i64 + Send + Sync> {
        todo!()
    }

    /// A mount over the fixture's folder, built through `build_order::build` with the
    /// fixture's store, leaf state and content source, and NOT served: its accessors give
    /// the parts a piece outside `fuse` is tested with or over.
    pub(crate) fn build_mount(&self) -> Arc<ItemMount> {
        todo!()
    }

    /// A content source that serves the bytes the test gave for each item id
    /// (`serve_content`), and fails, throttles or says signed out as the test asks.
    pub(crate) fn content_source(&self) -> Arc<dyn ContentSource> {
        todo!()
    }

    /// The content source serves `content` for the OneDrive item of `item` from now on.
    pub(crate) fn serve_content(&self, item: LocalItemId, content: &[u8]) {
        todo!()
    }

    /// What every part that asks Graph is made with, over `fake_onedrive`, with the
    /// fixture's store, drive and clock.
    pub(crate) fn graph_access(&self, fake_onedrive: &wiremock::MockServer) -> GraphAccess {
        todo!()
    }

    /// A poll handle with no poller behind it (`ListingPollHandle::for_tests`), and how
    /// many cycles it was asked for.
    pub(crate) fn poll_handle(&self) -> (ListingPollHandle, Arc<std::sync::atomic::AtomicUsize>) {
        todo!()
    }

    /// A stand-in for the runners of steps, as the plan's task takes them
    /// (`sync_plan_task::step_runners::StepRunners`): each step is committed by its own
    /// commit of `step_commit`, with answers made up as OneDrive would give them (a new
    /// id, a fresh eTag, the content sent), and nothing on disk. Written here, in the
    /// fixture; the convergence test's stand-in in `konedrive-tree` is its model, not its
    /// code.
    pub(crate) fn step_runners_stand_in(&self) -> StepRunners {
        todo!()
    }

    /// Asserts that the store's rules of the rows hold (`find_broken_rule`) and that every
    /// row that says on disk has its file at the path of its local version.
    pub(crate) fn assert_rows_and_disk_agree(&self, at: &Path) {
        todo!()
    }
}
