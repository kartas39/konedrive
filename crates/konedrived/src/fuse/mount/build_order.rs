//! The order the parts of a mount are built in. Each is given only what stands before it:
//! no part is handed one that is built after it, and none is handed the mount.
//!
//! 1. [`BackingDirectory`]: opened before the mount lies over it;
//! 2. [`DirectoryContents`]: over the store's reader and the backing directory;
//! 3. [`NodeResolver`]: over the directory contents and the backing directory;
//! 4. [`AccessModeSwitch`], read-only at first;
//! 5. [`WriteOperationThread`], over the tree lock, and [`OutboxRecorder`], over the store's
//!    writer: neither is given a part;
//! 6. [`UploadRules`], then [`ConflictCopies`]: the backing directory, the directory
//!    contents, the rules;
//! 7. [`OpenHandles`], which depends on no part; [`WriteOperations`]: the directory
//!    contents, the backing directory, the node resolver, the open handles, the recorder, the
//!    rules, the access mode switch; [`UnrecordedEntryScan`]: the directory contents, the
//!    backing directory, the rules, the recorder, the switch;
//! 8. [`DownloadStates`], which depends on no part, and [`Pins`] over the directory contents;
//! 9. [`InotifyBridge`]: the node resolver and the directory contents — its thread is not
//!    started yet;
//! 10. [`FileDownloader`]: the backing directory, the directory contents, the download
//!     states, the bridge; then [`PinnedFileDownloader`]: the pins, the directory contents,
//!     the file downloader;
//! 11. [`RangeReaders`] and [`InterruptibleReads`], which depend on no part, and
//!     [`RangeFetcher`] over the range readers;
//! 12. who asks, each given exactly the parts it uses: [`CycleStep`], [`FilesInterface`], and
//!     the five groups of the kernel's requests, which [`KernelRequests`] holds.
//!
//! Nothing here mounts: what is built answers a cycle, the `Files` interface and a write
//! operation without a mount over the folder.

use std::io;
use std::sync::{Arc, Mutex};

use konedrive_tree::TreeStore;

use super::mount_setup::MountSetup;
use super::{to_io_error, Mount};
use crate::fuse::access_mode_switch::{AccessMode, AccessModeSwitch};
use crate::fuse::backing_directory::BackingDirectory;
use crate::fuse::conflict_copies::ConflictCopies;
use crate::fuse::cycle_step::CycleStep;
use crate::fuse::directory_contents::DirectoryContents;
use crate::fuse::download_states::DownloadStates;
use crate::fuse::file_downloader::FileDownloader;
use crate::fuse::files_interface::FilesInterface;
use crate::fuse::inotify_bridge::InotifyBridge;
use crate::fuse::kernel_requests::{DirectoryRequests, InterruptibleReads, KernelRequests, LookupRequests, ReadRequests, WriteRequests, XattrRequests};
use crate::fuse::node_resolver::NodeResolver;
use crate::fuse::open_handles::OpenHandles;
use crate::fuse::outbox_recorder::OutboxRecorder;
use crate::fuse::pinned_file_downloader::PinnedFileDownloader;
use crate::fuse::pins::Pins;
use crate::fuse::range_fetcher::RangeFetcher;
use crate::fuse::range_readers::RangeReaders;
use crate::fuse::unrecorded_entry_scan::UnrecordedEntryScan;
use crate::fuse::upload_rules::UploadRules;
use crate::fuse::write_operation_thread::WriteOperationThread;
use crate::fuse::write_operations::WriteOperations;

