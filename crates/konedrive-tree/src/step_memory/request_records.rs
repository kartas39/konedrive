//! The requests that change OneDrive and are not settled yet: the table `request_records`,
//! one row for each item a step sent a request about, written BEFORE the request leaves
//! and kept until its answer was committed or what became of it was read from OneDrive
//! (the design's §7: "every request that changes OneDrive is on record before it leaves").
//!
//! A record says what the request asked — which item, into which directory, under which
//! name, which content by its hash —: enough to tell from OneDrive whether it went through
//! with no step running any more, after a stop or a kill of the daemon. While a record
//! stands its item is busy to the plan, and the directory it was sent into is a
//! destination no removal is sent for: the plan's input reads them beside the running
//! steps ([`list_records`]).
//!
//! An upload through an upload session writes none: its record is the opening and the
//! session the step memory keeps (`upload_session_openings`, the item's memory).
//!
//! *Voided by:* the commit of the request's answer and the reading of what became of it
//! (`StepMemory::forget_request_record` by the step,
//! `StepMemory::forget_request_record_as_read` by the one that settles the records left,
//! which forgets the record it read and no later one); an answer that says for certain that the request changed nothing; and the
//! item's row going (the trigger `step_memory_item_gone`). One record per item: a step
//! writes its record over one that was left, which the item's one step at a time makes the
//! same request sent again.

use std::ffi::OsString;

use rusqlite::{params, Connection};

use super::memory_row::name_value;
use crate::item_store::{find_item, refuse};
use crate::model::{DriveId, LocalItemId, OneDriveItemId, OneDriveItemReference};
use crate::path_column::path_from;
use crate::TreeError;

/// What a request asked of OneDrive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedRequest {
    /// A directory made in `parent` under `name`.
    MakeDirectory { parent: OneDriveItemReference, name: OsString },
    /// A file with the content of `sent_hash` (QuickXorHash, base64) made in `parent`
    /// under `name` by one request: an empty file.
    SendNewFile { parent: OneDriveItemReference, name: OsString, sent_hash: String },
    /// The content of `sent_hash` sent into the item by one request: an empty content.
    SendContent { sent_hash: String },
    /// The item sent into `parent` under `name`.
    SendMove { parent: OneDriveItemReference, name: OsString },
    /// The removal of a file.
    SendFileRemoval,
    /// The removal of a directory with all OneDrive has in it.
    SendDirectoryRemoval,
}

impl RecordedRequest {
    /// The directory of OneDrive the request sends its item into, when it sends one into
    /// a directory.
    pub fn destination(&self) -> Option<&OneDriveItemReference> {
        match self {
            RecordedRequest::MakeDirectory { parent, .. } | RecordedRequest::SendNewFile { parent, .. } | RecordedRequest::SendMove { parent, .. } => Some(parent),
            RecordedRequest::SendContent { .. } | RecordedRequest::SendFileRemoval | RecordedRequest::SendDirectoryRemoval => None,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            RecordedRequest::MakeDirectory { .. } => "make-directory",
            RecordedRequest::SendNewFile { .. } => "send-new-file",
            RecordedRequest::SendContent { .. } => "send-content",
            RecordedRequest::SendMove { .. } => "send-move",
            RecordedRequest::SendFileRemoval => "send-file-removal",
            RecordedRequest::SendDirectoryRemoval => "send-directory-removal",
        }
    }
}

/// A request that left and is not settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRecord {
    /// The item whose step sent the request.
    pub item: LocalItemId,
    /// The item of OneDrive the request was about, as the row was bound when it left; none
    /// for a request that makes a new item.
    pub onedrive_item: Option<OneDriveItemReference>,
    pub request: RecordedRequest,
    /// When the record was written, Unix seconds.
    pub sent_at: i64,
}

/// The table: one record per item.
pub(super) fn schema() -> &'static str {
    "CREATE TABLE request_records (
         local_item_id INTEGER PRIMARY KEY, kind TEXT NOT NULL,
         onedrive_drive_id TEXT, onedrive_item_id TEXT,
         parent_drive_id TEXT, parent_item_id TEXT, name, sent_hash TEXT,
         sent_at INTEGER NOT NULL,
         CHECK ((onedrive_drive_id IS NULL) = (onedrive_item_id IS NULL)),
         CHECK ((parent_drive_id IS NULL) = (parent_item_id IS NULL)));"
}

/// What the trigger `step_memory_item_gone` does to this table when the row `OLD` of `items`
/// goes: a record does not outlive its item.
pub(super) const ITEM_GONE_STATEMENTS: &str = "
    DELETE FROM request_records WHERE local_item_id = OLD.local_item_id;";

