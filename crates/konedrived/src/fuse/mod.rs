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

mod announce;
mod asked;
mod attr;
mod changes;
mod conflict;
mod fill;
mod follow;
mod local;
mod make;
mod mode;
mod mount;
mod nodes;
mod overlay;
mod pins;
mod read;
mod recorder;
mod remove;
mod rename;
mod settle;
mod requests;
mod shared;
mod under;
mod view;
mod write;
mod writers;

pub use asked::{Freed, Looked, Refused};
pub use follow::{Followed, Step};
pub use under::{clear_stale, directories, tidy, tidy_all};
pub use mount::{Mount, Parts};
pub use shared::Writing;
