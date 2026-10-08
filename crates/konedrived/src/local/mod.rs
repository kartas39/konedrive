//! Local changes on their way to the outbox (`docs/design/writes.md` §4): what a read-write
//! folder holds that the base does not.
//!
//! A change is made and recorded by the folder's mount (`crate::fuse`) as a program asks
//! for it; nothing is watched and nothing is examined. What is here is what the recording
//! and the worker that sends the rows go by: the names that stay local ([`ignore`]) and
//! the ones OneDrive refuses ([`names`]), how detections become rows ([`record`]), what
//! the rows hold of the folder while they wait ([`held`]), and the limits they share.

pub mod held;
mod ignore;
pub mod names;
pub mod record;

use std::time::Duration;

pub use ignore::{IgnoreList, SharedIgnore, DEFAULT_PATTERNS};

/// A file is read for its upload only once it was not written for this long (provisional).
pub const QUIET: Duration = Duration::from_secs(2);
/// A file that is busy — open for writing, being downloaded — is
/// looked at again after this long (provisional).
pub const RECHECK: Duration = Duration::from_secs(30);

/// The mass-delete guard (§4.5): a batch that would remove more items than
/// this from OneDrive is held for confirmation (provisional)...
pub const MASS_DELETE_ITEMS: u64 = 500;
/// ... or more than this share of the folder's items, in percent
/// (provisional)...
pub const MASS_DELETE_PERCENT: u64 = 20;
/// ... counted only from this many items up, so that removing one file of a
/// folder of four is not a mass delete (provisional).
pub const MASS_DELETE_FLOOR: u64 = 10;
