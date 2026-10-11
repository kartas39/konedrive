//! What becomes of a store that an older version of the daemon left: the one place where
//! it is decided, and it is never migrated.
//!
//! **State owned:** none. [`decide_old_store`] is a pure function of what the file says of
//! itself; [`TreeStore::open_with_origin`] performs its answer, and `TreeStore::open` is
//! that call with the origin left out.
//!
//! **Answers for:** the map's P7 and RISK-1.a, and design §13 (6), the owner's decision of
//! 2026-10-11: a store of an older version is rebuilt, as one of an unknown version is.
//! The cases, by what the file says of itself:
//!
//! - *today's version*: used as it is.
//! - *no file, or a database with no table*: the schema is made.
//! - *any other version* — an older one (the trial folders of the branch `fuse-probe`, 14
//!   to 19; 8 of the branch `dev`, whose folder is refused before that as one of legacy
//!   placeholders, which is not this decision), a newer daemon's — *no version, not a
//!   database*: rebuilt. What a rebuild loses is counted before the file goes
//!   ([`RebuildLoss`]): the pins (kept in the store alone), the activity log, the
//!   conflicts on record, what was unsent and is not on disk, and the open upload
//!   sessions. What is on disk is found again by the check at start.
//!
//! The upload sessions the old tables hold are not this module's to cancel: the switch
//! cancels the ones it can read before the tables go, so that their empty files do not
//! hold names in OneDrive. Here they are only counted.
//!
//! **The raises of the schema's version**, each by one hand. The first, before any body
//! of the daemon (step 0b, version 20), only ADDS: the tables `thumbnail_marks` and
//! `own_upload_contents` with a trigger each, the indexes `items_not_placed` and
//! `items_image_or_video`, and two keys of `meta`; no column for a directory's time.
//! Version 21 changes what one index holds: `items_changed_in_onedrive` does not ask the
//! time of a content any more. The last is the switch's: the tables that go, which
//! today's code reads until then. None has a migration step: a store of the version
//! before is rebuilt.
//!
//! **Tested without the rest:**
// test: a_store_of_todays_version_is_used
// test: a_store_of_an_older_version_is_rebuilt_and_never_migrated
// test: a_store_of_an_unknown_version_is_rebuilt
// test: a_file_that_is_no_database_is_rebuilt
// test: a_store_that_cannot_be_read_for_another_reason_is_left_alone
// test: where_there_is_no_store_one_is_made
// test: a_rebuild_counts_what_it_loses_before_the_file_goes

use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

use super::SCHEMA_VERSION;
use crate::{meta, TreeError};

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

/// What a rebuild loses, for the journal and for what the folder's start tells the user.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RebuildLoss {
    pub pins: u64,
    pub activity_events: u64,
    pub conflicts: u64,
    /// Unsent changes the check at start cannot find again: removals, and moves of files
    /// with nothing on disk. Counted in the outbox while a store has one, and in the rows
    /// of `items`.
    pub unsent_changes_not_on_disk: u64,
    /// The upload sessions that were opened and not ended, in the old tables and in what
    /// the steps remember.
    pub open_upload_sessions: u64,
}

/// What `TreeStore::open` does with the store it found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OldStoreDecision {
    /// Today's version: nothing to do.
    Use,
    /// No store: the schema is made.
    Make,
    /// The file is removed and the store made empty; the folder's start then goes the way
    /// of a store that was lost (design §10). What that loses is [`RebuildLoss`], counted
    /// by `TreeStore::open_with_origin` before the file goes.
    Rebuild,
}

/// How the store the folder's start was given came to be: what the start does next depends
/// on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreOrigin {
    /// It was there, of today's version.
    Kept,
    /// It was made empty: there was none (`loss` is `None`), or the one there was rebuilt,
    /// with what that lost — `None` too where nothing could be counted in it, as in a
    /// file that is no database. The check at start binds the marked entries, and the
    /// first listing is a full one.
    MadeEmpty { loss: Option<RebuildLoss> },
}

/// What to do with a store that says `stored` of itself. Pure: the cases are in the
/// module's doc. Called by `TreeStore::open_with_origin` before anything is written.
pub fn decide_old_store(stored: &StoredVersion) -> OldStoreDecision {
    match stored {
        StoredVersion::NoStore => OldStoreDecision::Make,
        StoredVersion::Version(version) if version == SCHEMA_VERSION => OldStoreDecision::Use,
        StoredVersion::Version(_) | StoredVersion::NoVersion | StoredVersion::NotADatabase => OldStoreDecision::Rebuild,
    }
}

/// Whether SQLite says of the file that it is no database, or is corrupt.
fn is_no_database(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(failure, _) if matches!(failure.code, rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt))
}

fn has_table(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    conn.query_row("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1", [table], |row| row.get::<_, i64>(0)).map(|count| count > 0)
}

