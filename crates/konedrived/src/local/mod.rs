//! Local changes, from the disk to the outbox (`docs/design/writes.md` §4, amended by
//! §17): what a read-write folder holds that the base does not.
//!
//! The watcher turns notification events into a [`Batch`] of dirty
//! places — directories and names, and object handles — and hands it over
//! once the folder has been quiet for [`QUIET`] (at the latest [`CEILING`]
//! after the first event). The [`Examiner`] compares what is on disk there
//! with the base (`items`) and records what differs as outbox rows
//! ([`konedrive_tree::outbox`]); the outbox worker sends them. A
//! [`Batch::full`] examines every directory: the Full local scan, run at
//! bring-up, after a queue overflow, after a helper reconnect and when the
//! ignore list shrinks.
//!
//! Events are hints, never the truth: every decision is made from the disk,
//! by item id, file handle and content, so a lost or merged event costs a
//! scan, never a wrong upload.
//!
//! The daemon runs it from `sync/watcher.rs`: the watcher ([`watcher`]) hands each
//! batch to the examination under the folder's tree lock (`watcher/service.rs`).
//! One path leads to each thing here: what an examination is made of and gives back
//! is named at this level ([`Batch`], [`Examiner`], [`Examined`], [`IgnoreList`]);
//! the rest is in its own public module ([`handles`], [`liveness`], [`names`],
//! [`scan`], [`watcher`]).

mod batch;
mod entry;
mod examine;
pub mod handles;
mod ignore;
pub mod liveness;
pub mod names;
pub mod scan;
#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;
pub mod watcher;

use std::time::Duration;

pub use batch::{Batch, ScanReason};
pub use examine::{ExamineError, Examined, Examiner, ScanProgress};
pub use ignore::{IgnoreList, SharedIgnore, DEFAULT_PATTERNS};
#[cfg(test)]
pub use testing::FakeLiveness;

/// A batch is examined when no event came for this long (provisional).
pub const QUIET: Duration = Duration::from_secs(2);
/// ... and at the latest this long after its first event, during continuous
/// activity (provisional).
pub const CEILING: Duration = Duration::from_secs(30);
/// A file that is busy — open for writing, being filled or freed — is
/// examined again after this long (provisional).
pub const RECHECK: Duration = Duration::from_secs(30);

/// The mass-delete guard (§3.4): a batch that would remove more items than
/// this from OneDrive is held for confirmation (provisional)...
pub const MASS_DELETE_ITEMS: u64 = 500;
/// ... or more than this share of the folder's items, in percent
/// (provisional)...
pub const MASS_DELETE_PERCENT: u64 = 20;
/// ... counted only from this many items up, so that removing one file of a
/// folder of four is not a mass delete (provisional; the design is silent).
pub const MASS_DELETE_FLOOR: u64 = 10;
