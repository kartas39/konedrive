//! The activity log, as the store keeps it.

use rusqlite::params;
use rusqlite::types::{ToSql, ToSqlOutput};

use super::{TreeError, TreeStore};

/// How many activity events the store keeps: the oldest go.
pub const ACTIVITY_KEPT: usize = 200;

/// What an event of the activity log records. Stored, and sent over D-Bus, as
/// its [name](Self::as_str): the spellings are the contract with the database
/// and with the clients (the window's table is keyed by them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActivityKind {
    /// A file was downloaded: opened, or `Hydrate`. Detail: its size.
    Downloaded,
    /// A file's space was freed up: `Dehydrate`, or `FreeUpSpace` for the
    /// whole folder at once. Detail: how much.
    Freed,
    /// Added in OneDrive, placed here by an incremental cycle.
    Added,
    /// Changed in OneDrive: a placeholder took the new version, or a
    /// downloaded file was replaced by it.
    Updated,
    /// Removed from OneDrive, and so from here.
    Removed,
    /// Moved or renamed in OneDrive. Detail: where it was.
    Moved,
    /// A listing, or a Full reconcile: one event for the whole folder.
    Listed,
    /// A local version moved out of the way. The path is where
    /// it was; the detail, where it is now.
    Conflict,
    /// A download that failed: a fill on open, or `Hydrate`. Detail: why —
    /// exactly "not enough disk space" when the disk is full.
    Failed,
    /// A file changed in OneDrive that could not be replaced here (spec
    /// §7.3); the old version stays. Detail: why — exactly "not enough disk
    /// space" when the disk cannot hold both versions.
    UpdateFailed,
    /// Content made or changed here went up to OneDrive (a folder made
    /// here too). Detail: its size, or "folder".
    Uploaded,
    /// Moved or renamed here, and so in OneDrive. Detail: where it was.
    CloudMoved,
    /// Deleted here, and so in OneDrive, to its recycle bin.
    CloudDeleted,
    /// A change made here that cannot go up until the user acts (a name
    /// OneDrive refuses, OneDrive full, a sign-in without write access):
    /// once per change and reason. Detail: the reason.
    UploadFailed,
    /// OneDrive's version was kept, or put back, where both sides changed
    /// one item (`docs/design/writes.md` §7). Detail: why.
    Restored,
    /// Made here, then removed here before its upload finished: it never
    /// goes up, and its rows leave the outbox. Detail: why.
    NotUploaded,
}

impl ActivityKind {
    /// Every kind there is, for whatever has to agree with them all (the
    /// window's guard in `konedrivectl`'s tests).
    pub const ALL: [ActivityKind; 16] = [
        Self::Downloaded,
        Self::Freed,
        Self::Added,
        Self::Updated,
        Self::Removed,
        Self::Moved,
        Self::Listed,
        Self::Conflict,
        Self::Failed,
        Self::UpdateFailed,
        Self::Uploaded,
        Self::CloudMoved,
        Self::CloudDeleted,
        Self::UploadFailed,
        Self::Restored,
        Self::NotUploaded,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Downloaded => "downloaded",
            Self::Freed => "freed",
            Self::Added => "added",
            Self::Updated => "updated",
            Self::Removed => "removed",
            Self::Moved => "moved",
            Self::Listed => "listed",
            Self::Conflict => "conflict",
            Self::Failed => "failed",
            Self::UpdateFailed => "update-failed",
            Self::Uploaded => "uploaded",
            Self::CloudMoved => "cloud-moved",
            Self::CloudDeleted => "cloud-deleted",
            Self::UploadFailed => "upload-failed",
            Self::Restored => "restored",
            Self::NotUploaded => "not-uploaded",
        }
    }

    /// The kind stored as `name`; `None` for a name no kind has.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == name)
    }
}

impl ToSql for ActivityKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

/// One event of the activity log, as stored: unix seconds, what it records,
/// a full path and a detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityRow {
    pub at: i64,
    pub kind: ActivityKind,
    pub path: String,
    pub detail: String,
}

impl TreeStore {
    /// Appends `events` to the activity log and drops all but
    /// the newest [`ACTIVITY_KEPT`], in one transaction.
    pub fn add_activity(&mut self, events: &[ActivityRow]) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        for event in events {
            tx.execute(
                "INSERT INTO activity (at, kind, path, detail) VALUES (?1, ?2, ?3, ?4)",
                params![event.at, event.kind, event.path, event.detail],
            )?;
        }
        tx.execute(
            "DELETE FROM activity WHERE id NOT IN (SELECT id FROM activity ORDER BY id DESC LIMIT ?1)",
            [ACTIVITY_KEPT as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The newest `limit` events, newest first. An event whose kind no
    /// [`ActivityKind`] names — another version's, a damaged one — is left
    /// out, and goes when newer events push it off the log.
    pub fn recent_activity(&self, limit: usize) -> Result<Vec<ActivityRow>, TreeError> {
        let mut statement =
            self.conn.prepare("SELECT at, kind, path, detail FROM activity ORDER BY id DESC LIMIT ?1")?;
        let rows = statement
            .query_map([limit.min(ACTIVITY_KEPT) as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(at, kind, path, detail)| Some(ActivityRow { at, kind: ActivityKind::parse(&kind)?, path, detail }))
            .collect())
    }
}
