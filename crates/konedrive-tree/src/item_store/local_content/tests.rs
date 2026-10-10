//! The mount's own writes of a file's local content, each over rows built by a listing and
//! the commit that shows the item; after each that is written the rules of the rows are asked.

use super::*;
use crate::item_store::tests::listed_drive::{listed_directory, listed_file, reference, store_with_listed_root, write_listing_of_changes};
use crate::item_store::tests::shown_items::{assert_rules_hold, bound, listed_content, row, show_listed_directory, show_listed_file};
use crate::item_store::tests::{is_refused, make_directory, make_file, place};
use crate::item_store::{MadeEntryOwner, PlaceChangeRecord};
use crate::model::{ContentPresence, Item, ItemKind};
use crate::TreeStore;

const ROOT: LocalItemId = LocalItemId::ROOT;

/// The file `F` of OneDrive, shown in the root as `f`, synced, its content on disk or not.
fn store_with_shown_file(content_presence: ContentPresence) -> (TreeStore, LocalItemId) {
    let mut store = store_with_listed_root();
    let file = show_listed_file(&mut store, "F", "R", "f", "h1");
    store.item_store().set_content_presence(file, content_presence).unwrap();
    (store, file)
}

fn local_content(row: &Item) -> Option<ContentVersion> {
    row.local.as_ref().and_then(|local| local.content.clone())
}

fn record_link(store: &mut TreeStore, item: LocalItemId, content: &ContentVersion) -> PlaceChangeRecordId {
    store.item_store().record_place_change(&PlaceChangeRecord::LinkFile { item, place: place(ROOT, "f"), content: content.clone() }).unwrap()
}

fn records(store: &mut TreeStore) -> Vec<PlaceChangeRecordId> {
    store.item_store().list_place_change_records().unwrap().into_iter().map(|(id, _)| id).collect()
}

#[test]
fn a_time_set_on_a_clean_copy_leaves_the_file_synced() {
    for content_presence in [ContentPresence::OnDisk, ContentPresence::NotOnDisk] {
        let (mut store, file) = store_with_shown_file(content_presence);
        let before = row(&mut store, file);
        store.item_store().set_local_modified_time(file, 5000, 7).unwrap();

        let after = row(&mut store, file);
        let with_time = ContentVersion::Hashed { size: 3, mtime: 5000, mtime_nsec: 7, hash: Some("h1".into()), ctag: Some("c-h1".into()) };
        assert_eq!(local_content(&after), Some(with_time.clone()), "the same content, told as before, with the time set");
        assert_eq!(after.synced_content, Some(with_time), "what was agreed on has the time too: the copy stays clean");
        assert_eq!(after.unsent_since, None, "nothing is unsent");
        assert_eq!(Item { local: before.local.clone(), synced_content: before.synced_content.clone(), ..after.clone() }, before, "nothing else of the row changed");
        assert_eq!(after.onedrive, before.onedrive, "OneDrive's version is OneDrive's");
        assert_rules_hold(&mut store);
    }
}

#[test]
fn a_time_set_on_changed_content_is_the_times_of_that_content() {
    let (mut store, file) = store_with_shown_file(ContentPresence::OnDisk);
    store.item_store().apply_all(&[LocalVersionChange::OpenedForWriting { item: file }, LocalVersionChange::ClosedChanged { item: file, size: 9, mtime: 300, mtime_nsec: 1 }]).unwrap();
    let before = row(&mut store, file);
    assert!(before.unsent_since.is_some());

    store.item_store().set_local_modified_time(file, 5000, 7).unwrap();
    let after = row(&mut store, file);
    assert_eq!(local_content(&after), Some(ContentVersion::ChangedHere { size: 9, mtime: 5000, mtime_nsec: 7, change_count: 1 }));
    assert_eq!(after.synced_content, Some(listed_content("h1")), "what was agreed on is another content: its time stays");
    assert_eq!(after.unsent_since, before.unsent_since, "the same unsent change");
    assert_rules_hold(&mut store);
}

