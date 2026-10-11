//! The plan's readers over the read-only view: each against the store's reader of the same
//! name, over rows built by the store's own operations, and against what those rows were
//! built to answer.

use std::collections::BTreeSet;

use super::*;
use crate::item_store::tests::listed_drive::{listed_file, not_placed, store_with_listed_root, write_listing_of_changes};
use crate::item_store::tests::shown_items::{bound, show_listed_directory, show_listed_file};
use crate::item_store::tests::{make_file, place};
use crate::model::{ContentPresence, ContentVersion, LocalVersionChange};
use crate::sync_plan::ItemVersions;
use crate::{StepCommitOutcome, TreeStore};

const ROOT: LocalItemId = LocalItemId::ROOT;
const NOW: i64 = 1_700_000_000;

fn ids(items: &[Item]) -> Vec<LocalItemId> {
    items.iter().map(|item| item.local_item_id).collect()
}

fn sorted(mut items: Vec<LocalItemId>) -> Vec<LocalItemId> {
    items.sort();
    items
}

fn apply(store: &mut TreeStore, change: LocalVersionChange) {
    store.item_store().apply(&change).unwrap();
}

/// A directory `a` with the file `f` and the directory `sub` {`g`}, all shown; `p` pinned
/// and not on disk in the root; `gone` and the directory `d` {`in-d`} removed here, and
/// one removal sent; `moved` renamed by OneDrive; `hidden` shown where the folder cannot
/// show it any more; `new` made by OneDrive and never shown; `own` made here.
struct Rows {
    store: TreeStore,
    a: LocalItemId,
    sub: LocalItemId,
    f: LocalItemId,
    g: LocalItemId,
    p: LocalItemId,
    gone: LocalItemId,
    d: LocalItemId,
    in_d: LocalItemId,
    moved: LocalItemId,
    hidden: LocalItemId,
    new: LocalItemId,
}

fn rows() -> Rows {
    let mut store = store_with_listed_root();
    let a = show_listed_directory(&mut store, "A", "R", "a");
    let f = show_listed_file(&mut store, "F", "A", "f", "h-f");
    let sub = show_listed_directory(&mut store, "S", "A", "sub");
    let g = show_listed_file(&mut store, "G", "S", "g", "h-g");
    let p = show_listed_file(&mut store, "P", "R", "p", "h-p");
    let gone = show_listed_file(&mut store, "X", "R", "gone", "h-x");
    let sent = show_listed_file(&mut store, "Y", "R", "sent", "h-y");
    let d = show_listed_directory(&mut store, "D", "R", "d");
    let in_d = show_listed_file(&mut store, "I", "D", "in-d", "h-i");
    let moved = show_listed_file(&mut store, "M", "R", "moved", "h-m");
    let hidden = show_listed_file(&mut store, "H", "R", "hidden", "h-h");
    make_file(&mut store, a, "own");
    store.set_pin("P", true).unwrap();
    for item in [gone, sent, in_d, d] {
        apply(&mut store, LocalVersionChange::Removed { item });
    }
    let planned_from = ItemVersions::of(&store.item_store().find_item(sent).unwrap().unwrap());
    assert_eq!(store.item_store().commit_send_file_removal(&planned_from, NOW).unwrap(), StepCommitOutcome::Written);
    write_listing_of_changes(&mut store, &[listed_file("M", "R", "Moved-There", (3, "h-m"), "e-M2"), not_placed(listed_file("H", "R", "hidden", (3, "h-h"), "e-H2")), listed_file("N", "A", "F", (3, "h-n"), "e-N")]);
    let new = bound(&mut store, "N").local_item_id;
    Rows { store, a, sub, f, g, p, gone, d, in_d, moved, hidden, new }
}

