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
//! directory and recorded in the outbox by one thread, under the folder's tree lock, before
//! the program is answered; the outbox worker sends what is recorded.
//!
//! - The kernel's requests read the store through a connection of their own, opened for
//!   reading alone: they never wait for the cycle that writes it.
//! - What changes in OneDrive is announced through the mount
//!   ([`Mount::announce_committed_cycle`]): the inotify bridge's thread creates, touches or
//!   removes the name by path, the mount answers it from the store, and the kernel tells
//!   whoever watches the directory.
//! - Pins are rows of the store ([`konedrive_tree::TreeStore::pins`]): a file that is not
//!   downloaded has nothing on disk to carry one.
//!
//! The mount is built of parts that own their state, each usable and tested without a mount;
//! [`Mount`] holds them and hands on what it is asked ([`mount`] has the order they are built
//! in).
//!
//! - The folder as it is: [`backing_directory`] (the directory on disk the mount lies over,
//!   by its descriptor) and [`unsent_changes`] (what was moved, removed or changed here and
//!   is not sent yet) depend on nothing; [`directory_contents`] (what each directory of the
//!   mount holds, each entry with its sync state) is over those two; [`node_table`] holds the
//!   node ids given to the kernel, and [`node_resolver`] is over the directory contents and
//!   owns the table; [`access_mode`] holds whether programs may change the folder, and
//!   [`attributes`] is a function of an entry and the access mode.
//! - A change a program makes: [`upload_rules`] (what is uploaded at all) and [`detections`]
//!   (what a change is recorded as, a function of typed events) depend on nothing but the
//!   store's facts; the [`outbox_recorder`] records them on its own thread;
//!   [`write_operations`] are the operations themselves, over the directory contents, the
//!   backing directory, the node resolver, the [`open_writers`], the recorder and the rules.
//! - A file that is not downloaded: [`range_readers`] holds the files being read as ranges,
//!   and fetches the ranges; [`download_states`] holds the state of each file's download and
//!   does no I/O; [`file_downloader`] downloads a file whole, over the backing directory,
//!   the directory contents and the download states; [`pins`] holds the pins, over the
//!   directory contents.
//! - A change from OneDrive: [`cycle_step`] is the step of a cycle of the delta listing
//!   over the backing directory; what finds the user's own in its way is moved aside by
//!   [`conflict_copies`]; [`inotify_bridge`] brings the change to whoever watches the folder.
//! - Who asks: [`kernel_requests`] answers the kernel, keeping what programs hold open in
//!   [`open_files`]; [`files_interface`] answers the daemon's bus interface `Files`.

mod access_mode;
mod attributes;
mod conflict_copies;
mod detections;
mod directory_reference;
mod entry_place;
mod download_states;
mod errno;
mod file_downloader;
mod files_interface;
mod inotify_bridge;
mod kernel_requests;
mod local_only_entry;
mod mount;
mod node_table;
mod open_files;
mod unsent_changes;
mod pins;
mod range_readers;
mod cycle_step;
mod upload_rules;
mod outbox_recorder;
mod node_resolver;
mod backing_directory;
mod directory_contents;
mod write_operations;
mod open_writers;

pub use files_interface::{FilesRefusal, FreedSpace, PathStatus};
pub use conflict_copies::CycleConflicts;
pub use cycle_step::{CycleOutcome, OneDriveChange};
pub use backing_directory::{clear_stale, directories, tidy, tidy_all};
pub use mount::{Mount, MountSetup, WriteSetup};