#[test]
fn a_time_set_is_refused_while_a_writer_holds_the_file() {
    let (mut store, file) = store_with_shown_file(ContentPresence::OnDisk);
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: file }).unwrap();
    let held = row(&mut store, file);
    assert!(is_refused(store.item_store().set_local_modified_time(file, 5000, 7)));
    assert_eq!(row(&mut store, file), held, "the close writes the times");

    let directory = make_directory(&mut store, ROOT, "d");
    assert!(is_refused(store.item_store().set_local_modified_time(directory, 5000, 7)), "a directory has no content");
    assert!(is_refused(store.item_store().set_local_modified_time(ROOT, 5000, 7)));
    assert!(is_refused(store.item_store().set_local_modified_time(LocalItemId::new(999), 5000, 7)), "no row");
    write_listing_of_changes(&mut store, &[listed_file("N", "R", "never-shown", (3, "h2"), "e-N")]);
    let never_shown = bound(&mut store, "N").local_item_id;
    assert!(is_refused(store.item_store().set_local_modified_time(never_shown, 5000, 7)), "the folder does not show it");
}

#[test]
fn a_file_opened_for_writing_with_nothing_on_disk_is_on_disk_in_the_same_transaction() {
    let (mut store, file) = store_with_shown_file(ContentPresence::NotOnDisk);
    assert_eq!(store.item_store().apply_with_content_on_disk(&LocalVersionChange::OpenedForWriting { item: file }).unwrap(), file);
    let opened = row(&mut store, file);
    assert_eq!(opened.content_presence, ContentPresence::OnDisk);
    assert_eq!(local_content(&opened), Some(ContentVersion::ChangedHere { size: 3, mtime: 1000, mtime_nsec: 0, change_count: 1 }));
    assert_eq!(opened.content_before_write, Some(listed_content("h1")));
    assert_rules_hold(&mut store);

    // A truncation to nothing of a file that is not on disk, and its last close.
    let (mut store, file) = store_with_shown_file(ContentPresence::NotOnDisk);
    store.item_store().apply_with_content_on_disk(&LocalVersionChange::Truncated { item: file, size: 0, mtime: 300, mtime_nsec: 0 }).unwrap();
    let cut = row(&mut store, file);
    assert_eq!((cut.content_presence, local_content(&cut)), (ContentPresence::OnDisk, Some(ContentVersion::ChangedHere { size: 0, mtime: 300, mtime_nsec: 0, change_count: 1 })));
    assert_rules_hold(&mut store);
    store.item_store().apply_with_content_on_disk(&LocalVersionChange::ClosedChanged { item: file, size: 5, mtime: 400, mtime_nsec: 0 }).unwrap();
    assert_eq!(row(&mut store, file).content_presence, ContentPresence::OnDisk);
    assert_rules_hold(&mut store);
}

#[test]
fn a_refused_change_leaves_the_presence_as_it_was() {
    let (mut store, file) = store_with_shown_file(ContentPresence::NotOnDisk);
    let directory = make_directory(&mut store, ROOT, "d");
    let before = store.item_store().list_items().unwrap();

    assert!(is_refused(store.item_store().apply_with_content_on_disk(&LocalVersionChange::OpenedForWriting { item: directory })), "as `apply` refuses");
    assert!(is_refused(store.item_store().apply_with_content_on_disk(&LocalVersionChange::Removed { item: file })), "a change that is of no file's content");
    assert!(is_refused(store.item_store().apply_with_content_on_disk(&LocalVersionChange::Moved { item: file, place: place(directory, "g"), is_name_ignored: false })));
    let made_file = LocalVersionChange::Made { kind: ItemKind::File, place: place(ROOT, "new"), content: Some(listed_content("h9")), is_name_ignored: false, replaced: None };
    assert!(is_refused(store.item_store().apply_with_content_on_disk(&made_file)));
    assert_eq!(store.item_store().list_items().unwrap(), before, "nothing was written: no row made, moved or removed, and the content is not on disk");

    // A file that is gone, which a program still holds open: nothing, and no error.
    store.item_store().apply(&LocalVersionChange::Removed { item: file }).unwrap();
    let removed = row(&mut store, file);
    assert_eq!(store.item_store().apply_with_content_on_disk(&LocalVersionChange::OpenedForWriting { item: file }).unwrap(), file);
    assert_eq!(row(&mut store, file), removed);
    assert_eq!(store.item_store().apply_with_content_on_disk(&LocalVersionChange::OpenedForWriting { item: LocalItemId::new(999) }).unwrap(), LocalItemId::new(999));
    assert_rules_hold(&mut store);
}

