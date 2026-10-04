//! A row of the outbox in the database: the one place it is read from
//! SQLite, written to it and removed from it.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use konedrive_fs::handle::FileHandle;
use rusqlite::types::{Value, ValueRef};
use rusqlite::{params, Connection};

use super::encoded::StoredSnapshot;
use super::{Base, Inode, OutboxKind, OutboxRow, OutboxState, Reason};
use crate::model::column;
use crate::TreeError;

/// What a query that reads rows selects, in the order [`outbox_row`] finds
/// them by name.
const OUTBOX_COLUMNS: &str = "seq, kind, item_id, dev, ino, rel, base_etag, base_ctag, base_parent, base_name, \
     target_parent, target_name, state, reason, attempts, next_try, snapshot_size, snapshot_mtime, snapshot_mtime_nsec, moved_out, \
     session_url, session_expires, session_next, handle, confirmed, size";

/// A path as the store keeps it: text when it is UTF-8, its bytes otherwise
/// (Linux names need not be UTF-8; such a name is blocked, and still has to
/// be listed where it is). One path always gets the same form, so equality
/// in SQL holds.
pub(super) fn path_value(path: &Path) -> Value {
    match path.to_str() {
        Some(text) => Value::Text(text.to_owned()),
        None => Value::Blob(path.as_os_str().as_bytes().to_vec()),
    }
}

pub(super) fn path_from(value: ValueRef<'_>) -> PathBuf {
    match value {
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => PathBuf::from(OsStr::from_bytes(bytes)),
        _ => PathBuf::new(),
    }
}

/// Where each column of [`OUTBOX_COLUMNS`] is in a row [`outbox_row`] reads.
mod at {
    use super::{column, OUTBOX_COLUMNS};

    pub(super) const SEQ: usize = column(OUTBOX_COLUMNS, "seq");
    pub(super) const KIND: usize = column(OUTBOX_COLUMNS, "kind");
    pub(super) const ITEM_ID: usize = column(OUTBOX_COLUMNS, "item_id");
    pub(super) const DEV: usize = column(OUTBOX_COLUMNS, "dev");
    pub(super) const INO: usize = column(OUTBOX_COLUMNS, "ino");
    pub(super) const REL: usize = column(OUTBOX_COLUMNS, "rel");
    pub(super) const BASE_ETAG: usize = column(OUTBOX_COLUMNS, "base_etag");
    pub(super) const BASE_CTAG: usize = column(OUTBOX_COLUMNS, "base_ctag");
    pub(super) const BASE_PARENT: usize = column(OUTBOX_COLUMNS, "base_parent");
    pub(super) const BASE_NAME: usize = column(OUTBOX_COLUMNS, "base_name");
    pub(super) const TARGET_PARENT: usize = column(OUTBOX_COLUMNS, "target_parent");
    pub(super) const TARGET_NAME: usize = column(OUTBOX_COLUMNS, "target_name");
    pub(super) const STATE: usize = column(OUTBOX_COLUMNS, "state");
    pub(super) const REASON: usize = column(OUTBOX_COLUMNS, "reason");
    pub(super) const ATTEMPTS: usize = column(OUTBOX_COLUMNS, "attempts");
    pub(super) const NEXT_TRY: usize = column(OUTBOX_COLUMNS, "next_try");
    pub(super) const SNAPSHOT_SIZE: usize = column(OUTBOX_COLUMNS, "snapshot_size");
    pub(super) const SNAPSHOT_MTIME: usize = column(OUTBOX_COLUMNS, "snapshot_mtime");
    pub(super) const SNAPSHOT_MTIME_NSEC: usize = column(OUTBOX_COLUMNS, "snapshot_mtime_nsec");
    pub(super) const MOVED_OUT: usize = column(OUTBOX_COLUMNS, "moved_out");
    pub(super) const SESSION_URL: usize = column(OUTBOX_COLUMNS, "session_url");
    pub(super) const SESSION_EXPIRES: usize = column(OUTBOX_COLUMNS, "session_expires");
    pub(super) const SESSION_NEXT: usize = column(OUTBOX_COLUMNS, "session_next");
    pub(super) const HANDLE: usize = column(OUTBOX_COLUMNS, "handle");
    pub(super) const CONFIRMED: usize = column(OUTBOX_COLUMNS, "confirmed");
    pub(super) const SIZE: usize = column(OUTBOX_COLUMNS, "size");
}