#[test]
fn each_plan_reader_answers_what_the_item_stores_reader_of_the_same_name_answers() {
    let Rows { mut store, a, sub, f, g, p, gone, d, in_d, moved, hidden, new } = rows();
    let confirmed: BTreeSet<LocalItemId> = [gone].into();
    let folded = FoldedName::of("f".as_ref());
    let items = store.item_store();
    let reader = items.reader();

    // What the rows were built to answer, not two answers that are empty alike.
    assert_eq!(ids(&reader.list_changed_in_onedrive(None, 10).unwrap()), [moved, new]);
    assert_eq!(ids(&reader.list_changed_in_onedrive(Some(moved), 10).unwrap()), [new]);
    assert_eq!(ids(&reader.list_changed_in_onedrive(None, 1).unwrap()), [moved]);
    assert_eq!(ids(&reader.list_pinned_files_to_download(None, 10).unwrap()), [p]);
    assert_eq!(ids(&reader.list_pinned_files_to_download(Some(p), 10).unwrap()), []);
    assert_eq!(ids(&reader.list_shown_items_that_cannot_be_shown(None, 10).unwrap()), [hidden]);
    assert_eq!(ids(&reader.list_shown_items_that_cannot_be_shown(Some(hidden), 10).unwrap()), []);
    assert_eq!(ids(&reader.list_children_in_onedrive(a).unwrap()), [new, f, sub], "by name in OneDrive: F, f, sub");
    assert_eq!(sorted(ids(&reader.list_below_in_onedrive(a).unwrap())), sorted(vec![f, sub, g, new]));
    assert_eq!(ids(&reader.list_below_in_onedrive(d).unwrap()), [in_d]);
    assert_eq!(ids(&reader.list_children_in_onedrive_with_folded_name(a, &folded).unwrap()), [f, new], "one name as OneDrive compares names");
    assert_eq!(reader.count_shown_items().unwrap(), 8, "a, f, sub, g, p, moved, hidden, own");
    assert_eq!(reader.count_unsent_removals(&BTreeSet::new()).unwrap(), 3, "gone, d and what OneDrive has in d");
    assert_eq!(reader.count_unsent_removals(&confirmed).unwrap(), 2);
    assert_eq!(reader.find_sent_removal_window(NOW + 1).unwrap(), SentRemovalWindow { last_removal_at: Some(NOW), removal_count: 1 });
    assert_eq!(reader.find_sent_removal_window(NOW + crate::SENT_REMOVAL_WINDOW_SECONDS).unwrap(), SentRemovalWindow::EMPTY);
    assert_eq!(reader.confirmation_count().unwrap(), 1, "the one removal OneDrive confirmed");

    // And each is the store's reader of the same name.
    for (after, limit) in [(None, 10), (None, 1), (Some(moved), 10), (Some(p), 10), (Some(hidden), 10)] {
        assert_eq!(reader.list_changed_in_onedrive(after, limit).unwrap(), items.list_changed_in_onedrive(after, limit).unwrap());
        assert_eq!(reader.list_pinned_files_to_download(after, limit).unwrap(), items.list_pinned_files_to_download(after, limit).unwrap());
        assert_eq!(reader.list_shown_items_that_cannot_be_shown(after, limit).unwrap(), items.list_shown_items_that_cannot_be_shown(after, limit).unwrap());
    }
    for directory in [ROOT, a, sub, d, f] {
        assert_eq!(reader.list_children_in_onedrive(directory).unwrap(), items.list_children_in_onedrive(directory).unwrap());
        assert_eq!(reader.list_below_in_onedrive(directory).unwrap(), items.list_below_in_onedrive(directory).unwrap());
        assert_eq!(reader.list_children_in_onedrive_with_folded_name(directory, &folded).unwrap(), items.list_children_in_onedrive_with_folded_name(directory, &folded).unwrap());
    }
    assert_eq!(reader.count_shown_items().unwrap(), items.count_shown_items().unwrap());
    for confirmed in [BTreeSet::new(), confirmed, [d].into()] {
        assert_eq!(reader.count_unsent_removals(&confirmed).unwrap(), items.count_unsent_removals(&confirmed).unwrap());
    }
    for now in [NOW, NOW + crate::SENT_REMOVAL_WINDOW_SECONDS] {
        assert_eq!(reader.find_sent_removal_window(now).unwrap(), items.find_sent_removal_window(now).unwrap());
    }
    assert_eq!(reader.confirmation_count().unwrap(), items.confirmation_count().unwrap());
}

/// What a plan is told is open for writing is read off the rows it is given: a row keeps
/// the content from before its first writer until the last writer closed, and a file with
/// a writer is among the unsent items, whatever OneDrive did to it meanwhile.
#[test]
fn the_items_whose_writer_has_not_closed_are_those_with_a_content_before_the_write() {
    let mut store = store_with_listed_root();
    let open = show_listed_file(&mut store, "A", "R", "open", "h-a");
    let open_and_changed = show_listed_file(&mut store, "B", "R", "open-and-changed", "h-b");
    let closed_unchanged = show_listed_file(&mut store, "C", "R", "closed-unchanged", "h-c");
    let closed_changed = show_listed_file(&mut store, "E", "R", "closed-changed", "h-e");
    let made_open = store.item_store().apply_made_open_for_writing(&place(ROOT, "made-open"), &ContentVersion::ChangedHere { size: 0, mtime: 300, mtime_nsec: 0, change_count: 1 }, false, None, None).unwrap();
    for item in [open, open_and_changed, closed_unchanged, closed_changed] {
        store.item_store().set_content_presence(item, ContentPresence::OnDisk).unwrap();
        apply(&mut store, LocalVersionChange::OpenedForWriting { item });
    }
    apply(&mut store, LocalVersionChange::ClosedUnchanged { item: closed_unchanged });
    apply(&mut store, LocalVersionChange::ClosedChanged { item: closed_changed, size: 9, mtime: 300, mtime_nsec: 1 });
    write_listing_of_changes(&mut store, &[listed_file("B", "R", "open-and-changed", (5, "h-b2"), "e-B2")]);

    let items = store.item_store();
    let reader = items.reader();
    let open_for_writing = |rows: Vec<Item>| ids(&rows.into_iter().filter(|row| row.content_before_write.is_some()).collect::<Vec<_>>());
    let unsent = reader.list_unsent(None, 10).unwrap();
    assert_eq!(sorted(ids(&unsent)), sorted(vec![open, open_and_changed, closed_changed, made_open]), "a file with a writer is unsent; one closed unchanged is not");
    assert_eq!(sorted(open_for_writing(unsent)), sorted(vec![open, open_and_changed, made_open]));
    assert_eq!(open_for_writing(reader.list_changed_in_onedrive(None, 10).unwrap()), [open_and_changed], "also as OneDrive changed it");
    assert_eq!(reader.find_item(closed_unchanged).unwrap().unwrap().content_before_write, None);
}