impl Mount {
    /// Builds the parts of the mount of `setup.root`, whose store has a pin on `pinned_ids`,
    /// in the order above; what waits or sweeps does so on `runtime`. The mount that holds
    /// them, which lies over nothing yet, and what answers the kernel once it does.
    pub(in crate::fuse) fn build(setup: MountSetup, pinned_ids: Vec<String>, runtime: tokio::runtime::Handle) -> io::Result<(Mount, KernelRequests)> {
        let MountSetup { root, store_path, store, source, report, pool, split_download_share, download_after, write_setup } = setup;
        let folder = root.path;
        let backing_directory = Arc::new(BackingDirectory::open(&folder)?);
        let directory_contents = Arc::new(DirectoryContents::new(TreeStore::open_read_only(&store_path).map_err(to_io_error)?, Arc::clone(&backing_directory)));
        let node_resolver = Arc::new(NodeResolver::new(Arc::clone(&directory_contents), Arc::clone(&backing_directory)));
        let access_mode_switch = Arc::new(AccessModeSwitch::new(AccessMode::ReadOnly));
        let write_operation_thread = Arc::new(WriteOperationThread::start(write_setup.tree_lock)?);
        let outbox_recorder = Arc::new(OutboxRecorder::new(store.clone()));
        let upload_rules = Arc::new(UploadRules::new(write_setup.ignore));
        let conflict_copies = ConflictCopies::new(Arc::clone(&backing_directory), Arc::clone(&directory_contents), Arc::clone(&upload_rules), write_setup.machine_name);
        let open_handles = Arc::new(OpenHandles::default());
        let write_operations = Arc::new(WriteOperations::new(
            Arc::clone(&directory_contents),
            Arc::clone(&backing_directory),
            Arc::clone(&node_resolver),
            Arc::clone(&open_handles),
            Arc::clone(&outbox_recorder),
            Arc::clone(&upload_rules),
            Arc::clone(&access_mode_switch),
        ));
        let unrecorded_entry_scan = Arc::new(UnrecordedEntryScan::new(Arc::clone(&directory_contents), Arc::clone(&backing_directory), upload_rules, Arc::clone(&outbox_recorder), Arc::clone(&access_mode_switch)));
        let download_states = Arc::new(DownloadStates::new(download_after));
        let pins = Arc::new(Pins::new(pinned_ids, store, Arc::clone(&directory_contents)));
        let inotify_bridge = Arc::new(InotifyBridge::new(folder.clone(), Arc::clone(&node_resolver), Arc::clone(&directory_contents)));
        let file_downloader = Arc::new(FileDownloader::new(
            Arc::clone(&backing_directory),
            Arc::clone(&directory_contents),
            Arc::clone(&download_states),
            Arc::clone(&inotify_bridge),
            Arc::clone(&source),
            Arc::clone(&pool),
            split_download_share,
            report.clone(),
            folder.clone(),
            runtime.clone(),
        ));
        let pinned_file_downloader = Arc::new(PinnedFileDownloader::new(Arc::clone(&pins), Arc::clone(&directory_contents), Arc::clone(&file_downloader), runtime.clone()));
        let range_readers = Arc::new(RangeReaders::new(pool, runtime.clone()));
        let range_fetcher = Arc::new(RangeFetcher::new(range_readers, source));
        let interruptible_reads = Arc::new(InterruptibleReads::default());
        let cycle_step = Arc::new(CycleStep::new(
            Arc::clone(&backing_directory),
            Arc::clone(&directory_contents),
            Arc::clone(&access_mode_switch),
            Arc::clone(&range_fetcher),
            conflict_copies,
            Arc::clone(&outbox_recorder),
            Arc::clone(&pins),
            Arc::clone(&inotify_bridge),
            Arc::clone(&pinned_file_downloader),
        ));
        let files_interface = FilesInterface::new(
            folder.clone(),
            Arc::clone(&directory_contents),
            Arc::clone(&backing_directory),
            Arc::clone(&download_states),
            Arc::clone(&file_downloader),
            Arc::clone(&open_handles),
            Arc::clone(&pins),
            Arc::clone(&inotify_bridge),
            Arc::clone(&pinned_file_downloader),
            report,
        );
        let kernel_requests = KernelRequests::new(
            LookupRequests::new(Arc::clone(&node_resolver), Arc::clone(&directory_contents), Arc::clone(&access_mode_switch), Arc::clone(&inotify_bridge), Arc::clone(&backing_directory)),
            DirectoryRequests::new(Arc::clone(&node_resolver), Arc::clone(&directory_contents), Arc::clone(&open_handles), Arc::clone(&access_mode_switch)),
            XattrRequests::new(Arc::clone(&node_resolver), Arc::clone(&backing_directory), Arc::clone(&download_states), pins),
            ReadRequests::new(
                Arc::clone(&node_resolver),
                directory_contents,
                Arc::clone(&backing_directory),
                Arc::clone(&open_handles),
                Arc::clone(&download_states),
                range_fetcher,
                Arc::clone(&file_downloader),
                Arc::clone(&interruptible_reads),
                runtime.clone(),
            ),
            WriteRequests::new(write_operations, Arc::clone(&write_operation_thread), Arc::clone(&access_mode_switch), node_resolver, open_handles, file_downloader, runtime),
            Arc::clone(&inotify_bridge),
        );
        let mount = Mount {
            folder,
            backing_directory,
            access_mode_switch,
            write_operation_thread,
            outbox_recorder,
            unrecorded_entry_scan,
            cycle_step,
            files_interface,
            inotify_bridge,
            download_states,
            interruptible_reads,
            pinned_file_downloader,
            session: Mutex::new(None),
            fuse_connection: None,
        };
        Ok((mount, kernel_requests))
    }
}