/// What the file at the store's path says of itself. Reads `sqlite_master` and `meta`;
/// changes nothing. An error is one that is no finding: permission denied, the disk — the
/// store is then left alone.
pub(super) fn read_stored_version(conn: &Connection) -> Result<StoredVersion, TreeError> {
    let read = || -> rusqlite::Result<StoredVersion> {
        let tables: i64 = conn.query_row("SELECT count(*) FROM sqlite_master WHERE type = 'table'", [], |row| row.get(0))?;
        if tables == 0 {
            return Ok(StoredVersion::NoStore);
        }
        if !has_table(conn, "meta")? {
            return Ok(StoredVersion::NoVersion);
        }
        let version: Option<Option<String>> = conn.query_row("SELECT value FROM meta WHERE key = ?1", [meta::SCHEMA_VERSION], |row| row.get(0)).optional()?;
        Ok(version.flatten().map_or(StoredVersion::NoVersion, StoredVersion::Version))
    };
    match read() {
        Ok(stored) => Ok(stored),
        Err(error) if is_no_database(&error) => Ok(StoredVersion::NotADatabase),
        Err(error) => Err(error.into()),
    }
}

/// What a rebuild of the store behind `conn` would lose, counted before the file goes.
/// Called by `TreeStore::open_with_origin` when the decision is `Rebuild`, for
/// [`StoreOrigin::MadeEmpty`]. A table the store does not have holds nothing; a store it
/// cannot count in — no database, or tables of another shape — answers nothing.
pub(super) fn count_rebuild_loss(conn: &Connection) -> Option<RebuildLoss> {
    let count = |table: &str, rows: &str| -> rusqlite::Result<u64> {
        if !has_table(conn, table)? {
            return Ok(0);
        }
        conn.query_row(&format!("SELECT count(*) FROM {table} {rows}"), [], |row| row.get::<_, i64>(0)).map(|count| count.max(0) as u64)
    };
    let count_all = || -> rusqlite::Result<RebuildLoss> {
        // A removal has nothing on disk; a move of what has no local object neither.
        let unsent_in_outbox = count("outbox", "WHERE kind = 'delete' OR (kind = 'move' AND handle IS NULL)")?;
        let unsent_in_items = count(
            "items",
            "WHERE unsent_since IS NOT NULL AND onedrive_item_id IS NOT NULL
               AND (local_parent_id IS NULL
                    OR (kind = 'file' AND content_presence != 'on-disk' AND synced_parent_id IS NOT NULL
                        AND (local_parent_id IS NOT synced_parent_id OR local_name IS NOT synced_name)))",
        )?;
        Ok(RebuildLoss {
            pins: count("pins", "")?,
            activity_events: count("activity", "")?,
            conflicts: count("conflicts", "")?,
            unsent_changes_not_on_disk: unsent_in_outbox + unsent_in_items,
            open_upload_sessions: count("upload_sessions", "")? + count("step_memory", "WHERE session_address IS NOT NULL")?,
        })
    };
    count_all().ok()
}

impl crate::TreeStore {
    /// Opens the store at `path` and says how it came to be: used as it is, made where
    /// there was none, or rebuilt empty where the one there cannot be used
    /// ([`decide_old_store`]), with what the rebuild lost counted before its file went.
    /// Every other error — permission denied, disk I/O, a lock that outlasts the busy
    /// timeout — is returned unchanged and nothing on disk is touched: a failure that
    /// passes is not corruption, and must never cost the user their store.
    ///
    /// Called by the folder's start (`sync::sync_tasks::start_order`), which runs the check
    /// at start and asks for a full listing by the answer; and by `TreeStore::open`.
    pub fn open_with_origin(path: &Path) -> Result<(Self, StoreOrigin), TreeError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if !path.try_exists()? {
            return Ok((Self::open_file(path)?, StoreOrigin::MadeEmpty { loss: None }));
        }
        // The file is there, so opening it makes none; nothing is written through this
        // connection, which is closed before the store is opened or its file removed.
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let stored = read_stored_version(&conn)?;
        match decide_old_store(&stored) {
            OldStoreDecision::Use => {
                drop(conn);
                Ok((Self::open_file(path)?, StoreOrigin::Kept))
            }
            OldStoreDecision::Make => {
                drop(conn);
                Ok((Self::open_file(path)?, StoreOrigin::MadeEmpty { loss: None }))
            }
            OldStoreDecision::Rebuild => {
                let loss = count_rebuild_loss(&conn);
                drop(conn);
                tracing::warn!("{} cannot be used ({stored:?}; this daemon knows version {SCHEMA_VERSION}); it is rebuilt from a full listing, and loses {loss:?}", path.display());
                for suffix in ["", "-wal", "-shm"] {
                    match std::fs::remove_file(format!("{}{suffix}", path.display())) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                Ok((Self::open_file(path)?, StoreOrigin::MadeEmpty { loss }))
            }
        }
    }
}

#[cfg(test)]
mod tests;
