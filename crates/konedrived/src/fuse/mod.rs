//! A folder served as a FUSE mount: the one way the daemon shows OneDrive in a folder.
//!
//! The mount lies over the folder itself. Every name comes from the tree store; the folder on
//! disk holds only the files that are downloaded, and is reached through the descriptor opened
//! before the mount, never through the mount. A file with no copy there is not downloaded:
//! reading it is answered with the bytes asked for, fetched from OneDrive as a range, and once
//! more than a threshold of it was read it is downloaded whole, into a file that has no name
//! until it is complete and checked, and is then given the file's.
//!
//! A read-only folder's mount refuses every change. A read-write one's takes them
//! ([`Mount::read_write`]): each change a program asks for is made in the backing
//! directory and recorded in the outbox by one thread, under the folder's tree lock, before the
//! program is answered; the outbox worker sends what is recorded.
//!
//! - The requests of the mount read the store through a connection of their own, opened for
//!   reading alone: they never wait for the cycle that writes it.
//! - What changes in OneDrive is announced through the mount ([`Mount::committed`]): the
//!   announcer's thread creates, touches or removes the name by path, the mount answers it from
//!   the store, and the kernel tells whoever watches the directory.
//! - Pins are rows of the store ([`konedrive_tree::TreeStore::pins`]): a file that is not
//!   downloaded has nothing on disk to carry one.
//!
//! The mount is built of parts that own their state, each usable and tested without a mount:
//! [`backing_directory`] (the directory on disk the mount lies over, by its descriptor) and
//! [`unsent_changes`] (what was moved, removed or changed here and is not sent yet) depend on
//! nothing; [`directory_contents`] (what each directory of the mount holds, each entry with
//! its sync state) is over those two; [`node_table`] holds the node ids given to the kernel,
//! and [`node_lookup`] is over the directory contents and the node table; [`attributes`] is a
//! function of an entry and the access mode. A change a program makes: [`upload_rules`] (what
//! is uploaded at all) and [`detections`] (what a change is recorded as, a function of typed
//! events) depend on nothing but the store's facts; the [`outbox_recorder`] records them on
//! its own thread; [`write_operations`] are the operations themselves, over the directory
//! contents, the disk, the node lookup, the open writers, the recorder and the rules. A
//! change from OneDrive that finds the user's own in its way: [`conflict_copies`]. `shared`
//! holds them together.

mod access_mode;
mod announce;
mod asked;
mod attributes;
mod conflict_copies;
mod detections;
mod directory;
mod download_states;
mod file_download;
mod follow;
mod local_only_entries;
mod mode;
mod mount;
mod node_table;
mod unsent_changes;
mod pins;
mod upload_rules;
mod read;
mod outbox_recorder;
mod requests;
mod node_lookup;
mod shared;
mod backing_directory;
mod directory_contents;
mod write_operations;
mod open_writers;

pub use asked::{Freed, Looked, Refused};
pub use follow::{Followed, Step};
pub use backing_directory::{clear_stale, directories, tidy, tidy_all};
pub use mount::{Mount, Parts};
pub use mount::Writing;
