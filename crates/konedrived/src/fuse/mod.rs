//! A folder served as a FUSE mount, read-only.
//!
//! The mount lies over the folder itself. Every name comes from the tree store; the folder on
//! disk holds only the files that are downloaded, and is reached through the descriptor opened
//! before the mount, never through the mount. A file with no object there is not downloaded:
//! reading it is answered with the bytes asked for, fetched from OneDrive as a range, and once
//! more than a threshold of it was read it is downloaded whole, into a file that has no name
//! until it is complete and checked, and is then given the item's. Nothing that changes the folder is accepted from a program.
//!
//! - The requests of the mount read the store through a connection of their own, opened for
//!   reading alone: they never wait for the cycle that writes it.
//! - What changes in OneDrive is announced through the mount ([`Mount::committed`]): the
//!   announcer's thread creates, touches or removes the name by path, the mount answers it from
//!   the store, and the kernel tells whoever watches the directory.
//! - Pins are rows of the store ([`konedrive_tree::TreeStore::pins`]): a file that is not
//!   downloaded has no object to carry one.

mod announce;
mod asked;
mod attr;
mod copies;
mod fill;
mod follow;
mod lock;
mod mount;
mod nodes;
mod pins;
mod read;
mod served;
mod shared;

pub use asked::{Freed, Looked, Refused};
pub use follow::Step;
pub use lock::{clear_stale, directories, tidy, tidy_all};
pub use mount::{Mount, Parts};