/// A row of the outbox, read from a query that selects [`OUTBOX_COLUMNS`].
fn outbox_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OutboxRow> {
    let kind: String = row.get(at::KIND)?;
    let state: String = row.get(at::STATE)?;
    let dev: Option<i64> = row.get(at::DEV)?;
    let ino: Option<i64> = row.get(at::INO)?;
    let handle: Option<Vec<u8>> = row.get(at::HANDLE)?;
    let handle = handle.as_deref().and_then(FileHandle::decode);
    let inode = match (dev, ino) {
        (Some(dev), Some(ino)) => Some(Inode { dev: dev as u64, ino: ino as u64, handle }),
        _ => handle.map(|handle| Inode { dev: 0, ino: 0, handle: Some(handle) }),
    };
    let base = Base { etag: row.get(at::BASE_ETAG)?, ctag: row.get(at::BASE_CTAG)?, parent: row.get(at::BASE_PARENT)?, name: row.get(at::BASE_NAME)? };
    let has_base = base != Base::default();
    let snapshot = StoredSnapshot {
        size: row.get(at::SNAPSHOT_SIZE)?,
        mtime: row.get(at::SNAPSHOT_MTIME)?,
        mtime_nsec: row.get(at::SNAPSHOT_MTIME_NSEC)?,
        moved_out: row.get(at::MOVED_OUT)?,
    }
    .read();
    // A value no konedrive writes fails closed: the row is blocked, never
    // run as a guess.
    let (known_kind, known_state) = (OutboxKind::parse(&kind), OutboxState::parse(&state));
    let unreadable = match (known_kind, known_state, &snapshot) {
        (None, _, _) => Some(format!("unreadable kind {kind:?}")),
        (_, None, _) => Some(format!("unreadable state {state:?}")),
        (_, _, Err(marker)) => Some(format!("unreadable marker {marker:?}")),
        _ => None,
    };
    Ok(OutboxRow {
        seq: row.get(at::SEQ)?,
        kind: known_kind.unwrap_or(OutboxKind::Update),
        item_id: row.get(at::ITEM_ID)?,
        inode,
        rel: path_from(row.get_ref(at::REL)?),
        base: has_base.then_some(base),
        target_parent: row.get(at::TARGET_PARENT)?,
        target_name: row.get(at::TARGET_NAME)?,
        state: if unreadable.is_some() { OutboxState::Blocked } else { known_state.unwrap_or(OutboxState::Blocked) },
        reason: match unreadable {
            Some(why) => Some(Reason::Other(why)),
            None => row.get::<_, Option<String>>(at::REASON)?.map(Reason::from),
        },
        attempts: row.get::<_, i64>(at::ATTEMPTS)? as u32,
        next_try: row.get(at::NEXT_TRY)?,
        snapshot: snapshot.unwrap_or(None),
        session_url: row.get(at::SESSION_URL)?,
        session_expires: row.get(at::SESSION_EXPIRES)?,
        session_next: row.get::<_, Option<i64>>(at::SESSION_NEXT)?.map(|n| n as u64),
        confirmed: row.get::<_, i64>(at::CONFIRMED)? != 0,
        size: row.get::<_, Option<i64>>(at::SIZE)?.map(|n| n.max(0) as u64),
    })
}

