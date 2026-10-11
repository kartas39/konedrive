//! What a step leaves on record beside the rows it wrote: the conflict copy it made and
//! the event of the activity log, in the transaction of its commit.
//!
//! **State owned:** none: the tables `conflicts` and `activity` are their own modules'.
//! This module only puts their rows into the transaction of a step's commit, as today's
//! `outbox_copied` and `outbox_commit` do (`outbox/worker.rs:426-431`, `outbox.rs:509`), so
//! that a stop never leaves a conflict copy nothing knows of.
//! One transaction for the conflict's record, where a loss costs the user the notice of
//! a copy ([`add_conflicts_in`], the form of `TreeStore::add_conflicts` over a
//! transaction); the plain events of the activity log are a second write by the plan's
//! task (`sync::sync_plan_task::step_settlement`), which a death in between loses.
//!
//! **Tested without the rest:**
// test: a_conflict_copy_and_its_record_are_written_together
// test: a_commit_that_finds_the_rows_changed_leaves_no_record
// test: conflicts_added_in_a_transaction_are_those_the_stores_own_call_adds
use std::ffi::OsStr;

use rusqlite::{params, Transaction};

use super::place_change_records::forget_place_change_record_when_written;
use super::step_commit::{commit_rename_to_conflict_copy, forget_tries_when_written};
use super::{ItemStore, PlaceChangeRecordId, StepCommitOutcome};
use crate::conflicts::ConflictRow;
use crate::sync_plan::ItemVersions;
use crate::TreeError;

/// Writes `conflicts` to the table `conflicts` inside `tx`: what `TreeStore::add_conflicts`
/// (`conflicts.rs:62`) writes in a transaction of its own. Called by the commit below.
pub(crate) fn add_conflicts_in(tx: &Transaction<'_>, conflicts: &[ConflictRow]) -> Result<(), TreeError> {
    let mut statement = tx.prepare_cached(
        "INSERT INTO conflicts (rescued, at, original, kind) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(rescued) DO UPDATE SET at = excluded.at, original = excluded.original, kind = excluded.kind",
    )?;
    for conflict in conflicts {
        statement.execute(params![conflict.rescued, conflict.at, conflict.original, conflict.kind.as_str()])?;
    }
    Ok(())
}

impl ItemStore<'_> {
    /// [`commit_rename_to_conflict_copy`](Self::commit_rename_to_conflict_copy) with the
    /// record of the copy written in the same transaction, when the commit is written.
    /// `conflict` holds the two full paths as the step renamed on disk.
    ///
    /// Called by the runner of `FolderStep::RenameToConflictCopy`
    /// (`fuse::folder_steps`), under the tree lock, after the rename on disk.
    /// `record` is the record of the place change this commit ends, when one was written:
    /// it goes in this transaction when the commit is written
    /// (`place_change_records::forget_place_change_record_when_written`, or
    /// `forget_place_change_record` for a write that has no outcome), and stands otherwise.
    pub fn commit_rename_to_conflict_copy_on_record(&mut self, item: &ItemVersions, name: &OsStr, is_name_ignored: bool, conflict: &ConflictRow, record: Option<PlaceChangeRecordId>) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| {
            let outcome = forget_tries_when_written(tx, item, commit_rename_to_conflict_copy(tx, item, name, is_name_ignored)?)?;
            if outcome == StepCommitOutcome::Written {
                add_conflicts_in(tx, std::slice::from_ref(conflict))?;
            }
            forget_place_change_record_when_written(tx, record, outcome)
        })
    }
}

#[cfg(test)]
mod tests;
