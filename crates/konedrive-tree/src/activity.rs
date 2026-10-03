//! The activity log, as the store keeps it.

use rusqlite::params;

use super::{TreeError, TreeStore};

/// How many activity events the store keeps: the oldest go.
pub const ACTIVITY_KEPT: usize = 200;

/// One event of the activity log, as stored: unix seconds, one
/// of the kinds `sync::activity::Kind` names, a full path and a detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityRow {
    pub at: i64,
    pub kind: String,
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

    /// The newest `limit` events, newest first.
    pub fn recent_activity(&self, limit: usize) -> Result<Vec<ActivityRow>, TreeError> {
        let mut statement =
            self.conn.prepare("SELECT at, kind, path, detail FROM activity ORDER BY id DESC LIMIT ?1")?;
        let rows = statement
            .query_map([limit.min(ACTIVITY_KEPT) as i64], |row| {
                Ok(ActivityRow { at: row.get(0)?, kind: row.get(1)?, path: row.get(2)?, detail: row.get(3)? })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