fn made_content() -> ContentVersion {
    ContentVersion::ChangedHere { size: 0, mtime: 200, mtime_nsec: 7, change_count: 1 }
}

fn record_make(store: &mut TreeStore, replaced: Option<LocalItemId>) -> PlaceChangeRecordId {
    store.item_store().record_place_change(&PlaceChangeRecord::MakeEntry { kind: ItemKind::File, place: place(ROOT, "new"), owner: MadeEntryOwner::NewItem { replaced } }).unwrap()
}

#[test]
fn a_file_made_open_for_writing_is_made_and_opened_in_one_transaction() {
    let mut store = store_with_listed_root();
    let record = record_make(&mut store, None);
    let file = store.item_store().apply_made_open_for_writing(&place(ROOT, "new"), &made_content(), true, None, Some(record)).unwrap();

    let made = row(&mut store, file);
    assert_eq!(made.local.as_ref().and_then(|local| local.place.clone()), Some(place(ROOT, "new")));
    assert_eq!(local_content(&made), Some(ContentVersion::ChangedHere { size: 0, mtime: 200, mtime_nsec: 7, change_count: 2 }), "another content from the first writer's open");
    assert_eq!(made.content_before_write, Some(made_content()), "what a close that changed nothing puts back");
    assert_eq!((made.content_presence, made.is_name_ignored, made.onedrive_item.clone()), (ContentPresence::OnDisk, true, None));
    assert!(made.unsent_since.is_some());
    assert_eq!(records(&mut store), [], "the record of the place change went with the row");
    assert_rules_hold(&mut store);

    // Made in the place of a file removed here: it takes the binding over, as `Made` does.
    let removed = show_listed_file(&mut store, "F", "R", "f", "h1");
    store.item_store().apply(&LocalVersionChange::Removed { item: removed }).unwrap();
    let replacing = store.item_store().apply_made_open_for_writing(&place(ROOT, "f"), &made_content(), false, Some(removed), None).unwrap();
    let replacing = row(&mut store, replacing);
    assert_eq!(replacing.onedrive_item, Some(reference("F")));
    assert_eq!(replacing.content_before_write, Some(made_content()));
    assert_eq!(store.item_store().find_item(removed).unwrap(), None);
    assert_rules_hold(&mut store);
}

#[test]
fn a_file_made_open_for_writing_that_is_refused_leaves_no_row() {
    let mut store = store_with_listed_root();
    make_file(&mut store, ROOT, "new");
    let record = record_make(&mut store, None);
    let before = store.item_store().list_items().unwrap();

    assert!(is_refused(store.item_store().apply_made_open_for_writing(&place(ROOT, "new"), &made_content(), false, None, Some(record))), "the place is another item's");
    assert!(is_refused(store.item_store().apply_made_open_for_writing(&place(LocalItemId::new(999), "x"), &made_content(), false, None, Some(record))), "no such directory");
    assert!(is_refused(store.item_store().apply_made_open_for_writing(&place(ROOT, "other"), &made_content(), false, Some(ROOT), Some(record))), "what it is to replace cannot be replaced");
    assert_eq!(store.item_store().list_items().unwrap(), before, "no row was made");
    assert_eq!(records(&mut store), [record], "the record stands for the caller, who undoes the disk");
}

