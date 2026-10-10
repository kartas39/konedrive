//! What becomes of a store that an older version of the daemon left: the one place where
//! "migrate or rebuild" is decided.
//!
//! **State owned:** none. [`decide_old_store`] is a pure function of what the file says of
//! itself; `TreeStore::open` performs its answer (today: `prepare` and `should_rebuild`,
//! `schema.rs:309-345`, which rebuild every version no migration step starts from).
//!
//! **Answers for:** the map's P7 and RISK-1.a. The cases, by the version found:
//!
//! - *today's version*: used as it is.
//! - *14 or 15 with the rows of `items` and the outbox beside them* (the trial folders of
//!   the branch `fuse-probe`): the product question. **Migrated**, the tables that stay
//!   are kept — pins, the activity log, the conflicts on record, `last_checked`, what the
//!   steps remember — and the tables that go are dropped; what the outbox held is found
//!   again by the check at start where it is on disk (new items, changed content), and a
//!   removal or a move of an item with nothing on disk is lost. **Rebuilt**, all of that is
//!   lost too: the pins (kept in the store alone), the log, the conflicts' records, the
//!   open upload sessions, whose empty files then hold their names in OneDrive until
//!   OneDrive ends them.
//! - *8* (the branch `dev`: placeholders, no mount): rebuilt; the folder itself is refused
//!   before that as one of legacy placeholders, which is not this decision.
//! - *any other version, no version, not a database*: rebuilt, as today.
// OPEN: the product question above is the owner's. Both answers are cases of
// `OldStoreDecision`; `decide_old_store` is written to answer one of them for 14 and 15,
// and which is not decided here.
//!
//! **Two raises of the schema's version**, each by one hand. The first, before any body
//! of the daemon (step 0b), only ADDS: `thumbnail_marks`, the columns of
//! `own_upload_content`; no column for a directory's time. The second is the switch's:
//! the tables that go — which today's code reads until then — and this module's
//! migration or rebuild of an older store.
//!
//! **Tested without the rest:**
// test: a_store_of_todays_version_is_used
// test: a_store_of_the_trial_versions_is_decided_as_the_owner_said
// test: a_store_of_an_unknown_version_is_rebuilt
// test: a_file_that_is_no_database_is_rebuilt
// test: a_store_that_cannot_be_read_for_another_reason_is_left_alone
// test: a_migrated_store_keeps_its_pins_its_log_and_its_conflicts_and_has_no_outbox
#![allow(dead_code, unused_variables)]

use rusqlite::Connection;

use crate::TreeError;

/// What the file at the store's path says of itself, before anything is changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredVersion {
    /// No file, or a database with no table: a store is made.
    NoStore,
    /// A store that says its version.
    Version(String),
    /// Tables and no version: a build that died while it made them.
    NoVersion,
    /// SQLite says the file is no database, or is corrupt.
    NotADatabase,
}

/// What a rebuild loses that a migration would keep, for the journal and for what the
/// folder's start tells the user.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RebuildLoss {
    pub pins: u64,
    pub activity_events: u64,
    pub conflicts: u64,
    /// Unsent changes the check at start cannot find again: removals and moves of items
    /// with nothing on disk.
    pub unsent_changes_not_on_disk: u64,
    pub open_upload_sessions: u64,
}

/// What `TreeStore::open` does with the store it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OldStoreDecision {
    /// Today's version: nothing to do.
    Use,
    /// No store: the schema is made.
    Make,
    /// The steps of `migrations` bring it to today's version, starting from `from`.
    Migrate { from: String },
    /// The file is removed and the store made empty; the folder's start then goes the way
    /// of a store that was lost (design §10).
    Rebuild,
}

/// How the store the folder's start was given came to be: what the start does next depends
/// on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreOrigin {
    /// It was there, of today's version.
    Kept,
    /// It was brought to today's version from `from`.
    Migrated { from: String },
    /// It was made empty: there was none, or the one there was rebuilt, with what that
    /// lost. The check at start binds the marked entries, and the first listing is a full
    /// one.
    MadeEmpty { loss: Option<RebuildLoss> },
}

/// What to do with a store that says `stored` of itself. Pure: the cases are in the
/// module's doc. Called by `TreeStore::open` before anything is written; relies on
/// `migrations::from` having a step for every version it answers `Migrate` for (a test
/// holds the two together).
pub fn decide_old_store(stored: &StoredVersion) -> OldStoreDecision {
    todo!()
}

/// What the file at the store's path says of itself. Reads `sqlite_master` and `meta`;
/// changes nothing. An error is one that is no finding: permission denied, the disk — the
/// store is then left alone, as today (`should_rebuild`).
pub(super) fn read_stored_version(conn: &Connection) -> Result<StoredVersion, TreeError> {
    todo!()
}

/// What a rebuild of the store behind `conn` would lose, counted before the file goes.
/// Called by `TreeStore::open` when the decision is `Rebuild`, for
/// [`StoreOrigin::MadeEmpty`]; a store it cannot count in answers nothing.
pub(super) fn count_rebuild_loss(conn: &Connection) -> Option<RebuildLoss> {
    todo!()
}

/// The migration step from the trial versions (14, 15) to the first version without the
/// tables that go: drops `onedrive_items`, `staging`, `staging_gone`, `deferred`,
/// `outbox_gone`, `outbox`, `local_skipped`, `upload_sessions`, `upload_openings`,
/// `upload_openings_left` and the `meta` keys that go with them (the map's 1.1, 1.2), and
/// asks for a full listing. Run by `migrations` inside `at_version`'s transaction. The open
/// upload sessions of the old tables are not carried over: their addresses are written to
/// `abandoned_upload_sessions`, to be cancelled. Exists only if the decision is to migrate.
pub(super) fn migrate_from_trial_version(conn: &Connection) -> Result<(), TreeError> {
    todo!()
}

impl crate::TreeStore {
    /// Opens the store at `path` as [`open`](Self::open) does and says how it came to be.
    /// Called by the folder's start (`sync::sync_tasks::start_order`), which runs the check
    /// at start and asks for a full listing by the answer.
    pub fn open_with_origin(path: &std::path::Path) -> Result<(Self, StoreOrigin), TreeError> {
        todo!()
    }
}
