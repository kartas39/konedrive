//! The `user.konedrive.sync` attribute of the files the outbox's rows name (§9): what the
//! worker last wrote for each row, and writing it again where a row changed.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use konedrive_tree::outbox::{OutboxKind, OutboxRow, OutboxState};
use konedrive_tree::TreeError;

use super::Engine;
use crate::folder::disk::Disk;
use crate::upload::local::{self, SYNC_BLOCKED, SYNC_PENDING, SYNC_UPLOADING};

struct Mark {
    rel: PathBuf,
    value: &'static str,
    item: Option<String>,
}

/// The `user.konedrive.sync` value last written for each row, where, and the row's item.
#[derive(Default)]
pub(super) struct Marks {
    rows: HashMap<i64, Mark>,
    /// The marks were written for every row once: from then on, only for
    /// the rows that changed.
    read: bool,
}

impl Marks {
    /// Row `seq` changed its state: its mark is written again at the next look.
    pub fn forget(&mut self, seq: i64) {
        self.rows.remove(&seq);
    }
}

/// The `user.konedrive.sync` value for a row's file (§9).
fn wanted_mark(row: &OutboxRow) -> Option<&'static str> {
    if !matches!(row.kind, OutboxKind::Create | OutboxKind::Update | OutboxKind::Move) {
        return None;
    }
    Some(match row.state {
        OutboxState::Blocked => SYNC_BLOCKED,
        OutboxState::Held => return None,
        OutboxState::Running if row.kind.sends_content() => SYNC_UPLOADING,
        _ => SYNC_PENDING,
    })
}

impl Engine {
    /// Sets `user.konedrive.sync` on the files of rows whose state changed,
    /// and takes it off those whose row went: the rows written or removed
    /// since the last look ([`OutboxChanges`]), every row the first time.
    /// The attributes are written with no lock held. The counts follow.
    ///
    /// [`OutboxChanges`]: konedrive_tree::outbox::OutboxChanges
    fn mark_rows(&self, disk: &Disk) {
        let store = self.store();
        let first = !self.shared().marks.read;
        let dirty = store.changes().take_dirty();
        let (read, asked): (Result<Vec<OutboxRow>, TreeError>, Option<HashSet<i64>>) = match dirty.filter(|_| !first) {
            Some(seqs) if seqs.is_empty() => (Ok(Vec::new()), Some(seqs)),
            Some(seqs) => {
                let list: Vec<i64> = seqs.iter().copied().collect();
                (store.call_blocking(move |s| s.outbox_rows_of(&list)), Some(seqs))
            }
            None => (store.call_blocking(move |s| s.outbox_rows()), None),
        };
        let mut rows = match read {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("cannot read the outbox for its marks: {e}");
                // Every row, next time.
                self.shared().marks.read = false;
                return;
            }
        };
        let present: HashSet<i64> = rows.iter().map(|r| r.seq).collect();
        let gone: Vec<Mark> = {
            let mut shared = self.shared();
            shared.marks.read = true;
            let gone: Vec<i64> = match &asked {
                Some(seqs) => seqs.iter().filter(|seq| !present.contains(seq)).copied().collect(),
                None => shared.marks.rows.keys().filter(|seq| !present.contains(seq)).copied().collect(),
            };
            gone.into_iter().filter_map(|seq| shared.marks.rows.remove(&seq)).collect()
        };
        let mut cleared = HashSet::new();
        for mark in gone {
            local::mark(disk, &mark.rel, None);
            cleared.insert(mark.rel);
            // A move taken back (the file went back to its base place): the
            // mark is on the file there.
            if let Some(id) = mark.item {
                if let Ok(Some(at)) = store.call_blocking(move |s| s.locate(konedrive_tree::Table::Items, &id)) {
                    if !at.rel.as_os_str().is_empty() {
                        local::mark(disk, &at.rel, None);
                        cleared.insert(at.rel);
                    }
                }
            }
        }
        // A row behind the one that went writes its mark again.
        if !cleared.is_empty() {
            let again: Vec<i64> = {
                let mut shared = self.shared();
                let again: Vec<i64> = shared.marks.rows.iter().filter(|(_, m)| cleared.contains(&m.rel)).map(|(&seq, _)| seq).collect();
                for seq in &again {
                    shared.marks.rows.remove(seq);
                }
                again.into_iter().filter(|seq| !present.contains(seq)).collect()
            };
            if !again.is_empty() {
                match store.call_blocking(move |s| s.outbox_rows_of(&again)) {
                    Ok(behind) => rows.extend(behind),
                    Err(e) => tracing::debug!("the rows behind one that went keep no upload mark for now: {e}"),
                }
            }
        }
        let wanted: Vec<(PathBuf, &'static str)> = {
            let mut shared = self.shared();
            rows.iter()
                .filter_map(|row| {
                    let value = wanted_mark(row)?;
                    if shared.marks.rows.get(&row.seq).is_some_and(|m| m.rel == row.rel && m.value == value) {
                        return None;
                    }
                    shared.marks.rows.insert(row.seq, Mark { rel: row.rel.clone(), value, item: row.item_id.clone() });
                    Some((row.rel.clone(), value))
                })
                .collect()
        };
        for (rel, value) in wanted {
            local::mark(disk, &rel, Some(value));
        }
    }

    /// [`mark_rows`](Self::mark_rows) off the async runtime.
    pub(super) async fn mark_rows_blocking(self: &Arc<Self>, disk: &Arc<Disk>) {
        let (engine, disk) = (Arc::clone(self), Arc::clone(disk));
        if let Err(e) = tokio::task::spawn_blocking(move || engine.mark_rows(&disk)).await {
            tracing::warn!("the outbox's marks task failed: {e}");
        }
    }
}