#[test]
fn the_files_several_steps_show_are_shown_in_one_transaction_each_by_its_own_versions() {
    let mut store = store_with_listed_root();
    write_listing_of_changes(
        &mut store,
        &[
            listed_directory("D", "R", "d", "e-D"),
            listed_file("F1", "D", "f1", (3, "h1"), "e-F1"),
            listed_file("F2", "R", "f2", (3, "h2"), "e-F2"),
            listed_file("F3", "R", "f3", (3, "h3"), "e-F3"),
            listed_directory("E", "R", "e", "e-E"),
            listed_file("F4", "E", "f4", (3, "h4"), "e-F4"),
        ],
    );
    let planned = |store: &mut TreeStore, id: &str| ItemVersions::of(&bound(store, id));
    let stale = |store: &mut TreeStore, id: &str| ItemVersions { onedrive_etag: Some("an-older-etag".into()), ..planned(store, id) };
    let steps = [
        (planned(&mut store, "D"), false),
        (planned(&mut store, "F1"), true),
        (stale(&mut store, "F2"), false),
        (planned(&mut store, "F3"), false),
        (stale(&mut store, "E"), false),
        (planned(&mut store, "F4"), false),
    ];

    let outcomes = store.item_store().commit_show_items(&steps).unwrap();
    use StepCommitOutcome::{RowsChanged, Written};
    assert_eq!(outcomes, [Written, Written, RowsChanged, Written, RowsChanged, RowsChanged], "in the order given; a file whose directory was not shown is not shown either");
    let shown = |store: &mut TreeStore, id: &str| bound(store, id).local.is_some();
    assert_eq!(["D", "F1", "F2", "F3", "E", "F4"].map(|id| shown(&mut store, id)), [true, true, false, true, false, false]);
    let f1 = bound(&mut store, "F1");
    assert_eq!((f1.is_name_ignored, f1.content_presence, f1.synced_content.clone(), f1.unsent_since), (true, ContentPresence::NotOnDisk, Some(listed_content("h1")), None));
    assert_eq!(store.item_store().find_path(f1.local_item_id).unwrap(), Some("d/f1".into()));
    assert_rules_hold(&mut store);
    assert_eq!(store.item_store().commit_show_items(&[]).unwrap(), []);

    // One transaction: a step no plan asks for fails the whole call, and nothing of it is written.
    let already_shown = planned(&mut store, "F3");
    let steps = [(planned(&mut store, "E"), false), (planned(&mut store, "F4"), false), (already_shown, false)];
    assert!(is_refused(store.item_store().commit_show_items(&steps)));
    assert_eq!(["E", "F4"].map(|id| shown(&mut store, id)), [false, false]);
}

#[test]
fn a_landing_is_written_when_the_local_content_is_still_the_one_downloaded() {
    let (mut store, file) = store_with_shown_file(ContentPresence::NotOnDisk);
    let before = row(&mut store, file);
    let record = record_link(&mut store, file, &listed_content("h1"));

    assert_eq!(store.item_store().commit_landed_content(file, &listed_content("h1"), Some(record)).unwrap(), StepCommitOutcome::Written);
    assert_eq!(row(&mut store, file), Item { content_presence: ContentPresence::OnDisk, ..before }, "on disk, and nothing else");
    assert_eq!(records(&mut store), [], "the record went in the commit's transaction");
    assert_rules_hold(&mut store);
    assert_eq!(store.item_store().commit_landed_content(file, &listed_content("h1"), None).unwrap(), StepCommitOutcome::Written, "a second landing of the same content");
}

#[test]
fn a_landing_of_content_the_row_no_longer_has_writes_nothing() {
    let (mut store, file) = store_with_shown_file(ContentPresence::NotOnDisk);
    let record = record_link(&mut store, file, &listed_content("h1"));
    let before = row(&mut store, file);
    let land = |store: &mut TreeStore, item, content: &ContentVersion| store.item_store().commit_landed_content(item, content, Some(record)).unwrap();

    assert_eq!(land(&mut store, file, &listed_content("another")), StepCommitOutcome::RowsChanged);
    assert_eq!(land(&mut store, LocalItemId::new(999), &listed_content("h1")), StepCommitOutcome::RowsChanged, "the row is gone");
    let directory = make_directory(&mut store, ROOT, "d");
    assert_eq!(land(&mut store, directory, &listed_content("h1")), StepCommitOutcome::RowsChanged);
    assert_eq!(row(&mut store, file), before);

    // A listing brought another content and the folder took it while the download ran.
    write_listing_of_changes(&mut store, &[listed_file("F", "R", "f", (3, "h2"), "e-F-2")]);
    let changed = ItemVersions::of(&row(&mut store, file));
    assert_eq!(store.item_store().commit_take_onedrive_content(&changed, None).unwrap(), StepCommitOutcome::Written);
    assert_eq!(land(&mut store, file, &listed_content("h1")), StepCommitOutcome::RowsChanged);
    assert_eq!(row(&mut store, file).content_presence, ContentPresence::NotOnDisk);

    // A program removed the file while the download ran.
    store.item_store().apply(&LocalVersionChange::Removed { item: file }).unwrap();
    assert_eq!(land(&mut store, file, &listed_content("h2")), StepCommitOutcome::RowsChanged);
    assert_eq!(row(&mut store, file).content_presence, ContentPresence::NotOnDisk);
    assert_eq!(records(&mut store), [record], "the record stands: the caller removes the file it landed and takes it");
    assert_rules_hold(&mut store);
}

