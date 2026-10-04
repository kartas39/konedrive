//! What the outbox holds, summed by SQL (issue #38): the counts, sizes and
//! the Not Uploaded summary come from one `GROUP BY` over the rows' kind,
//! state and reason, and the files of one reason from a query with a `LIMIT`
//! — never from every row read into memory, never from the disk. A size is
//! the row's snapshot's, or the size the examination recorded.

use std::path::PathBuf;

use rusqlite::types::Value;

use super::{path_from, LocalSkip, OutboxKind, OutboxState, Reason};
use crate::{TreeError, TreeStore};

/// The bytes a row sends, in SQL: its snapshot's size (`<size> <mtime_ns>`),
/// or the size recorded when it was detected; nothing for what sends no content.
const BYTES: &str = "CASE WHEN kind IN ('create', 'update')
                      THEN COALESCE(CAST(substr(snapshot, 1, instr(snapshot, ' ') - 1) AS INTEGER), size, 0) ELSE 0 END";

/// Rows of one kind, state and reason, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxGroup {
    kind: String,
    state: String,
    reason: Option<String>,
    pub count: u64,
    /// What those rows send, summed.
    pub bytes: u64,
}

impl OutboxGroup {
    /// The kind as a row reads it: one no konedrive writes reads `update`,
    /// and its row is blocked ([`OutboxGroup::state`]).
    pub fn kind(&self) -> OutboxKind {
        OutboxKind::parse(&self.kind).unwrap_or(OutboxKind::Update)
    }

    /// The state as a row reads it: blocked when the kind or the state is
    /// one no konedrive writes.
    pub fn state(&self) -> OutboxState {
        match (OutboxKind::parse(&self.kind), OutboxState::parse(&self.state)) {
            (Some(_), Some(state)) => state,
            _ => OutboxState::Blocked,
        }
    }

    /// The reason as a row reads it.
    pub fn reason(&self) -> Option<Reason> {
        match (OutboxKind::parse(&self.kind), OutboxState::parse(&self.state)) {
            (None, _) => Some(Reason::Other(format!("unreadable kind {:?}", self.kind))),
            (_, None) => Some(Reason::Other(format!("unreadable state {:?}", self.state))),
            _ => self.reason.as_deref().map(Reason::parse),
        }
    }
}

/// What is never uploaded, of one reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedGroup {
    pub reason: LocalSkip,
    pub count: u64,
    pub bytes: u64,
}

impl TreeStore {
    /// The rows, grouped by kind, state and reason.
    pub fn outbox_groups(&self) -> Result<Vec<OutboxGroup>, TreeError> {
        let sql = format!("SELECT kind, state, reason, count(*), COALESCE(SUM({BYTES}), 0) FROM outbox GROUP BY kind, state, reason");
        let mut statement = self.conn.prepare_cached(&sql)?;
        let groups = statement
            .query_map([], |r| {
                Ok(OutboxGroup {
                    kind: r.get(0)?,
                    state: r.get(1)?,
                    reason: r.get(2)?,
                    count: r.get::<_, i64>(3)?.max(0) as u64,
                    bytes: r.get::<_, i64>(4)?.max(0) as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(groups)
    }

    /// What is never uploaded, grouped by reason.
    pub fn skipped_groups(&self) -> Result<Vec<SkippedGroup>, TreeError> {
        let mut statement = self.conn.prepare_cached("SELECT reason, count(*), COALESCE(SUM(size), 0) FROM local_skipped GROUP BY reason")?;
        let groups = statement
            .query_map([], |r| Ok(SkippedGroup { reason: r.get::<_, String>(0)?.into(), count: r.get::<_, i64>(1)?.max(0) as u64, bytes: r.get::<_, i64>(2)?.max(0) as u64 }))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(groups)
    }

    /// The places of the rows of `groups`, by place, at most `limit` (0 for all),
    /// each with its group.
    pub fn outbox_places_of(&self, groups: &[&OutboxGroup], limit: u32) -> Result<Vec<(PathBuf, usize)>, TreeError> {
        let mut out = Vec::new();
        for (n, group) in groups.iter().enumerate() {
            let sql = format!("SELECT rel FROM outbox WHERE kind = ?1 AND state = ?2 AND reason IS ?3 ORDER BY rel{}", limited(limit));
            let mut statement = self.conn.prepare_cached(&sql)?;
            let rels = statement
                .query_map(rusqlite::params![group.kind, group.state, group.reason], |r| Ok(path_from(r.get_ref(0)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            out.extend(rels.into_iter().map(|rel| (rel, n)));
        }
        Ok(out)
    }

    /// The places of what is never uploaded for one of `reasons`, by place, at
    /// most `limit` (0 for all), each with its reason.
    pub fn skipped_places_of(&self, reasons: &[&LocalSkip], limit: u32) -> Result<Vec<(PathBuf, LocalSkip)>, TreeError> {
        let mut out = Vec::new();
        for reason in reasons {
            let sql = format!("SELECT rel FROM local_skipped WHERE reason = ?1 ORDER BY rel{}", limited(limit));
            let mut statement = self.conn.prepare_cached(&sql)?;
            let rels = statement.query_map([Value::Text(reason.to_string())], |r| Ok(path_from(r.get_ref(0)?)))?.collect::<Result<Vec<_>, _>>()?;
            out.extend(rels.into_iter().map(|rel| (rel, (*reason).clone())));
        }
        Ok(out)
    }
}

fn limited(limit: u32) -> String {
    if limit == 0 {
        String::new()
    } else {
        format!(" LIMIT {limit}")
    }
}
