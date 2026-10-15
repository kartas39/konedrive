//! The store: one row per item, what the steps remember, the pins, the activity log, the
//! conflicts, and the delta link. What the folder shows is written here alone; a store
//! that is lost is rebuilt from OneDrive and the disk, which costs one listing and every
//! unsent change that left nothing on disk (`docs/design/decisions.md` §3.9).
//!
//! `items` has one row per item the folder shows or OneDrive has, keyed by a local item id
//! that is never given twice: the item's binding to OneDrive, what the folder shows of it,
//! what OneDrive has of it, and what the two sides last agreed on ([`ItemStore`]). What makes the two
//! sides of the rows agree again is the sync plan ([`make_sync_plan`]), a function of rows
//! given as values.

use std::path::PathBuf;

use rusqlite::Connection;

mod activity;
mod conflicts;
mod folded_name;
mod item_store;
mod meta;
mod model;
mod path_column;
mod pins;
mod read;
mod schema;
mod shared;
/// What a step remembers of an item beyond the versions.
mod step_memory;
mod sync_plan;

pub use activity::{ActivityKind, ActivityRow, ACTIVITY_KEPT};
pub use conflicts::ConflictRow;
pub use folded_name::FoldedName;
pub use item_store::{ConfirmedItem, ConfirmedOneDriveVersion, ItemStore, ListedChangeOutcome, ListedChangeRefusal, ListedPageOutcome, ListingId, ListingKind, ListingLink, ListingUnderWay, StepCommitOutcome};
pub use step_memory::{
    is_content_of, EmptyFileSession, FactStanding, FailedTries, FailedTryStanding, ItemMemory, ItemMemoryParts, ItemRefusalReason, ItemTakenName, MismatchedItem, MismatchedItemStanding, MismatchedItemUpload, OpeningTry, OpeningWindow, RecordedRequest, Refusal, RefusedVersion, RequestRecord,
    SpaceFit, SpaceRefusal, StepFact, StepMemory, TakenName, TemporaryName, TimedWait, UploadSession, UploadSessionAddress, UploadSessionStanding, UploadTag,
    UPLOAD_SESSION_OPENING_KEPT,
};
pub use item_store::{OwnUploadContent, PinChange};
pub use item_store::{MadeEntryOwner, PlaceChangeRecord, PlaceChangeRecordId, PlaceChangeRowWrite};
pub use item_store::{FullListingCause, ItemCounts, ItemNotShown, ItemReader, ItemsNeverShown, PinnedDownloadTotal, ThumbnailCandidate, UnsentTotal};
pub use model::{
    usable_id, Change, ContentPresence, ContentVersion, DriveId, FileSyncState, FolderVisibility, Item, ItemKind, ItemPlace, ItemVersion, Kind, LocalItemId,
    LocalVersionChange, OneDriveItemId, OneDriveItemReference, Placement, Row, SkipReason,
};
pub use konedrive_reason::WaitsFor;
/// The reasons a change waits or is refused for: `konedrive-reason`'s, named here so that
/// the daemon does not need that crate.
pub use konedrive_reason::Reason;
/// The groups of those reasons, in the order the window shows them (`Group::ALL`).
pub use konedrive_reason::Group;
pub use read::ReadStore;
pub use schema::SCHEMA_VERSION;
pub use schema::{decide_old_store, find_stored_version, list_old_store_upload_sessions, OldStoreDecision, RebuildLoss, StoreOrigin, StoredVersion};
pub use shared::{Store, QUEUE};
pub use sync_plan::{
    is_same_content_but_for_time, make_conflict_copy_name, make_sync_plan, make_temporary_name, ExclusionReason, FolderStep, FolderStepKind, ItemExclusion, ItemVersions, ItemWait, NextTry, OneDriveStep, PlanFault, RowStep, RowStepKind, SyncPlan, SyncPlanInput, SyncStep, TriedStep,
    WaitReason, READ_ONLY_FOLDER,
};

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
    Refused(RowRefusal),
}

/// A change the store refused: which rule of the rows it would break, and the text that
/// says it of the items it was about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowRefusal {
    pub reason: RowRefusalReason,
    pub text: String,
}

/// Why the store refused a change, where a caller answers a program by it: a change of a
/// local version ([`ItemStore::apply`]) and the row written with the going of a record
/// ([`ItemStore::settle_place_change_record`]) say the first four.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowRefusalReason {
    /// The item the change is of has no row, or the folder does not show it.
    ItemGone,
    /// The folder shows another item under the name, in the directory.
    PlaceTaken,
    /// The folder shows something in the directory that is to go.
    DirectoryNotEmpty,
    /// The directory something is to be placed in has no row, or the folder does not
    /// show it.
    NoDirectory,
    /// Any other rule of the rows: the text alone says which.
    AnotherRule,
}

impl std::fmt::Display for RowRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.text)
    }
}

impl From<String> for RowRefusal {
    /// A refusal that names no rule a caller answers by.
    fn from(text: String) -> Self {
        RowRefusal { reason: RowRefusalReason::AnotherRule, text }
    }
}

impl From<&str> for RowRefusal {
    fn from(text: &str) -> Self {
        RowRefusal::from(text.to_owned())
    }
}

pub struct TreeStore {
    conn: Connection,
    /// Where it is on disk; `None` in memory.
    path: Option<PathBuf>,
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
