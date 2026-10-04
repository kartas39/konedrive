//! The tree store: one row per file and folder of the
//! drive, and the delta link. A map, not the truth — the extended attributes
//! on the files are that — so it is rebuilt from a full listing whenever it
//! cannot be used, and losing it costs one listing, never data.
//!
//! `items` is the tree the folder was last made to match; [`Table::Staging`]
//! is the tree a cycle is building. The folder is reconciled against the new
//! tree, and only then does it replace `items`, in one transaction, so a
//! crash in between leaves `items` and the delta link as they were and the
//! next cycle asks for the same changes again.
//!
//! A delta's new tree is `items` with the delta laid over it (issue #39):
//! the table `staging` holds only the rows the delta writes, each a whole
//! row, and `staging_gone` the ids it removes. Reading the new tree reads
//! `staging` first and `items` for the rest; the swap writes those rows and
//! removes those ids, nothing else. A full listing, which may leave out
//! anything, is staged whole instead: `staging` is then the new tree by
//! itself (`meta` `staging_whole`), and the swap replaces every row.
//!
//! A folder's first listing is the one exception: each page goes
//! into `items` as soon as it is placed, with the link to the next page
//! (`listing_next`) in the same transaction, so that a listing stopped
//! part-way resumes where it stopped. `commit_staging` ends it as it ends
//! every listing.

use std::path::PathBuf;

use rusqlite::Connection;

mod activity;
mod conflicts;
mod forget;
mod meta;
mod model;
pub mod outbox;
mod plan;
mod query;
mod read;
pub mod reconcile;
mod schema;
mod shared;
mod source;
mod staging;
mod thumbs;

#[cfg(test)]
use forget::forget_subtrees;
pub use activity::{ActivityRow, ACTIVITY_KEPT};
pub use conflicts::{ConflictKind, ConflictRow};
pub use model::{usable_id, Chain, Change, Counts, Kind, Located, NewTree, Placement, Row, SkipReason, Table};
pub use plan::{Plan, Planned, Side};
pub use read::ReadStore;
pub use schema::SCHEMA_VERSION;
pub use shared::{Store, QUEUE};
pub use thumbs::{Thumbnail, ThumbnailBatch, THUMB_PAGE, THUMB_SCAN};

/// A chain of parents longer than this is a cycle or corruption, not a drive.
const MAX_CHAIN: usize = konedrive_fs::MAX_DEPTH + 2;

#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    #[error("the tree store: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("the tree store: {0}")]
    Io(#[from] std::io::Error),
    #[error("the tree store has schema version {0:?}; this daemon knows {SCHEMA_VERSION}")]
    Schema(Option<String>),
}

pub struct TreeStore {
    conn: Connection,
    /// Where it is on disk; `None` in memory.
    path: Option<PathBuf>,
    /// `staging` holds a whole new tree ([`NewTree::Whole`]), not a delta.
    whole: bool,
    /// What changed in the outbox since it was last asked (issue #38).
    changes: std::sync::Arc<outbox::OutboxChanges>,
}

impl TreeStore {
    /// Runs `sql` as it is: the bench seeds a large store fast.
    #[cfg(any(test, feature = "testing"))]
    pub fn bench_sql(&self, sql: &str) -> Result<(), TreeError> {
        self.conn.execute_batch(sql)?;
        Ok(())
    }
}

/// Runs `f` on a plain thread of its own and waits for it: for tests that
/// call blocking code (the activity log, the examiner) from async code.
#[cfg(any(test, feature = "testing"))]
pub fn off_runtime<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| scope.spawn(f).join().expect("the plain thread panicked"))
}

#[cfg(test)]
mod tests;