#[test]
fn a_copy_is_freed_in_the_row_only_while_its_content_is_the_synced_one() {
    let (mut store, file) = store_with_shown_file(ContentPresence::OnDisk);
    let before = row(&mut store, file);
    assert_eq!(store.item_store().commit_freed_content(file, &listed_content("h1")).unwrap(), StepCommitOutcome::Written);
    assert_eq!(row(&mut store, file), Item { content_presence: ContentPresence::NotOnDisk, ..before }, "not on disk, and nothing else");
    assert_rules_hold(&mut store);

    // A copy whose time a program set is clean still, and freed as the content with that time.
    let (mut store, file) = store_with_shown_file(ContentPresence::OnDisk);
    store.item_store().set_local_modified_time(file, 5000, 0).unwrap();
    assert_eq!(store.item_store().commit_freed_content(file, &listed_content("h1")).unwrap(), StepCommitOutcome::RowsChanged, "asked about the content of before the time was set");
    let with_time = local_content(&row(&mut store, file)).unwrap();
    assert_eq!(store.item_store().commit_freed_content(file, &with_time).unwrap(), StepCommitOutcome::Written);
    assert_rules_hold(&mut store);
}

#[test]
fn a_copy_that_was_changed_since_it_was_asked_about_is_not_freed() {
    let (mut store, file) = store_with_shown_file(ContentPresence::OnDisk);
    let free = |store: &mut TreeStore, item, content: &ContentVersion| store.item_store().commit_freed_content(item, content).unwrap();
    assert_eq!(free(&mut store, file, &listed_content("another")), StepCommitOutcome::RowsChanged);
    assert_eq!(free(&mut store, LocalItemId::new(999), &listed_content("h1")), StepCommitOutcome::RowsChanged);

    // A writer holds it: the local content is another, and the one before the write is kept.
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: file }).unwrap();
    let opened = local_content(&row(&mut store, file)).unwrap();
    assert_eq!(free(&mut store, file, &listed_content("h1")), StepCommitOutcome::RowsChanged);
    assert_eq!(free(&mut store, file, &opened), StepCommitOutcome::RowsChanged);
    // Closed changed: the user's bytes, which OneDrive does not have.
    store.item_store().apply(&LocalVersionChange::ClosedChanged { item: file, size: 9, mtime: 300, mtime_nsec: 0 }).unwrap();
    let changed = local_content(&row(&mut store, file)).unwrap();
    assert_eq!(free(&mut store, file, &changed), StepCommitOutcome::RowsChanged);
    assert_eq!(row(&mut store, file).content_presence, ContentPresence::OnDisk);

    // A file made here, which nothing was agreed on of.
    let made_file = make_file(&mut store, ROOT, "made");
    let made_content = local_content(&row(&mut store, made_file)).unwrap();
    assert_eq!(free(&mut store, made_file, &made_content), StepCommitOutcome::RowsChanged);
    assert_eq!(row(&mut store, made_file).content_presence, ContentPresence::OnDisk);
    let directory = show_listed_directory(&mut store, "D", "R", "d");
    assert_eq!(free(&mut store, directory, &listed_content("h1")), StepCommitOutcome::RowsChanged);
    assert_rules_hold(&mut store);
}
