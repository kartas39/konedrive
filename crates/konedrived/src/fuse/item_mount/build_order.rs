//! The order the parts of a mount over the rows are built in: the one place where a part
//! of the mount is made, each by its `new` with its final list of parameters.
//!
//! 1. a stale mount taken off the folder (`stale_mount`); `BackingDirectory::open`;
//!    `UploadRules::new(ignore)`;
//! 2. `AccessModeSwitch::new(ReadOnly)`; `MountTakeDown::new(switch, report)`;
//!    `WriteOperationThread::start(tree_lock)`; `PlaceChanges::new` with that thread's
//!    id, and `PlaceChanges::remove_aside_entries`: what a death left set aside goes;
//! 3. THE CHECK AT START (`start_check::check_backing_directory_at_start`), over the
//!    writer, before anything else reads the rows;
//! 4. `ReadConnections::new(store_path)`; `ItemHandles::new`;
//!    `ItemDownloadStates::new(download_after)`;
//! 5. `ItemInotifyBridge::new` (its queue; its thread starts when the mount is served); `RangeReaders` and `ItemRangeFetcher::new`;
//!    `ItemDownloadLanding::new`; `ItemDownloader::new`;
//! 6. `ItemFreeUp::new`; `ItemContentFiles::new`; `ItemLookup::new`;
//!    `ItemWriteOperations::new`; `FolderSteps::new`; `ItemFilesInterface::new`;
//! 7. `ItemKernelRequests::new`, which the switch's `Filesystem` is made over.
//!
//! **State owned:** none.
#![allow(dead_code, unused_variables)]

use std::sync::Arc;

use super::super::item_download_states::ItemDownloadStates;
use super::super::item_downloader::ItemDownloader;
use super::super::item_inotify_bridge::ItemInotifyBridge;
use super::super::item_kernel_requests::ItemKernelRequests;
use super::super::read_connections::ReadConnections;
use super::super::write_operation_thread::WriteOperationThread;
use super::{ItemMount, ItemMountError, ItemMountSetup};

/// The parts a stop of the mount reaches, in the order it stops them.
pub(super) struct PartsToStop {
    pub(super) write_operation_thread: Arc<WriteOperationThread>,
    pub(super) inotify_bridge: Arc<ItemInotifyBridge>,
    pub(super) downloader: Arc<ItemDownloader>,
    pub(super) download_states: Arc<ItemDownloadStates>,
    pub(super) read_connections: Arc<ReadConnections>,
}

/// Builds the parts in the order of the module's doc and answers the mount, not over the
/// folder, with what answers the kernel. Blocking (the check at start walks the backing
/// directory): called by `ItemMount::build` off the runtime, and by the fixture.
pub(in crate::fuse) fn build(setup: ItemMountSetup, runtime: tokio::runtime::Handle) -> Result<(ItemMount, ItemKernelRequests), ItemMountError> {
        todo!()
}
