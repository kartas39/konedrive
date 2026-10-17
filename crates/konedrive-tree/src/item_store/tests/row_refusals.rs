//! The reason a refusal names ([`RowRefusalReason`]): one test per reason a caller answers
//! a program by, through a change of a local version and through the row written with
//! the going of a record.

use super::{made, make_directory, make_file, place};
use crate::model::{ItemKind, LocalItemId, LocalVersionChange};
use crate::{MadeEntryOwner, PlaceChangeRecord, PlaceChangeRowWrite, RowRefusalReason, TreeError, TreeStore};

const ROOT: LocalItemId = LocalItemId::ROOT;

/// The reason `result` was refused for; the text is kept beside it.
fn reason_of<T: std::fmt::Debug>(result: Result<T, TreeError>) -> RowRefusalReason {
    match result {
        Err(TreeError::Refused(refusal)) => {
            assert!(!refusal.text.is_empty(), "the text is kept");
            refusal.reason
        }
        other => panic!("not refused: {other:?}"),
    }
}

fn moved(item: LocalItemId, to: crate::model::ItemPlace) -> LocalVersionChange {
    LocalVersionChange::Moved { item, place: to, is_name_ignored: false }
}

#[test]
fn a_change_of_an_item_whose_row_is_gone_or_that_the_folder_does_not_show_is_refused_as_item_gone() {
    let mut store = TreeStore::in_memory().unwrap();
    let nowhere = LocalItemId::new(99);
    assert_eq!(reason_of(store.item_store().apply(&LocalVersionChange::Removed { item: nowhere })), RowRefusalReason::ItemGone);
    assert_eq!(reason_of(store.item_store().apply(&moved(nowhere, place(ROOT, "to")))), RowRefusalReason::ItemGone);
    let file = make_file(&mut store, ROOT, "f");
    assert_eq!(reason_of(store.item_store().apply(&LocalVersionChange::SavedOver { replacing: file, replaced: nowhere })), RowRefusalReason::ItemGone);
    assert_eq!(reason_of(store.item_store().apply(&LocalVersionChange::SavedOver { replacing: nowhere, replaced: file })), RowRefusalReason::ItemGone);
}

#[test]
fn a_change_to_a_name_the_folder_shows_another_item_under_is_refused_as_place_taken() {
    let mut store = TreeStore::in_memory().unwrap();
    make_file(&mut store, ROOT, "taken");
    let other = make_file(&mut store, ROOT, "other");
    assert_eq!(reason_of(store.item_store().apply(&made(place(ROOT, "taken"), ItemKind::Directory))), RowRefusalReason::PlaceTaken);
    assert_eq!(reason_of(store.item_store().apply(&moved(other, place(ROOT, "taken")))), RowRefusalReason::PlaceTaken);
}

#[test]
fn a_directory_the_folder_shows_something_in_is_refused_to_go_as_directory_not_empty() {
    let mut store = TreeStore::in_memory().unwrap();
    let held = make_directory(&mut store, ROOT, "held");
    make_file(&mut store, held, "inside");
    let other = make_directory(&mut store, ROOT, "other");
    assert_eq!(reason_of(store.item_store().apply(&LocalVersionChange::Removed { item: held })), RowRefusalReason::DirectoryNotEmpty);
    assert_eq!(reason_of(store.item_store().apply(&LocalVersionChange::SavedOver { replacing: other, replaced: held })), RowRefusalReason::DirectoryNotEmpty);
}

#[test]
fn a_change_into_a_directory_that_has_no_row_or_is_not_shown_is_refused_as_no_directory() {
    let mut store = TreeStore::in_memory().unwrap();
    let nowhere = LocalItemId::new(99);
    let file = make_file(&mut store, ROOT, "f");
    assert_eq!(reason_of(store.item_store().apply(&made(place(nowhere, "new"), ItemKind::Directory))), RowRefusalReason::NoDirectory);
    assert_eq!(reason_of(store.item_store().apply(&moved(file, place(nowhere, "f")))), RowRefusalReason::NoDirectory);
}

#[test]
fn a_rule_no_caller_answers_a_program_by_names_no_reason_and_keeps_its_text() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "f");
    // Into a file: no directory to answer `ENOENT` for.
    assert_eq!(reason_of(store.item_store().apply(&made(place(file, "new"), ItemKind::File))), RowRefusalReason::AnotherRule);
    let directory = make_directory(&mut store, ROOT, "d");
    assert_eq!(reason_of(store.item_store().apply(&moved(directory, place(directory, "d")))), RowRefusalReason::AnotherRule);
    let refused = store.item_store().apply(&made(place(ROOT, "f"), ItemKind::File)).unwrap_err();
    assert!(refused.to_string().starts_with("the tree store refuses: item "), "the text an error is printed as is the one it had: {refused}");
}

#[test]
fn the_row_written_with_the_going_of_a_record_is_refused_for_the_same_reasons() {
    let mut store = TreeStore::in_memory().unwrap();
    make_file(&mut store, ROOT, "taken");
    let nowhere = LocalItemId::new(99);
    let record = store.item_store().record_place_change(&PlaceChangeRecord::MakeEntry { kind: ItemKind::File, place: place(ROOT, "taken"), owner: MadeEntryOwner::NewItem { replaced: None } }).unwrap();
    let settle = |store: &mut TreeStore, write: PlaceChangeRowWrite| reason_of(store.item_store().settle_place_change_record(record, &write));
    assert_eq!(settle(&mut store, PlaceChangeRowWrite::LocalVersion(made(place(ROOT, "taken"), ItemKind::File))), RowRefusalReason::PlaceTaken);
    assert_eq!(settle(&mut store, PlaceChangeRowWrite::LocalVersion(made(place(nowhere, "new"), ItemKind::File))), RowRefusalReason::NoDirectory);
    assert_eq!(settle(&mut store, PlaceChangeRowWrite::LocalVersion(LocalVersionChange::Removed { item: nowhere })), RowRefusalReason::ItemGone);
    assert_eq!(settle(&mut store, PlaceChangeRowWrite::ContentBeforeWrite { item: nowhere }), RowRefusalReason::ItemGone);
    let held = make_directory(&mut store, ROOT, "held");
    make_file(&mut store, held, "inside");
    assert_eq!(settle(&mut store, PlaceChangeRowWrite::LocalVersion(LocalVersionChange::Removed { item: held })), RowRefusalReason::DirectoryNotEmpty);
    assert_eq!(store.item_store().list_place_change_records().unwrap().len(), 1, "the record stands");
}
