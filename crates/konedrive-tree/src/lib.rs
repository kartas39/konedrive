//! The tree store: one row per file and folder of the
//! drive, and the delta link. A map, not the truth — the extended attributes
//! on the files are that — so it is rebuilt from a full listing whenever it
//! cannot be used, and losing it costs one listing, never data.
//!
//! `items` has one row per item the folder shows or OneDrive has, keyed by a local item id
//! that is never given twice: the item's binding to OneDrive, what the folder shows of it,
//! what OneDrive has of it, and what the two sides last agreed on ([`ItemStore`]). Nothing
//! reads it for the folder yet, and no cycle writes a listing into it yet. What makes the two
//! sides of the rows agree again is the sync plan ([`make_sync_plan`]), a function of rows
//! given as values, which nothing calls yet either.
//!
//! `onedrive_items` is the tree the folder was last made to match; [`Table::Staging`]
//! is the tree a cycle is building. The folder is reconciled against the new
//! tree, and only then does it replace `onedrive_items`, in one transaction, so a
//! crash in between leaves `onedrive_items` and the delta link as they were and the
//! next cycle asks for the same changes again.
//!
//! A delta's new tree is `onedrive_items` with the delta laid over it:
//! the table `staging` holds only the rows the delta writes, each a whole
//! row, and `staging_gone` the ids it removes. Reading the new tree reads
//! `staging` first and `onedrive_items` for the rest; the swap writes those rows and
//! removes those ids, nothing else. A full listing, which may leave out
//! anything, is staged whole instead: `staging` is then the new tree by
//! itself (`meta` `staging_whole`), and the swap replaces every row.
//!
//! A folder's first listing is the one exception: each page goes
//! into `onedrive_items` as soon as it is placed, with the link to the next page
//! (`listing_next`) in the same transaction, so that a listing stopped
//! part-way resumes where it stopped. `commit_staging` ends it as it ends
//! every listing.

use std::path::PathBuf;

use rusqlite::Connection;

mod activity;
mod conflicts;
mod folded_name;
mod item_store;
mod meta;
mod model;
pub mod outbox;
mod pins;
mod placing;
mod query;
mod read;
pub mod reconcile;
mod schema;
mod shared;
mod source;
mod staging;
mod sync_plan;
mod thumbs;

pub use activity::{ActivityKind, ActivityRow, ACTIVITY_KEPT};
pub use conflicts::{ConflictKind, ConflictRow};
pub use folded_name::FoldedName;
pub use item_store::{ConfirmedOneDriveVersion, ItemStore, ListedChangeOutcome, ListedChangeRefusal, ListedPageOutcome, ListingId, ListingKind, ListingLink, ListingUnderWay};
pub use model::{
    usable_id, Change, ContentPresence, ContentVersion, Counts, DriveId, FileSyncState, FolderVisibility, Item, ItemKind, ItemPlace, ItemVersion, Kind, LocalItemId,
    LocalVersionChange, Located, NewTree, OneDriveItemId, OneDriveItemReference, Placement, Row, SkipReason, Table,
};
pub use konedrive_reason::WaitsFor;
pub use query::Skipped;
pub use read::ReadStore;
pub use schema::SCHEMA_VERSION;
pub use shared::{Store, QUEUE};
pub use sync_plan::{
    make_conflict_copy_name, make_sync_plan, ExclusionReason, FolderStep, ItemExclusion, ItemVersions, ItemWait, OneDriveStep, PlanFault, RowStep, SendingState, SyncPlan, SyncPlanInput, SyncStep,
    WaitReason,
};
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
    /// The store refused a change because it would break a rule of its rows: the refused
    /// change wrote nothing, and neither SQLite nor the disk failed. What the same
    /// transaction wrote before it is still in that transaction: a caller that has one open
    /// drops it on this error as on every other.
    #[error("the tree store refuses: {0}")]
    Refused(String),
}

pub struct TreeStore {
    conn: Connection,
    /// Where it is on disk; `None` in memory.
    path: Option<PathBuf>,
    /// `staging` holds a whole new tree ([`NewTree::Whole`]), not a delta.
    whole: bool,
    /// What changed in the outbox since it was last asked.
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

#[cfg(test)]
mod tests;
