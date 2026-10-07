//! What the store keeps one of: the `meta` table, a key and a text each.
//! Every key is named here, and each has its own typed accessors on
//! [`TreeStore`]; nothing outside the crate reads or writes a key by name.

use rusqlite::{params, Connection, OptionalExtension};

use crate::{TreeError, TreeStore};

/// The schema's version ([`crate::schema`]).
pub(crate) const SCHEMA_VERSION: &str = "schema_version";
/// The link the next delta is asked from.
pub(crate) const DELTA_LINK: &str = "delta_link";
/// The item id of the drive's root.
pub(crate) const ROOT_ITEM_ID: &str = "root_item_id";
/// A first listing's resume point.
pub(crate) const LISTING_NEXT: &str = "listing_next";
/// Set while `staging` holds a whole new tree (a full listing) rather than
/// a delta laid over `items`.
pub(crate) const STAGING_WHOLE: &str = "staging_whole";
/// The drive the tree is a listing of.
const DRIVE_ID: &str = "drive_id";
/// When a cycle last ended, unix seconds.
const LAST_CHECKED: &str = "last_checked";
/// The filesystem the recorded file handles were taken on.
const HANDLES_FILESYSTEM: &str = "handles_root";
/// The count of outbox commits: `items.local_seq` of the row a commit
/// writes (the stale-delta guard, `docs/design/writes.md` §9).
pub(crate) const OUTBOX_SEQ: &str = "outbox_seq";
/// A pause's end, unix seconds; `0` until resumed (§11).
pub(crate) const PAUSED_UNTIL: &str = "paused_until";

/// The value of `key`, read on `conn`: a transaction's too.
pub(crate) fn get(conn: &Connection, key: &str) -> Result<Option<String>, TreeError> {
    let value: Option<Option<String>> = conn.prepare_cached("SELECT value FROM meta WHERE key = ?1")?.query_row([key], |row| row.get(0)).optional()?;
    Ok(value.flatten())
}

/// `key` is `value` from now on; `None` takes the key away.
pub(crate) fn set(conn: &Connection, key: &str, value: Option<&str>) -> Result<(), TreeError> {
    match value {
        Some(value) => conn
            .prepare_cached("INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value")?
            .execute(params![key, value])?,
        None => conn.prepare_cached("DELETE FROM meta WHERE key = ?1")?.execute([key])?,
    };
    Ok(())
}

/// `++outbox_seq`, inside the caller's transaction: the commit count a
/// commit of the outbox writes its rows with.
pub(crate) fn next_outbox_seq(tx: &rusqlite::Transaction<'_>) -> Result<i64, TreeError> {
    let next = get(tx, OUTBOX_SEQ)?.and_then(|v| v.parse::<i64>().ok()).unwrap_or(0) + 1;
    set(tx, OUTBOX_SEQ, Some(&next.to_string()))?;
    Ok(next)
}

impl TreeStore {
    pub(crate) fn meta(&self, key: &str) -> Result<Option<String>, TreeError> {
        get(&self.conn, key)
    }

    pub(crate) fn set_meta(&self, key: &str, value: Option<&str>) -> Result<(), TreeError> {
        set(&self.conn, key, value)
    }

    pub fn delta_link(&self) -> Result<Option<String>, TreeError> {
        self.meta(DELTA_LINK)
    }

    /// The next delta is asked from `link`: one a cycle got without a swap
    /// (nothing changed, so nothing was staged).
    pub fn set_delta_link(&self, link: &str) -> Result<(), TreeError> {
        self.set_meta(DELTA_LINK, Some(link))
    }

    pub fn root_item_id(&self) -> Result<Option<String>, TreeError> {
        self.meta(ROOT_ITEM_ID)
    }

    /// Where a first listing placed page by page goes on from:
    /// the link to the page after the last one placed, or `""` — the start —
    /// before its first page is committed. `None` when no such listing is
    /// under way.
    pub fn listing_next(&self) -> Result<Option<String>, TreeError> {
        self.meta(LISTING_NEXT)
    }

    /// A first listing placed page by page begins: under way, at
    /// the start, before anything of it is placed.
    pub fn begin_placing(&self) -> Result<(), TreeError> {
        self.set_meta(LISTING_NEXT, Some(""))
    }

    /// The resume point of a first listing is no use (OneDrive refused
    /// it): no such listing is under way until one begins again.
    pub fn forget_listing_next(&self) -> Result<(), TreeError> {
        self.set_meta(LISTING_NEXT, None)
    }

    /// The drive the tree is a listing of, once recorded.
    pub fn drive_id(&self) -> Result<Option<String>, TreeError> {
        self.meta(DRIVE_ID)
    }

    pub fn set_drive_id(&self, drive: &str) -> Result<(), TreeError> {
        self.set_meta(DRIVE_ID, Some(drive))
    }

    /// When a cycle last ended, unix seconds; `None` before the first, and
    /// for a value that is no number.
    pub fn last_checked(&self) -> Result<Option<i64>, TreeError> {
        Ok(self.meta(LAST_CHECKED)?.and_then(|v| v.parse().ok()))
    }

    pub fn set_last_checked(&self, at: i64) -> Result<(), TreeError> {
        self.set_meta(LAST_CHECKED, Some(&at.to_string()))
    }

    /// The filesystem the recorded file handles were taken on, as the
    /// daemon names one; `None` until it is recorded.
    pub fn handles_filesystem(&self) -> Result<Option<String>, TreeError> {
        self.meta(HANDLES_FILESYSTEM)
    }

    pub fn set_handles_filesystem(&self, filesystem: &str) -> Result<(), TreeError> {
        self.set_meta(HANDLES_FILESYSTEM, Some(filesystem))
    }

    /// The outbox commits so far.
    pub fn outbox_seq(&self) -> Result<i64, TreeError> {
        Ok(self.meta(OUTBOX_SEQ)?.and_then(|v| v.parse().ok()).unwrap_or(0))
    }

    /// The pause as stored: until then (unix seconds, 0 for until resumed),
    /// or `None`. A value that is no number reads as until resumed.
    pub(crate) fn paused_until(&self) -> Result<Option<i64>, TreeError> {
        Ok(self.meta(PAUSED_UNTIL)?.map(|v| v.parse::<i64>().unwrap_or(0).max(0)))
    }

    pub(crate) fn set_paused_until(&self, until: Option<i64>) -> Result<(), TreeError> {
        self.set_meta(PAUSED_UNTIL, until.map(|u| u.to_string()).as_deref())
    }
}
