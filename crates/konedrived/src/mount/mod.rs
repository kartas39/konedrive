//! A folder served as a FUSE mount over the rows of `items`: the one way the daemon shows
//! OneDrive in a folder.
//!
//! The mount lies over the folder itself. Every name comes from the tree store; the backing
//! directory — the folder on disk — holds only the downloaded copies and what the user made
//! here, and is reached through the descriptor opened before the mount, never through the
//! mount. A file with no downloaded copy is read by fetching the bytes asked for from OneDrive
//! as a range, and once more than a threshold of it was read it is downloaded whole.
//!
//! The mount is built of parts that own their state, each usable and tested without a mount;
//! [`ItemMount`] holds them (`item_mount/build_order.rs` has the order they are built in).
//! What is decided is decided by a function of typed cases, with no disk, store or mount in
//! it, and performed by the part around it:
//!
//! - The folder on disk: [`backing_directory`] (the directory the mount lies over, by its
//!   descriptor), [`place_changes`] (every change of a place in it, and every open of a
//!   backing file by the path of a row) and [`upload_rules`] (what is uploaded at all).
//! - Before the mount goes over the folder: [`start_check`] calls the
//!   [`backing_directory_check`], which makes the rows say what the backing directory has.
//! - What the kernel asks: [`item_kernel_requests`], answered from the rows by
//!   [`item_lookup`] on the asking thread's own connection ([`read_connections`]), with
//!   [`item_attributes`] and [`errno`]; what programs hold open is [`item_handles`]'.
//! - A change a program makes: [`item_write_operations`], on the one
//!   [`write_operation_thread`], while the [`access_mode_switch`] says the folder takes
//!   changes.
//! - A file that is not on disk: [`item_read_origin`] says where its bytes are read from,
//!   [`item_range_fetcher`] fetches ranges ([`range_readers`] holds the files being read so),
//!   [`item_downloader`] downloads a file whole, by [`item_download_states`], and
//!   [`item_download_landing`] lands it; [`item_free_up`] removes a downloaded copy.
//! - Who else asks: [`item_files_interface`] answers the bus interface `Files`;
//!   [`item_content_files`] gives a step towards OneDrive the local content;
//!   `crate::folder_steps` runs the sync plan's steps towards the folder, made of the parts
//!   [`ItemMount::folder_step_parts`] hands out; what a step changed is
//!   told to whoever watches the folder by the [`item_inotify_bridge`]
//!   ([`item_announcements`]).
//! - The mount's end: [`mount_take_down`] is how a part says the mount must be taken down;
//!   [`stale_mount`] takes off the mount a daemon that died left, and [`tidy_step`] leaves
//!   each folder a plain directory without its write bits after the daemon's end.

mod access_mode_switch;
// Some of what it holds is called by its tests alone, or by nothing.
#[allow(dead_code, unused_imports)]
mod backing_directory;
// Some of what it holds is called by its tests alone.
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod backing_directory_check;
mod errno;
mod range_readers;
mod stale_mount;
// The tidy step after the daemon's end (`konedrived --tidy`, the unit's `ExecStopPost`): it reads
// the kernel's table of mounts and the folder on disk and nothing of the rows, so it is the same
// for whichever mount was served.
mod tidy_step;
#[allow(dead_code, unused_imports)]
mod upload_rules;
mod write_operation_thread;
mod item_announcements;
mod fuse_connection;
mod item_attributes;
mod item_content_files;
mod item_download_landing;
mod item_download_states;
mod item_downloader;
mod item_files_interface;
mod item_free_up;
mod item_handles;
mod item_inotify_bridge;
mod item_kernel_requests;
// Some of what it holds is called by its tests alone.
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod item_lookup;
mod item_mount;
mod item_range_fetcher;
mod item_read_origin;
// Some of what it holds is called by its tests alone.
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod item_write_operations;
mod kernel_requests;
mod mount_take_down;
mod place_changes;
mod read_connections;
mod start_check;
// The fixture the tests of the mount over the rows, the steps and the plan's task share.
#[cfg(test)]
pub(crate) mod item_fixture;

pub(crate) use backing_directory::restore_write_bits;
pub(crate) use item_announcements::ItemAnnouncement;
pub(crate) use item_content_files::{ItemContentFiles, LocalContentFile};
pub(crate) use item_files_interface::FilesRefusal;
pub(crate) use item_free_up::FreeUpDecision;
pub(crate) use item_handles::ItemHandles;
pub(crate) use item_mount::{ItemMount, ItemMountError, ItemMountSetup};
pub(crate) use mount_take_down::{MountTakeDown, TakeDownCause, TakeDownReport};
pub(crate) use stale_mount::unmount_stale_mount;
pub(crate) use start_check::StartCheckFindings;
pub use tidy_step::{list_directories, tidy_folder, tidy_served_folders};

// What the runner of the plan's steps towards the folder (`crate::folder_steps`) is made of and
// calls: the mount's parts are its own, and these names are all another directory sees of them.
pub(crate) use backing_directory::EntryKind;
pub(crate) use item_announcements::describe_move;
pub(crate) use item_download_landing::ItemLandingOutcome;
pub(crate) use item_download_states::ItemDownloadStates;
pub(crate) use item_downloader::{DownloadRefusal, ItemDownloader, ItemToDownload};
pub(crate) use item_inotify_bridge::ItemInotifyBridge;
pub(crate) use item_mount::FolderStepParts;
pub(crate) use item_range_fetcher::ItemRangeFetcher;
pub(crate) use item_read_origin::RangeSource;
pub(crate) use place_changes::{DirectoryMarks, PlaceChange, PlaceChanges, PlaceRefusal, PlaceUndo, RowPathStat};
pub(crate) use upload_rules::UploadRules;
pub(crate) use write_operation_thread::WriteOperationThread;
// And what its tests make and ask beside the fixture.
#[cfg(test)]
pub(crate) use item_download_landing::ItemDownloadLanding;
#[cfg(test)]
pub(crate) use item_download_states::DownloadKey;
