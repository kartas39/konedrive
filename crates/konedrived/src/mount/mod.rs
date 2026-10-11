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
// Parts of it have no caller until the bodies of the mount over the rows are written.
#[allow(dead_code, unused_imports)]
mod backing_directory;
// No caller until the mount reads the rows: its tests are what runs it.
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod backing_directory_check;
mod errno;
mod range_readers;
mod stale_mount;
// TODO: check whether the tidy step after the daemon's end (`konedrived --tidy`) stays as it is
// with the mount over the rows, and fix if needed: `main` is its one caller.
mod tidy_step;
#[allow(dead_code, unused_imports)]
mod upload_rules;
mod write_operation_thread;
// The mount over the rows of `items`: no caller until the owner of a folder's state starts a
// sync through `sync_tasks`.
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod item_announcements;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod item_attributes;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod item_content_files;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod item_download_landing;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod item_download_states;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod item_downloader;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod item_files_interface;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod item_free_up;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod item_handles;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod item_inotify_bridge;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod item_kernel_requests;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod item_lookup;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod item_mount;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod item_range_fetcher;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod item_read_origin;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod item_write_operations;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod mount_take_down;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod place_changes;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod read_connections;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
mod start_check;
// The fixture the tests of the mount over the rows, the steps and the plan's task share.
#[cfg(test)]
pub(crate) mod item_fixture;

pub use access_mode_switch::AccessModeSwitch;
pub use backing_directory::{restore_write_bits, BackingDirectory};
pub use item_announcements::ItemAnnouncement;
pub use item_content_files::{ItemContentFiles, LocalContentFile};
pub use item_files_interface::{DownloadedSpace, FilesRefusal, FreedSpace, ItemFilesInterface, ItemPathStatus, PinHolder};
pub use item_free_up::FreeUpDecision;
pub use item_handles::ItemHandles;
pub use item_mount::{ItemMount, ItemMountError, ItemMountSetup};
pub use mount_take_down::{MountTakeDown, TakeDownCause, TakeDownReport};
pub use stale_mount::unmount_stale_mount;
pub use start_check::StartCheckFindings;
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