pub(super) fn rows_where(conn: &Connection, filter: &str, params: impl rusqlite::Params) -> Result<Vec<OutboxRow>, TreeError> {
    let order = if filter.contains("ORDER BY") { "" } else { " ORDER BY seq" };
    let mut statement = conn.prepare_cached(&format!("SELECT {OUTBOX_COLUMNS} FROM outbox {filter}{order}"))?;
    let rows = statement.query_map(params, outbox_row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub(super) fn all_rows(conn: &Connection) -> Result<Vec<OutboxRow>, TreeError> {
    rows_where(conn, "", [])
}

/// Every column a row is written with, each with the row's value for it:
/// what [`insert`] and [`rewrite`] both write. `seq` is the database's, and
/// a bad item's columns have their own writer
/// ([`TreeStore::outbox_set_bad_item`](crate::TreeStore::outbox_set_bad_item)).
fn bind(row: &OutboxRow) -> Vec<(&'static str, Value)> {
    let base = row.base.clone().unwrap_or_default();
    let (dev, ino, handle) = match &row.inode {
        Some(inode) => (Some(inode.dev as i64), Some(inode.ino as i64), inode.handle.as_ref().map(FileHandle::encode)),
        None => (None, None, None),
    };
    let snapshot = StoredSnapshot::of(row.snapshot);
    vec![
        ("kind", row.kind.as_str().to_owned().into()),
        ("item_id", row.item_id.clone().into()),
        ("dev", dev.into()),
        ("ino", ino.into()),
        ("rel", path_value(&row.rel)),
        ("base_etag", base.etag.into()),
        ("base_ctag", base.ctag.into()),
        ("base_parent", base.parent.into()),
        ("base_name", base.name.into()),
        ("target_parent", row.target_parent.clone().into()),
        ("target_name", row.target_name.clone().into()),
        ("state", row.state.as_str().to_owned().into()),
        ("reason", row.reason_text().into()),
        ("attempts", i64::from(row.attempts).into()),
        ("next_try", row.next_try.into()),
        ("snapshot_size", snapshot.size.into()),
        ("snapshot_mtime", snapshot.mtime.into()),
        ("snapshot_mtime_nsec", snapshot.mtime_nsec.into()),
        ("moved_out", snapshot.moved_out.into()),
        ("session_url", row.session_url.clone().into()),
        ("session_expires", row.session_expires.into()),
        ("session_next", row.session_next.map(|n| n as i64).into()),
        ("handle", handle.into()),
        ("confirmed", i64::from(row.confirmed).into()),
        ("size", row.size.map(|n| n as i64).into()),
    ]
}

/// `row` as a new row; its `seq`.
pub(super) fn insert(conn: &Connection, row: &OutboxRow) -> Result<i64, TreeError> {
    let (columns, values): (Vec<&str>, Vec<Value>) = bind(row).into_iter().unzip();
    let places = (1..=columns.len()).map(|n| format!("?{n}")).collect::<Vec<_>>().join(", ");
    let sql = format!("INSERT INTO outbox ({}) VALUES ({places})", columns.join(", "));
    conn.prepare_cached(&sql)?.execute(rusqlite::params_from_iter(values))?;
    Ok(conn.last_insert_rowid())
}

/// The row that has `row.seq` is `row` from now on.
pub(super) fn rewrite(conn: &Connection, row: &OutboxRow) -> Result<(), TreeError> {
    let (columns, mut values): (Vec<&str>, Vec<Value>) = bind(row).into_iter().unzip();
    let sets = columns.iter().enumerate().map(|(n, column)| format!("{column} = ?{}", n + 1)).collect::<Vec<_>>().join(", ");
    let sql = format!("UPDATE outbox SET {sets} WHERE seq = ?{}", columns.len() + 1);
    values.push(row.seq.into());
    conn.prepare_cached(&sql)?.execute(rusqlite::params_from_iter(values))?;
    Ok(())
}

/// The content row `seq` sends, or its marker, is `snapshot` from now on.
pub(super) fn set_snapshot(conn: &Connection, seq: i64, snapshot: Option<super::Snapshot>) -> Result<(), TreeError> {
    let stored = StoredSnapshot::of(snapshot);
    conn.prepare_cached("UPDATE outbox SET snapshot_size = ?2, snapshot_mtime = ?3, snapshot_mtime_nsec = ?4, moved_out = ?5 WHERE seq = ?1")?
        .execute(params![seq, stored.size, stored.mtime, stored.mtime_nsec, stored.moved_out])?;
    Ok(())
}

/// Row `seq` leaves the outbox, whatever the reason: every delete of a row
/// is this one. Whether it was there.
///
/// The record of an opening the row made (issue #84) is kept without it
/// (`upload_openings_left`, issue #89), from now by the wall clock: the
/// placeholder the opening may have left in OneDrive is still this
/// folder's. Call it inside the transaction that removes the row, so that
/// a row never goes and leaves its record pointing at nothing.
pub(super) fn remove(conn: &Connection, seq: i64) -> Result<bool, TreeError> {
    if conn.prepare_cached("DELETE FROM outbox WHERE seq = ?1")?.execute([seq])? == 0 {
        return Ok(false);
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_secs() as i64);
    conn.prepare_cached(
        "INSERT INTO upload_openings_left (parent, name, at, last, left_at)
             SELECT parent, name, at, COALESCE(last, at), ?2 FROM upload_openings WHERE seq = ?1",
    )?
    .execute(params![seq, now])?;
    conn.prepare_cached("DELETE FROM upload_openings WHERE seq = ?1")?.execute([seq])?;
    Ok(true)
}
