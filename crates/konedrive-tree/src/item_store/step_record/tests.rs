use std::ffi::OsStr;

use super::*;
use crate::conflicts::ConflictKind;
use crate::item_store::tests::listed_drive::store_with_listed_root;
use crate::item_store::tests::shown_items::{assert_rules_hold, row, show_listed_file};
use crate::item_store::tests::place;
use crate::item_store::PlaceChangeRecord;
use crate::model::{ContentPresence, LocalItemId};
use crate::TreeStore;

const ROOT: LocalItemId = LocalItemId::ROOT;

fn conflict(rescued: &str) -> ConflictRow {
    ConflictRow { at: 77, original: "/folder/f".into(), rescued: rescued.into(), kind: ConflictKind::Copy }
}

/// The file `F`, shown as `f` with its content on disk, and the record of the move of its
/// entry to the conflict copy's name.
fn store_with_file_to_rename() -> (TreeStore, LocalItemId, PlaceChangeRecordId) {
    let mut store = store_with_listed_root();
    let file = show_listed_file(&mut store, "F", "R", "f", "h1");
    store.item_store().set_content_presence(file, ContentPresence::OnDisk).unwrap();
    let record = store.item_store().record_place_change(&PlaceChangeRecord::MoveEntry { item: file, from: place(ROOT, "f"), to: place(ROOT, "f (conflict)") }).unwrap();
    (store, file, record)
}

#[test]
fn a_conflict_copy_and_its_record_are_written_together() {
    let (mut store, file, record) = store_with_file_to_rename();
    let planned = ItemVersions::of(&row(&mut store, file));
    let outcome = store.item_store().commit_rename_to_conflict_copy_on_record(&planned, OsStr::new("f (conflict)"), false, &conflict("/folder/f (conflict)"), Some(record)).unwrap();

    assert_eq!(outcome, StepCommitOutcome::Written);
    assert_eq!(row(&mut store, file).local.unwrap().place, Some(place(ROOT, "f (conflict)")));
    assert_eq!(store.conflicts().unwrap(), [conflict("/folder/f (conflict)")]);
    assert_eq!(store.item_store().list_place_change_records().unwrap(), [], "the record of the place change went too");
    assert_rules_hold(&mut store);
}

#[test]
fn a_commit_that_finds_the_rows_changed_leaves_no_record() {
    let (mut store, file, record) = store_with_file_to_rename();
    let planned = ItemVersions::of(&row(&mut store, file));
    let stale = ItemVersions { content_presence: ContentPresence::NotOnDisk, ..planned.clone() };
    let before = row(&mut store, file);

    let outcome = store.item_store().commit_rename_to_conflict_copy_on_record(&stale, OsStr::new("f (conflict)"), false, &conflict("/folder/f (conflict)"), Some(record)).unwrap();
    assert_eq!(outcome, StepCommitOutcome::RowsChanged);
    // The name is another item's by now.
    crate::item_store::tests::make_file(&mut store, ROOT, "taken");
    let outcome = store.item_store().commit_rename_to_conflict_copy_on_record(&planned, OsStr::new("taken"), false, &conflict("/folder/taken"), Some(record)).unwrap();
    assert_eq!(outcome, StepCommitOutcome::RowsChanged);

    assert_eq!(row(&mut store, file), before);
    assert_eq!(store.conflicts().unwrap(), [], "no copy is on record that the rows do not say");
    assert_eq!(store.item_store().list_place_change_records().unwrap().len(), 1, "the record of the place change stands for the caller's undo");
}

/// The form over a transaction writes what `TreeStore::add_conflicts` writes, a copy kept
/// at one path again replacing its row; and nothing while the transaction is not committed.
#[test]
fn conflicts_added_in_a_transaction_are_those_the_stores_own_call_adds() {
    let mut store = TreeStore::in_memory().unwrap();
    let mut other = TreeStore::in_memory().unwrap();
    let later = ConflictRow { at: 99, ..conflict("/folder/a") };
    let rows = [conflict("/folder/a"), conflict("/folder/b"), later.clone()];
    other.add_conflicts(&rows).unwrap();

    let transaction = store.conn.transaction().unwrap();
    add_conflicts_in(&transaction, &rows).unwrap();
    drop(transaction);
    assert_eq!(store.conflicts().unwrap(), [], "dropped with its transaction");
    let transaction = store.conn.transaction().unwrap();
    add_conflicts_in(&transaction, &rows).unwrap();
    transaction.commit().unwrap();
    assert_eq!(store.conflicts().unwrap(), other.conflicts().unwrap());
    assert_eq!(store.conflicts().unwrap(), [later, conflict("/folder/b")]);
}
