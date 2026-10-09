//! A folder served as a FUSE mount: the one way the daemon shows OneDrive in a folder.
//!
//! The mount lies over the folder itself. Every name comes from the tree store; the backing
//! directory — the folder on disk — holds only the downloaded copies and what the user made
//! here, and is reached through the descriptor opened before the mount, never through the
//! mount. A file with no downloaded copy is read by fetching the bytes asked for from OneDrive
//! as a range, and once more than a threshold of it was read it is downloaded whole, into a
//! file that has no name until it is complete and checked, and is then given the file's.
//!
//! A read-only folder's mount refuses every change. A read-write one's takes them
//! ([`Mount::switch_to_read_write`]): each change a program asks for is made in the backing
//! directory and recorded in the outbox on one thread, under the folder's tree lock, before
//! the program is answered; the outbox worker sends what is recorded. The switch back to
//! read-only is made on that thread, so nothing is recorded in a folder that was switched to
//! read-only.
//!
//! - The kernel's requests read the store through a connection of their own, opened for
//!   reading alone: they never wait for the cycle that writes it.
//! - What changes in OneDrive is announced through the mount
//!   ([`CycleStep::announce_committed_cycle`]): the inotify bridge's thread creates, touches or
//!   removes the name by path, the mount answers it from the store, and the kernel tells
//!   whoever watches the directory.
//! - Pins are rows of the store ([`konedrive_tree::TreeStore::pins`]): a file that is not
//!   downloaded has nothing on disk to carry one.
//!
//! The mount is built of parts that own their state, each usable and tested without a mount;
//! [`Mount`] holds them, and calls them in order at its start, its stop and a switch of the
//! access mode (`mount/build_order.rs` has the order they are built in). What is decided is
//! decided by a function of typed cases, with no disk, store or mount in it, and performed
//! by the part around it. What each part is over is said with it; none is over the mount,
//! and no two are over each other:
//!
//! - The folder as it is: [`backing_directory`] (the directory on disk the mount lies over,
//!   by its descriptor) and [`unsent_changes`] (what was moved, removed or changed here and
//!   is not sent yet) depend on nothing; [`directory_contents`] (what each directory of the
//!   mount holds, each entry with its sync state) is over those two; [`node_table`] holds the
//!   node ids given to the kernel, and [`node_resolver`] is over the directory contents and
//!   owns the table; [`attributes`] is a function of an entry and the access mode; the
//!   values these pass around are a [`directory_reference`], an [`entry_place`] and a
//!   [`local_only_entry`].
//! - A change a program makes: [`write_operation_thread`] is the one thread such a change
//!   is made on, and depends on nothing; [`access_mode_switch`] holds whether programs may
//!   change the folder, and is switched to read-only on that thread; [`upload_rules`] (what is uploaded
//!   at all) and [`detections`] (what a change is recorded as, a function of typed events)
//!   depend on nothing but the store's facts; the [`outbox_recorder`] writes the detections
//!   and wakes the outbox's worker; [`open_handles`] holds what programs hold open, and
//!   alone says what is open for writing; [`write_operations`] are the operations
//!   themselves — each a lookup, a decision, the disk, a record — over the directory
//!   contents, the backing directory, the node resolver, the open handles, the recorder, the
//!   rules and the switch; the [`unrecorded_entry_scan`] finds what the outbox lost, over the
//!   directory contents, the backing directory, the rules, the recorder and the switch.
//! - A change from OneDrive: [`inotify_bridge`] brings it to whoever watches the folder, over
//!   the node resolver and the directory contents; what finds the user's own in its way is
//!   moved aside by [`conflict_copies`].
//! - A file that is not downloaded: [`range_readers`] holds the files being read as ranges,
//!   and the [`range_fetcher`] fetches the ranges, over it; [`download_states`] holds the
//!   state of each file's download and does no I/O; [`file_downloader`] downloads a file
//!   whole, over the backing directory, the directory contents, the download states and the
//!   bridge; [`state_attribute`] is what is shown of a file; [`pins`] holds the pins, over
//!   the directory contents, and the [`pinned_file_downloader`] downloads what they keep,
//!   over the pins, the directory contents and the file downloader; [`free_up_decision`] is
//!   whether a downloaded copy is freed.
//! - Who asks: [`cycle_step`] is the step of a cycle of the delta listing over the backing
//!   directory, over the backing directory, the directory contents, the switch, the range
//!   fetcher, the conflict copies, the recorder, the pins, the bridge and the pinned file
//!   downloader; [`kernel_requests`] answers the kernel, in five groups, each over the parts
//!   it uses; [`files_interface`] answers the daemon's bus interface `Files`, over the
//!   directory contents, the backing directory, the download states, the two downloaders,
//!   the open handles, the pins and the bridge.
//! - The mount itself: [`mount`] holds the session, the [`fuse_connection`] and the parts.
//! - After the daemon's end: [`stale_mount`] takes off the mount a daemon that died left, and
//!   [`tidy_step`] leaves each folder a plain directory without its write bits.

mod access_mode_switch;
mod attributes;
mod backing_directory;
mod conflict_copies;
mod cycle_step;
mod detections;
mod directory_contents;
mod directory_reference;
mod download_states;
mod entry_place;
mod errno;
mod file_downloader;
mod files_interface;
mod free_up_decision;
mod fuse_connection;
mod inotify_bridge;
mod kernel_requests;
mod local_only_entry;
mod mount;
mod node_resolver;
mod node_table;
mod open_handles;
mod outbox_recorder;
mod pinned_file_downloader;
mod pins;
mod range_fetcher;
mod range_readers;
mod stale_mount;
mod state_attribute;
mod tidy_step;
mod unrecorded_entry_scan;
mod unsent_changes;
mod upload_rules;
mod write_operation_thread;
mod write_operations;

pub use access_mode_switch::AccessModeSwitch;
pub use backing_directory::BackingDirectory;
pub use conflict_copies::{ConflictCopy, CycleConflicts};
pub use cycle_step::{CycleOutcome, CycleStep, OneDriveChange};
#[cfg(any(test, feature = "fault-injection"))]
pub use file_downloader::UnfinishedDownload;
pub use files_interface::{DownloadedCopies, FilesInterface, FilesRefusal, FreedSpace, PathStatus};
pub use free_up_decision::FreeUpDecision;
pub use mount::{Mount, MountSetup, WriteSetup};
pub use stale_mount::unmount_stale_mount;
pub use tidy_step::{list_directories, tidy_folder, tidy_served_folders};
