//! A folder served as a FUSE mount: the one way the daemon shows OneDrive in a folder.
//!
//! The mount lies over the folder itself. Every name comes from the tree store; the folder on
//! disk holds only the files that are downloaded, and is reached through the descriptor opened
//! before the mount, never through the mount. A file with no object there is not downloaded:
//! reading it is answered with the bytes asked for, fetched from OneDrive as a range, and once
//! more than a threshold of it was read it is downloaded whole, into a file that has no name
//! until it is complete and checked, and is then given the item's.
//!
//! A read-only folder's mount refuses every change. A read-write one's takes them
//! ([`Mount::read_write`]): each change a program asks for is made in the folder under the
//! mount and recorded in the outbox by one thread, under the folder's tree lock, before the
//! program is answered; the outbox worker sends what is recorded.
//!
//! - The requests of the mount read the store through a connection of their own, opened for
//!   reading alone: they never wait for the cycle that writes it.
//! - What changes in OneDrive is announced through the mount ([`Mount::committed`]): the
//!   announcer's thread creates, touches or removes the name by path, the mount answers it from
//!   the store, and the kernel tells whoever watches the directory.
//! - Pins are rows of the store ([`konedrive_tree::TreeStore::pins`]): a file that is not
//!   downloaded has no object to carry one.
//!
//! The mount is built of parts that own their state, each usable and tested without a mount:
//! [`backing_directory`] (the folder on disk, by its descriptor) and [`unsent_changes`] (what the outbox's rows
//! say) depend on nothing; [`directory_contents`] (the folder as it is shown) is over those two; [`node_table`]
//! is the map of the kernel's node ids, and [`node_lookup`] is over the view and the nodes;
//! [`attr`] is a function of facts. A change a program makes: [`upload_rules`] (what is
//! uploaded at all) and [`detections`] (what a change is recorded as, a function of typed
//! events) depend on nothing but the store's facts; the [`recorder`] records them on its own
//! thread; [`write_operations`] are the operations themselves, over the view, the disk, the
//! nodes, the writers, the recorder and the rules. `shared` holds them together.

mod announce;
mod asked;
mod attr;
mod conflict;
mod detections;
mod fill;
mod follow;
mod local_entries;
mod mode;
mod mount;
mod node_table;
mod unsent_changes;
mod pins;
mod upload_rules;
mod read;
mod recorder;
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