/// `record` is written, over the one its item had. Refused for an id with no row.
pub(super) fn remember_record(conn: &Connection, record: &RequestRecord) -> Result<(), TreeError> {
    if find_item(conn, record.item)?.is_none() {
        return Err(refuse(format!("item {} has no row: no request of it is on record", record.item)));
    }
    let (parent, name, sent_hash) = match &record.request {
        RecordedRequest::MakeDirectory { parent, name } | RecordedRequest::SendMove { parent, name } => (Some(parent), Some(name), None),
        RecordedRequest::SendNewFile { parent, name, sent_hash } => (Some(parent), Some(name), Some(sent_hash.as_str())),
        RecordedRequest::SendContent { sent_hash } => (None, None, Some(sent_hash.as_str())),
        RecordedRequest::SendFileRemoval | RecordedRequest::SendDirectoryRemoval => (None, None, None),
    };
    conn.prepare_cached(
        "INSERT OR REPLACE INTO request_records (local_item_id, kind, onedrive_drive_id, onedrive_item_id, parent_drive_id, parent_item_id, name, sent_hash, sent_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?
    .execute(params![
        record.item.as_i64(),
        record.request.kind(),
        record.onedrive_item.as_ref().map(|item| item.drive_id.as_str()),
        record.onedrive_item.as_ref().map(|item| item.item_id.as_str()),
        parent.map(|parent| parent.drive_id.as_str()),
        parent.map(|parent| parent.item_id.as_str()),
        name.map(|name| name_value(name)),
        sent_hash,
        record.sent_at,
    ])?;
    Ok(())
}

/// `record` is settled by one that read it some time ago: it goes when it is still the
/// record its item has — the same request, written at the same time — and no other does: a
/// record a step wrote since is that step's. Whether it went.
pub(super) fn forget_record_as_read(conn: &Connection, record: &RequestRecord) -> Result<bool, TreeError> {
    let forgotten = conn
        .prepare_cached("DELETE FROM request_records WHERE local_item_id = ?1 AND sent_at = ?2 AND kind = ?3")?
        .execute(params![record.item.as_i64(), record.sent_at, record.request.kind()])?;
    Ok(forgotten > 0)
}

/// The record of `item` is settled: it goes. Nothing for an item with none.
pub(super) fn forget_record(conn: &Connection, item: LocalItemId) -> Result<(), TreeError> {
    conn.prepare_cached("DELETE FROM request_records WHERE local_item_id = ?1")?.execute([item.as_i64()])?;
    Ok(())
}

fn reference_from(drive_id: Option<String>, item_id: Option<String>) -> Option<OneDriveItemReference> {
    Some(OneDriveItemReference { drive_id: DriveId::new(drive_id?)?, item_id: OneDriveItemId::new(item_id?) })
}

/// A row as a record; none for a row that cannot be read as one (a kind this version does
/// not know, a place that is not whole).
fn record_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<RequestRecord>> {
    let kind: String = row.get(1)?;
    let onedrive_item = reference_from(row.get(2)?, row.get(3)?);
    let parent = reference_from(row.get(4)?, row.get(5)?);
    let name_column = row.get_ref(6)?;
    let name = (name_column != rusqlite::types::ValueRef::Null).then(|| path_from(name_column).into_os_string());
    let sent_hash: Option<String> = row.get(7)?;
    let request = match (kind.as_str(), parent, name, sent_hash) {
        ("make-directory", Some(parent), Some(name), _) => RecordedRequest::MakeDirectory { parent, name },
        ("send-new-file", Some(parent), Some(name), Some(sent_hash)) => RecordedRequest::SendNewFile { parent, name, sent_hash },
        ("send-content", _, _, Some(sent_hash)) => RecordedRequest::SendContent { sent_hash },
        ("send-move", Some(parent), Some(name), _) => RecordedRequest::SendMove { parent, name },
        ("send-file-removal", ..) => RecordedRequest::SendFileRemoval,
        ("send-directory-removal", ..) => RecordedRequest::SendDirectoryRemoval,
        _ => return Ok(None),
    };
    Ok(Some(RequestRecord { item: LocalItemId::new(row.get(0)?), onedrive_item, request, sent_at: row.get(8)? }))
}

/// Up to `limit` records that stand, the one written first first.
pub(crate) fn list_records(conn: &Connection, limit: usize) -> Result<Vec<RequestRecord>, TreeError> {
    let mut statement = conn.prepare_cached(
        "SELECT local_item_id, kind, onedrive_drive_id, onedrive_item_id, parent_drive_id, parent_item_id, name, sent_hash, sent_at
           FROM request_records ORDER BY sent_at, local_item_id LIMIT ?1",
    )?;
    let records = statement.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], record_from)?;
    Ok(records.collect::<Result<Vec<_>, _>>()?.into_iter().flatten().collect())
}
