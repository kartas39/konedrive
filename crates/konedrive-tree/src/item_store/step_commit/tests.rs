//! A test per case of a step: what its commit leaves in the rows, built by the store's own
//! operations — a listing, a change of a local version, another commit. Each commit is
//! first given versions that are not the row's any more, and must then write nothing
//! ([`commit`]); after each that is written the rules of the rows are asked.

use super::*;
use crate::item_store::tests::listed_drive::{listed_directory, listed_file, listed_gone, not_placed, reference, store_with_listed_root, write_listing_of_changes};
use crate::item_store::tests::row_rules::find_broken_rule;
use crate::item_store::{PlaceChangeRecord, PlaceChangeRecordId};
use crate::model::{ItemKind, LocalVersionChange, Placement};
use crate::TreeStore;

const ROOT: LocalItemId = LocalItemId::ROOT;

fn place(parent: LocalItemId, name: &str) -> ItemPlace {
    ItemPlace { parent_local_item_id: parent, name: name.into() }
}

/// The content a listing gives as `(size, hash)`, as a row has it.
fn listed_content(size: u64, hash: &str) -> ContentVersion {
    ContentVersion::Hashed { size, mtime: 1000, mtime_nsec: 0, hash: Some(hash.to_owned()), ctag: Some(format!("c-{hash}")) }
}

/// A content OneDrive answered an upload with.
fn confirmed_content(size: u64, hash: &str) -> ContentVersion {
    ContentVersion::Hashed { size, mtime: 200, mtime_nsec: 0, hash: Some(hash.to_owned()), ctag: Some(format!("c-{hash}")) }
}

fn confirmed(item_id: &str, parent_id: &str, name: &str, content: Option<ContentVersion>, etag: &str) -> ConfirmedItem {
    ConfirmedItem { item: reference(item_id), parent: reference(parent_id), name: name.into(), content, etag: etag.to_owned() }
}

fn row(store: &mut TreeStore, id: LocalItemId) -> Item {
    store.item_store().find_item(id).unwrap().unwrap_or_else(|| panic!("item {id} has no row"))
}

/// The row of the item of OneDrive `item_id`.
fn bound(store: &mut TreeStore, item_id: &str) -> Item {
    store.item_store().find_bound_item(&reference(item_id)).unwrap().unwrap_or_else(|| panic!("the item of OneDrive {item_id} has no row"))
}

fn versions(store: &mut TreeStore, id: LocalItemId) -> ItemVersions {
    ItemVersions::of(&row(store, id))
}

/// What says whether anything was written: every row, and the count of confirmations.
fn written(store: &mut TreeStore) -> (Vec<Item>, u64) {
    (store.item_store().list_items().unwrap(), store.item_store().confirmation_count().unwrap())
}

/// Runs the commit `commit_step` of a step of the item `id` twice: with versions that are not
/// the row's, which answers that the rows changed and writes nothing; and with the row's,
/// which writes, and leaves rows that keep their rules.
fn commit(store: &mut TreeStore, id: LocalItemId, commit_step: impl Fn(&mut ItemStore<'_>, &ItemVersions) -> Result<StepCommitOutcome, TreeError>) {
    let planned_from = versions(store, id);
    let before = written(store);
    let outdated = ItemVersions { onedrive_etag: Some("e-outdated".to_owned()), ..planned_from.clone() };
    assert_eq!(commit_step(&mut store.item_store(), &outdated).unwrap(), StepCommitOutcome::RowsChanged);
    assert_eq!(written(store), before, "a commit that answers that the rows changed writes nothing");
    assert_eq!(commit_step(&mut store.item_store(), &planned_from).unwrap(), StepCommitOutcome::Written);
    assert_eq!(find_broken_rule(&store.item_store().list_items().unwrap(), false), None);
}

/// [`commit`] for a step that changed a name in the backing directory and wrote a record of
/// it before: the commit that answers that the rows changed leaves the record standing,
/// for the caller to take after its undo; the one that is written takes it in its
/// transaction.
fn commit_taking_record(store: &mut TreeStore, id: LocalItemId, commit_step: impl Fn(&mut ItemStore<'_>, &ItemVersions, Option<PlaceChangeRecordId>) -> Result<StepCommitOutcome, TreeError>) {
    let place = row(store, id).local.and_then(|local| local.place).expect("the folder shows it");
    let record = store.item_store().record_place_change(&PlaceChangeRecord::SetAside { item: id, place, aside_name: ".konedrive-aside".into() }).unwrap();
    let standing = |store: &mut TreeStore| store.item_store().list_place_change_records().unwrap().len();
    let planned_from = versions(store, id);
    let before = written(store);
    let outdated = ItemVersions { onedrive_etag: Some("e-outdated".to_owned()), ..planned_from.clone() };
    assert_eq!(commit_step(&mut store.item_store(), &outdated, Some(record)).unwrap(), StepCommitOutcome::RowsChanged);
    assert_eq!((written(store), standing(store)), (before, 1), "a commit that answers that the rows changed writes nothing and leaves the record");
    assert_eq!(commit_step(&mut store.item_store(), &planned_from, Some(record)).unwrap(), StepCommitOutcome::Written);
    assert_eq!(standing(store), 0, "a commit that is written takes the record of its place change");
    assert_eq!(find_broken_rule(&store.item_store().list_items().unwrap(), false), None);
}

/// `commit_step` answers that the rows changed, and writes nothing.
fn is_left_unwritten(store: &mut TreeStore, commit_step: impl Fn(&mut ItemStore<'_>) -> Result<StepCommitOutcome, TreeError>) -> bool {
    let before = written(store);
    let outcome = commit_step(&mut store.item_store()).unwrap();
    assert_eq!(written(store), before, "a commit that answers that the rows changed writes nothing");
    outcome == StepCommitOutcome::RowsChanged
}

fn make(store: &mut TreeStore, kind: ItemKind, parent: LocalItemId, name: &str) -> LocalItemId {
    let content = (kind == ItemKind::File).then_some(ContentVersion::ChangedHere { size: 5, mtime: 200, mtime_nsec: 7, change_count: 1 });
    store.item_store().apply(&LocalVersionChange::Made { kind, place: place(parent, name), content, is_name_ignored: false, replaced: None }).unwrap()
}

fn move_here(store: &mut TreeStore, item: LocalItemId, parent: LocalItemId, name: &str) {
    store.item_store().apply(&LocalVersionChange::Moved { item, place: place(parent, name), is_name_ignored: false }).unwrap();
}

/// A program changes the content of the file `item`, which is then of `size` bytes.
fn write_here(store: &mut TreeStore, item: LocalItemId, size: u64) {
    store.item_store().set_content_presence(item, ContentPresence::OnDisk).unwrap();
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item }).unwrap();
    store.item_store().apply(&LocalVersionChange::ClosedChanged { item, size, mtime: 300, mtime_nsec: 9 }).unwrap();
}

/// The file `item_id` of OneDrive, in the root under `name` with a content of 3 bytes and
/// the hash `h-<item_id>`, listed and shown by the folder: its local item id.
fn show_file(store: &mut TreeStore, item_id: &str, name: &str) -> LocalItemId {
    write_listing_of_changes(store, &[listed_file(item_id, "R", name, (3, &format!("h-{item_id}")), &format!("e-{item_id}"))]);
    show(store, item_id)
}

fn show_directory(store: &mut TreeStore, item_id: &str, parent_id: &str, name: &str) -> LocalItemId {
    write_listing_of_changes(store, &[listed_directory(item_id, parent_id, name, &format!("e-{item_id}"))]);
    show(store, item_id)
}

fn show(store: &mut TreeStore, item_id: &str) -> LocalItemId {
    let id = bound(store, item_id).local_item_id;
    let planned_from = versions(store, id);
    assert_eq!(store.item_store().commit_show_item(&planned_from, false).unwrap(), StepCommitOutcome::Written);
    id
}

/// Whether the two sides have the item at one place with one content, and agree on both.
fn agrees(item: &Item) -> bool {
    item.local.is_some() && item.local == item.onedrive && item.synced_place == item.local.as_ref().and_then(|local| local.place.clone()) && item.synced_content == item.local.as_ref().and_then(|local| local.content.clone()) && item.unsent_since.is_none()
}

/// Whether the row is a binding and nothing else.
fn is_binding_alone(item: &Item) -> bool {
    item.onedrive_item.is_some() && item.local.is_none() && item.onedrive.is_none() && item.synced_place.is_none() && item.synced_content.is_none()
}

// -- towards OneDrive

#[test]
fn a_directory_made_in_onedrive_has_its_binding_and_the_place_sent_as_the_place_agreed_on() {
    let mut store = store_with_listed_root();
    let directory = make(&mut store, ItemKind::Directory, ROOT, "d");
    commit(&mut store, directory, |items, planned_from| items.commit_make_directory(planned_from, &reference("R"), "d".as_ref(), &confirmed("D", "R", "d", None, "e-D")));
    let made = row(&mut store, directory);
    assert_eq!(made.onedrive_item, Some(reference("D")));
    assert_eq!((made.onedrive_etag.as_deref(), made.confirmed_at_count), (Some("e-D"), 1));
    assert!(agrees(&made), "{made:?}");

    // Sent under a temporary name: that is OneDrive's place and the place agreed on, and the
    // item is moved here still. A name OneDrive stored otherwise than it was sent is
    // OneDrive's change of the place.
    let temporary = make(&mut store, ItemKind::Directory, ROOT, "t");
    commit(&mut store, temporary, |items, planned_from| items.commit_make_directory(planned_from, &reference("R"), ".konedrive-swap-3".as_ref(), &confirmed("T", "R", ".konedrive-swap-3", None, "e-T")));
    let made = row(&mut store, temporary);
    assert_eq!((made.onedrive.unwrap().place, made.synced_place, made.local.unwrap().place), (Some(place(ROOT, ".konedrive-swap-3")), Some(place(ROOT, ".konedrive-swap-3")), Some(place(ROOT, "t"))));
    assert!(made.unsent_since.is_some());
    let stored_otherwise = make(&mut store, ItemKind::Directory, directory, "name ");
    commit(&mut store, stored_otherwise, |items, planned_from| items.commit_make_directory(planned_from, &reference("D"), "name ".as_ref(), &confirmed("N", "D", "name", None, "e-N")));
    let made = row(&mut store, stored_otherwise);
    assert_eq!((made.onedrive.unwrap().place, made.synced_place, made.unsent_since), (Some(place(directory, "name")), Some(place(directory, "name ")), None));
}

#[test]
fn a_new_item_a_listing_brought_before_its_commit_is_one_row_with_it() {
    let mut store = store_with_listed_root();
    let directory = make(&mut store, ItemKind::Directory, ROOT, "d");
    write_listing_of_changes(&mut store, &[listed_directory("D", "R", "d", "e-D")]);
    let listed = bound(&mut store, "D").local_item_id;
    commit(&mut store, directory, |items, planned_from| items.commit_make_directory(planned_from, &reference("R"), "d".as_ref(), &confirmed("D", "R", "d", None, "e-D")));
    assert!(agrees(&row(&mut store, directory)));
    assert_eq!(store.item_store().find_item(listed).unwrap(), None);

    // Shown by the folder as an item of its own meanwhile: the rows changed.
    let second = make(&mut store, ItemKind::Directory, ROOT, "second");
    show_directory(&mut store, "S", "R", "shown");
    let planned_from = versions(&mut store, second);
    assert!(is_left_unwritten(&mut store, |items| items.commit_make_directory(&planned_from, &reference("R"), "second".as_ref(), &confirmed("S", "R", "second", None, "e-S"))));
}

#[test]
fn a_new_file_sent_has_onedrives_content_as_the_content_agreed_on_and_as_its_own_while_that_is_the_one_sent() {
    let mut store = store_with_listed_root();
    let file = make(&mut store, ItemKind::File, ROOT, "a.txt");
    commit(&mut store, file, |items, planned_from| items.commit_send_new_file(planned_from, &reference("R"), "a.txt".as_ref(), &confirmed("A", "R", "a.txt", Some(confirmed_content(5, "h5")), "e-A"), None));
    let sent = row(&mut store, file);
    assert!(agrees(&sent), "{sent:?}");
    assert_eq!((sent.synced_content, sent.content_presence), (Some(confirmed_content(5, "h5")), ContentPresence::OnDisk));

    // Changed and renamed again while it was sent: what OneDrive confirmed is written, and
    // what was done here since is unsent.
    let changed = make(&mut store, ItemKind::File, ROOT, "b.txt");
    let planned_from = versions(&mut store, changed);
    write_here(&mut store, changed, 9);
    move_here(&mut store, changed, ROOT, "c.txt");
    let answer = confirmed("B", "R", "b.txt", Some(confirmed_content(5, "h5")), "e-B");
    assert_eq!(store.item_store().commit_send_new_file(&planned_from, &reference("R"), "b.txt".as_ref(), &answer, None).unwrap(), StepCommitOutcome::Written);
    let sent = row(&mut store, changed);
    assert_eq!(sent.onedrive, Some(ItemVersion { place: Some(place(ROOT, "b.txt")), content: Some(confirmed_content(5, "h5")) }));
    assert_eq!((sent.synced_place, sent.synced_content), (Some(place(ROOT, "b.txt")), Some(confirmed_content(5, "h5"))));
    assert_eq!(sent.local, Some(ItemVersion { place: Some(place(ROOT, "c.txt")), content: Some(ContentVersion::ChangedHere { size: 9, mtime: 300, mtime_nsec: 9, change_count: 2 }) }));
    assert!(sent.unsent_since.is_some());

    // Removed while it was sent: its row is gone, and what OneDrive made is the step's to remove.
    let removed = make(&mut store, ItemKind::File, ROOT, "d.txt");
    let planned_from = versions(&mut store, removed);
    store.item_store().apply(&LocalVersionChange::Removed { item: removed }).unwrap();
    assert!(is_left_unwritten(&mut store, |items| items.commit_send_new_file(&planned_from, &reference("R"), "d.txt".as_ref(), &confirmed("X", "R", "d.txt", Some(confirmed_content(5, "h5")), "e-X"), None)));
}

#[test]
fn content_sent_is_onedrives_content_and_the_content_agreed_on() {
    let mut store = store_with_listed_root();
    let file = show_file(&mut store, "A", "a.txt");
    write_here(&mut store, file, 9);
    commit(&mut store, file, |items, planned_from| items.commit_send_content(planned_from, &confirmed("A", "R", "a.txt", Some(confirmed_content(9, "h9")), "e-A-2"), None));
    let sent = row(&mut store, file);
    assert!(agrees(&sent), "{sent:?}");
    assert_eq!((sent.synced_content, sent.onedrive_etag.as_deref(), sent.confirmed_at_count), (Some(confirmed_content(9, "h9")), Some("e-A-2"), 1));

    // Changed again while it was sent: the local content stays, and is unsent.
    write_here(&mut store, file, 11);
    let planned_from = versions(&mut store, file);
    write_here(&mut store, file, 12);
    assert_eq!(store.item_store().commit_send_content(&planned_from, &confirmed("A", "R", "a.txt", Some(confirmed_content(11, "h11")), "e-A-3"), None).unwrap(), StepCommitOutcome::Written);
    let sent = row(&mut store, file);
    assert_eq!(sent.synced_content, Some(confirmed_content(11, "h11")));
    assert_eq!(sent.local.unwrap().content.unwrap().size(), 12);
    assert!(sent.unsent_since.is_some());
}

#[test]
fn a_move_sent_is_onedrives_place_and_the_place_agreed_on() {
    let mut store = store_with_listed_root();
    let directory = show_directory(&mut store, "D", "R", "d");
    let file = show_file(&mut store, "A", "a.txt");
    move_here(&mut store, file, directory, "b.txt");
    commit(&mut store, file, |items, planned_from| items.commit_send_move(planned_from, &reference("D"), "b.txt".as_ref(), &confirmed("A", "D", "b.txt", Some(listed_content(3, "h-A")), "e-A-2")));
    let moved = row(&mut store, file);
    assert!(agrees(&moved), "{moved:?}");
    assert_eq!((moved.synced_place, moved.onedrive_etag.as_deref(), moved.confirmed_at_count), (Some(place(directory, "b.txt")), Some("e-A-2"), 1));

    // The directory it was sent into has no row any more: the rows changed.
    move_here(&mut store, file, ROOT, "c.txt");
    let planned_from = versions(&mut store, file);
    assert!(is_left_unwritten(&mut store, |items| items.commit_send_move(&planned_from, &reference("unknown"), "c.txt".as_ref(), &confirmed("A", "unknown", "c.txt", Some(listed_content(3, "h-A")), "e-A-3"))));
}

#[test]
fn the_removal_of_a_file_sent_leaves_its_row_with_its_binding_alone_stamped() {
    let mut store = store_with_listed_root();
    let file = show_file(&mut store, "A", "a.txt");
    store.item_store().apply(&LocalVersionChange::Removed { item: file }).unwrap();
    commit(&mut store, file, |items, planned_from| items.commit_send_file_removal(planned_from, 0));
    let removed = row(&mut store, file);
    assert!(is_binding_alone(&removed), "{removed:?}");
    assert_eq!((removed.confirmed_at_count, removed.unsent_since), (1, None));
}

#[test]
fn the_removal_of_a_directory_sent_takes_all_that_was_removed_with_it() {
    let mut store = store_with_listed_root();
    let directory = show_directory(&mut store, "D", "R", "d");
    let inner = show_directory(&mut store, "E", "D", "e");
    write_listing_of_changes(&mut store, &[listed_file("A", "E", "a.txt", (3, "h-A"), "e-A")]);
    let file = show(&mut store, "A");
    for item in [file, inner, directory] {
        store.item_store().apply(&LocalVersionChange::Removed { item }).unwrap();
    }
    let below = [versions(&mut store, inner), versions(&mut store, file)];

    // One of those removed with it changed since: nothing is written.
    let outdated = [below[0].clone(), ItemVersions { onedrive_etag: Some("e-outdated".to_owned()), ..below[1].clone() }];
    let planned_from = versions(&mut store, directory);
    assert!(is_left_unwritten(&mut store, |items| items.commit_send_directory_removal(&planned_from, &outdated, 0)));

    commit(&mut store, directory, |items, planned_from| items.commit_send_directory_removal(planned_from, &below, 0));
    for removed in [directory, inner, file] {
        let removed = row(&mut store, removed);
        assert!(is_binding_alone(&removed) && removed.confirmed_at_count > 0, "{removed:?}");
    }
}

// -- towards the folder

#[test]
fn an_item_shown_has_onedrives_version_here_not_on_disk_and_agreed_on() {
    let mut store = store_with_listed_root();
    write_listing_of_changes(&mut store, &[listed_file("A", "R", "a.txt", (3, "h-A"), "e-A"), listed_directory("D", "R", "d", "e-D")]);
    let (file, directory) = (bound(&mut store, "A").local_item_id, bound(&mut store, "D").local_item_id);
    commit(&mut store, file, |items, planned_from| items.commit_show_item(planned_from, true));
    commit(&mut store, directory, |items, planned_from| items.commit_show_item(planned_from, false));
    let shown = row(&mut store, file);
    assert!(agrees(&shown), "{shown:?}");
    assert_eq!((shown.content_presence, shown.is_name_ignored, shown.confirmed_at_count), (ContentPresence::NotOnDisk, true, 0));
    assert!(agrees(&row(&mut store, directory)));

    // Its name was taken here meanwhile, or the folder does not show its directory: the
    // rows changed.
    write_listing_of_changes(&mut store, &[listed_file("B", "R", "b.txt", (3, "h-B"), "e-B"), listed_directory("E", "R", "e", "e-E"), listed_file("C", "E", "c.txt", (3, "h-C"), "e-C")]);
    make(&mut store, ItemKind::File, ROOT, "b.txt");
    for item_id in ["B", "C"] {
        let planned_from = ItemVersions::of(&bound(&mut store, item_id));
        assert!(is_left_unwritten(&mut store, |items| items.commit_show_item(&planned_from, false)), "{item_id}");
    }
}

#[test]
fn an_item_moved_here_is_at_onedrives_place_which_is_the_place_agreed_on() {
    let mut store = store_with_listed_root();
    let directory = show_directory(&mut store, "D", "R", "d");
    let file = show_file(&mut store, "A", "a.txt");
    write_listing_of_changes(&mut store, &[listed_file("A", "D", "b.txt", (3, "h-A"), "e-A-2")]);
    commit_taking_record(&mut store, file, |items, planned_from, record| items.commit_move_item(planned_from, false, record));
    let moved = row(&mut store, file);
    assert!(agrees(&moved), "{moved:?}");
    assert_eq!(moved.synced_place, Some(place(directory, "b.txt")));

    // A directory OneDrive moved into what the folder shows in it: the rows refuse it.
    let inner = show_directory(&mut store, "E", "D", "e");
    write_listing_of_changes(&mut store, &[listed_directory("D", "E", "d", "e-D-2")]);
    let _ = inner;
    let planned_from = versions(&mut store, directory);
    assert!(is_left_unwritten(&mut store, |items| items.commit_move_item(&planned_from, false, None)));
}

#[test]
fn a_file_that_takes_onedrives_content_shows_it_not_on_disk() {
    let mut store = store_with_listed_root();
    let file = show_file(&mut store, "A", "a.txt");
    store.item_store().set_content_presence(file, ContentPresence::OnDisk).unwrap();
    write_listing_of_changes(&mut store, &[listed_file("A", "R", "a.txt", (8, "h-new"), "e-A-2")]);
    commit_taking_record(&mut store, file, |items, planned_from, record| items.commit_take_onedrive_content(planned_from, record));
    let taken = row(&mut store, file);
    assert!(agrees(&taken), "{taken:?}");
    assert_eq!((taken.synced_content, taken.content_presence), (Some(listed_content(8, "h-new")), ContentPresence::NotOnDisk));
}

#[test]
fn an_item_that_leaves_the_folder_has_nothing_agreed_on_any_more() {
    // Removed in OneDrive: the row is a binding alone.
    let mut store = store_with_listed_root();
    let file = show_file(&mut store, "A", "a.txt");
    write_listing_of_changes(&mut store, &[listed_gone("A")]);
    commit_taking_record(&mut store, file, |items, planned_from, record| items.commit_remove_item(planned_from, record));
    assert!(is_binding_alone(&row(&mut store, file)));

    // OneDrive has it where the folder cannot show it: the row is that of an item of
    // OneDrive the folder never showed, and nothing reads as removed here.
    let unplaced = show_file(&mut store, "B", "b.txt");
    write_listing_of_changes(&mut store, &[not_placed(listed_file("B", "R", "b.txt", (3, "h-B"), "e-B"))]);
    commit(&mut store, unplaced, |items, planned_from| items.commit_remove_item(planned_from, None));
    let left = row(&mut store, unplaced);
    assert_eq!((left.local, left.synced_place, left.synced_content, left.unsent_since), (None, None, None, None));
    assert!(left.onedrive.is_some() && left.placement != Placement::Placed);

    // A directory the folder shows something in: the rows changed.
    let directory = show_directory(&mut store, "D", "R", "d");
    make(&mut store, ItemKind::File, directory, "mine.txt");
    write_listing_of_changes(&mut store, &[listed_gone("D")]);
    let planned_from = versions(&mut store, directory);
    assert!(is_left_unwritten(&mut store, |items| items.commit_remove_item(&planned_from, None)));
}

#[test]
fn a_downloaded_content_is_on_disk() {
    let mut store = store_with_listed_root();
    let file = show_file(&mut store, "A", "a.txt");
    let before = row(&mut store, file);
    commit_taking_record(&mut store, file, |items, planned_from, record| items.commit_download_content(planned_from, record));
    assert_eq!(row(&mut store, file), Item { content_presence: ContentPresence::OnDisk, ..before });
}

#[test]
fn an_item_renamed_to_a_conflict_copys_name_has_that_name_here_and_nothing_else_changes() {
    let mut store = store_with_listed_root();
    let file = make(&mut store, ItemKind::File, ROOT, "a.txt");
    let before = row(&mut store, file);
    commit(&mut store, file, |items, planned_from| items.commit_rename_to_conflict_copy(planned_from, "a-pc.txt".as_ref(), true));
    let mut renamed = before.clone();
    renamed.local.as_mut().unwrap().place = Some(place(ROOT, "a-pc.txt"));
    renamed.is_name_ignored = true;
    assert_eq!(row(&mut store, file), renamed);
    assert!(store.item_store().is_ignored(file).unwrap());

    // An item OneDrive has keeps what was agreed on: its move is sent.
    let bound_file = show_file(&mut store, "B", "b.txt");
    commit(&mut store, bound_file, |items, planned_from| items.commit_rename_to_conflict_copy(planned_from, "b-pc.txt".as_ref(), false));
    let renamed = row(&mut store, bound_file);
    assert_eq!((renamed.local.unwrap().place, renamed.synced_place), (Some(place(ROOT, "b-pc.txt")), Some(place(ROOT, "b.txt"))));

    // The name was taken here meanwhile: the rows changed.
    make(&mut store, ItemKind::File, ROOT, "taken.txt");
    let planned_from = versions(&mut store, file);
    assert!(is_left_unwritten(&mut store, |items| items.commit_rename_to_conflict_copy(&planned_from, "taken.txt".as_ref(), false)));
}

#[test]
fn a_move_taken_back_puts_the_item_at_onedrives_place_again() {
    let mut store = store_with_listed_root();
    let directory = show_directory(&mut store, "D", "R", "d");
    let other = show_directory(&mut store, "E", "R", "e");
    move_here(&mut store, directory, other, "d");
    commit_taking_record(&mut store, directory, |items, planned_from, record| items.commit_take_move_back(planned_from, false, record));
    assert!(agrees(&row(&mut store, directory)));
}

#[test]
fn an_item_moved_to_a_temporary_name_has_that_place_here_and_as_the_place_agreed_on() {
    let mut store = store_with_listed_root();
    let file = show_file(&mut store, "A", "a.txt");
    write_listing_of_changes(&mut store, &[listed_file("A", "R", "b.txt", (3, "h-A"), "e-A-2")]);
    let temporary = place(ROOT, ".konedrive-swap-2");
    commit_taking_record(&mut store, file, |items, planned_from, record| items.commit_move_to_temporary_name(planned_from, &temporary, false, record));
    let moved = row(&mut store, file);
    assert_eq!((moved.local.unwrap().place, moved.synced_place, moved.onedrive.unwrap().place, moved.unsent_since), (Some(temporary.clone()), Some(temporary), Some(place(ROOT, "b.txt")), None));
}

// -- of the rows alone

#[test]
fn an_item_that_gives_up_its_binding_is_the_users_own_and_another_row_has_what_is_onedrives() {
    let mut store = store_with_listed_root();
    let file = show_file(&mut store, "A", "a.txt");
    write_here(&mut store, file, 9);
    write_listing_of_changes(&mut store, &[listed_file("A", "R", "a.txt", (8, "h-new"), "e-A-2")]);
    commit(&mut store, file, |items, planned_from| items.commit_give_up_binding(planned_from, true));
    let own = row(&mut store, file);
    assert_eq!((own.onedrive_item, own.onedrive, own.synced_place, own.synced_content, own.is_name_ignored), (None, None, None, None, true));
    let of_onedrive = bound(&mut store, "A");
    assert_eq!((of_onedrive.local, of_onedrive.onedrive.unwrap().content, of_onedrive.onedrive_etag.as_deref()), (None, Some(listed_content(8, "h-new")), Some("e-A-2")));
}

/// The third review's S4: a merge is something the two sides agree on. The two items were
/// in one directory under one name as OneDrive compares names, and two files had one
/// content: the commit writes that as agreed on, the place and the content as the folder
/// has them. The binding of the row is then no guess to any later plan, and what either
/// side does next reads as that side's change.
#[test]
fn two_items_merged_are_one_row_with_both_sides_that_agree_on_the_place_and_the_content_they_were_merged_for() {
    let mut store = store_with_listed_root();
    let directory = make(&mut store, ItemKind::Directory, ROOT, "d");
    write_listing_of_changes(&mut store, &[listed_directory("D", "R", "d", "e-D")]);
    let listed = bound(&mut store, "D").local_item_id;
    let of_onedrive = versions(&mut store, listed);

    // The row of OneDrive's item changed since: nothing is written.
    let planned_from = versions(&mut store, directory);
    let outdated = ItemVersions { onedrive_etag: Some("e-outdated".to_owned()), ..of_onedrive.clone() };
    assert!(is_left_unwritten(&mut store, |items| items.commit_merge_items(&planned_from, &outdated)));

    commit(&mut store, directory, |items, planned_from| items.commit_merge_items(planned_from, &of_onedrive));
    let merged = row(&mut store, directory);
    assert_eq!((merged.onedrive_item.clone(), merged.local.clone().unwrap().place, merged.onedrive.clone().unwrap().place, merged.synced_place.clone()), (Some(reference("D")), Some(place(ROOT, "d")), Some(place(ROOT, "d")), Some(place(ROOT, "d"))));
    assert!(agrees(&merged), "{merged:?}");
    assert_eq!(store.item_store().find_item(listed).unwrap(), None);

    // Two files of one hash, under names that differ by the case of a letter: the place
    // agreed on is the folder's, so that OneDrive's spelling reads as OneDrive's change,
    // which the folder follows; the content agreed on is the local one, the same bytes.
    let file = make(&mut store, ItemKind::File, ROOT, "Readme");
    let made = versions(&mut store, file);
    store.item_store().commit_hash_local_content(&made, "h-F").unwrap();
    write_listing_of_changes(&mut store, &[listed_file("F", "R", "README", (5, "h-F"), "e-F")]);
    let listed = bound(&mut store, "F").local_item_id;
    let of_onedrive = versions(&mut store, listed);
    commit(&mut store, file, |items, planned_from| items.commit_merge_items(planned_from, &of_onedrive));
    let merged = row(&mut store, file);
    let local = merged.local.clone().unwrap();
    assert_eq!((local.place.clone(), merged.onedrive.clone().unwrap().place, merged.synced_place.clone()), (Some(place(ROOT, "Readme")), Some(place(ROOT, "README")), Some(place(ROOT, "Readme"))));
    assert_eq!((merged.synced_content.clone(), merged.unsent_since), (local.content, None), "the same bytes: nothing is unsent");
    assert_eq!(merged.onedrive.unwrap().content, Some(listed_content(5, "h-F")));
}

#[test]
fn a_row_both_sides_removed_the_item_of_is_left_with_its_binding_alone() {
    let mut store = store_with_listed_root();
    let file = show_file(&mut store, "A", "a.txt");
    store.item_store().apply(&LocalVersionChange::Removed { item: file }).unwrap();
    write_listing_of_changes(&mut store, &[listed_gone("A")]);
    commit(&mut store, file, |items, planned_from| items.commit_agree_on_removal(planned_from));
    let dropped = row(&mut store, file);
    assert!(is_binding_alone(&dropped) && dropped.confirmed_at_count == 0 && dropped.unsent_since.is_none(), "{dropped:?}");
}

#[test]
fn what_was_agreed_on_of_an_item_removed_here_is_forgotten_and_onedrives_version_stays() {
    let mut store = store_with_listed_root();
    let file = show_file(&mut store, "A", "a.txt");
    store.item_store().apply(&LocalVersionChange::Removed { item: file }).unwrap();
    write_listing_of_changes(&mut store, &[not_placed(listed_file("A", "R", "a.txt", (8, "h-new"), "e-A-2"))]);
    commit(&mut store, file, |items, planned_from| items.commit_forget_synced_version(planned_from));
    let forgotten = row(&mut store, file);
    assert_eq!((forgotten.local, forgotten.synced_place, forgotten.synced_content, forgotten.unsent_since), (None, None, None, None));
    assert_eq!(forgotten.onedrive.unwrap().content, Some(listed_content(8, "h-new")));
}

#[test]
fn a_local_content_hashed_is_told_by_its_hash_with_the_size_and_the_time_it_had() {
    let mut store = store_with_listed_root();
    let file = make(&mut store, ItemKind::File, ROOT, "a.txt");
    commit(&mut store, file, |items, planned_from| items.commit_hash_local_content(planned_from, "h5"));
    assert_eq!(row(&mut store, file).local.unwrap().content, Some(ContentVersion::Hashed { size: 5, mtime: 200, mtime_nsec: 7, hash: Some("h5".to_owned()), ctag: None }));
}

#[test]
fn a_place_both_sides_moved_the_item_to_is_the_place_agreed_on() {
    let mut store = store_with_listed_root();
    let file = show_file(&mut store, "A", "a.txt");
    move_here(&mut store, file, ROOT, "b.txt");
    write_listing_of_changes(&mut store, &[listed_file("A", "R", "b.txt", (3, "h-A"), "e-A-2")]);
    commit(&mut store, file, |items, planned_from| items.commit_agree_on_place(planned_from));
    assert!(agrees(&row(&mut store, file)));
}

#[test]
fn a_content_both_sides_have_is_onedrives_content_version_here_and_agreed_on() {
    let mut store = store_with_listed_root();
    let file = show_file(&mut store, "A", "a.txt");
    write_here(&mut store, file, 8);
    write_listing_of_changes(&mut store, &[listed_file("A", "R", "a.txt", (8, "h-new"), "e-A-2")]);
    commit(&mut store, file, |items, planned_from| items.commit_agree_on_content(planned_from));
    let agreed = row(&mut store, file);
    // OneDrive's content version, with the time the file has here: that of the bytes on
    // disk, which the check at start compares the row with.
    let with_local_time = ContentVersion::Hashed { size: 8, mtime: 300, mtime_nsec: 9, hash: Some("h-new".to_owned()), ctag: Some("c-h-new".to_owned()) };
    assert_eq!((agreed.local.as_ref().and_then(|local| local.content.clone()), agreed.synced_content.clone(), agreed.content_presence), (Some(with_local_time.clone()), Some(with_local_time), ContentPresence::OnDisk));
    assert_eq!(agreed.onedrive.as_ref().and_then(|onedrive| onedrive.content.clone()), Some(listed_content(8, "h-new")), "OneDrive's time is OneDrive's");
    assert_eq!((agreed.unsent_since, store.item_store().list_changed_in_onedrive(None, 10).unwrap()), (None, vec![]), "nothing is unsent, and nothing reads as changed in OneDrive");
}

/// What no plan asks for is an error, not "the rows changed": a step of a kind of item it is
/// not for, and an answer of OneDrive that is of another item.
#[test]
fn a_step_no_plan_asks_for_is_refused() {
    let mut store = store_with_listed_root();
    let file = show_file(&mut store, "A", "a.txt");
    let planned_from = versions(&mut store, file);
    let is_refused = |result: Result<StepCommitOutcome, TreeError>| matches!(result, Err(TreeError::Refused(_))) || panic!("not refused: {result:?}");
    let mut items = store.item_store();
    assert!(is_refused(items.commit_send_content(&planned_from, &confirmed("other", "R", "a.txt", Some(confirmed_content(3, "h")), "e"), None)));
    assert!(is_refused(items.commit_send_file_removal(&planned_from, 0)), "the folder shows it");
    assert!(is_refused(items.commit_show_item(&planned_from, false)), "the folder shows it");
    assert!(is_refused(items.commit_agree_on_removal(&planned_from)));
    assert!(is_refused(items.commit_make_directory(&planned_from, &reference("R"), "a.txt".as_ref(), &confirmed("N", "R", "a.txt", None, "e"))));
    assert!(is_refused(items.commit_agree_on_place(&ItemVersions::of(&items.find_item(ROOT).unwrap().unwrap()))));
}

// -- the findings of the review of part 3

/// The review's S1, new: a file saved by a rename over the file whose content is being
/// sent takes that file's binding, and what OneDrive confirmed of the upload is written to
/// the row that has the binding now. The client's own upload is then not "OneDrive changed
/// the content": the content agreed on is the one that was sent, and the content saved
/// since is unsent.
#[test]
fn content_sent_is_confirmed_of_the_row_that_has_the_binding_now() {
    let mut store = store_with_listed_root();
    let file = show_file(&mut store, "G", "g");
    write_here(&mut store, file, 7);
    let planned_from = versions(&mut store, file);
    // An editor saves again while the upload runs: a temporary file, renamed over the file.
    let saved = make(&mut store, ItemKind::File, ROOT, "g.tmp");
    store.item_store().apply(&LocalVersionChange::SavedOver { replacing: saved, replaced: file }).unwrap();
    assert_eq!((store.item_store().find_item(file).unwrap(), bound(&mut store, "G").local_item_id), (None, saved), "the replacing item has the binding");
    let saved_content = row(&mut store, saved).local.unwrap().content;
    // A try of the running upload failed and is remembered of the row that has the binding.
    let tried = row(&mut store, saved).unsent_since.map(|unsent_since| crate::TriedChange::Unsent { unsent_since });
    assert_eq!(store.step_memory().remember_failed_try(saved, tried.as_ref(), &konedrive_reason::Reason::Network, Some(900)).unwrap(), Some(1));

    let sent = confirmed("G", "R", "g", Some(confirmed_content(7, "h-sent")), "e-sent");
    assert_eq!(store.item_store().commit_send_content(&planned_from, &sent, None).unwrap(), StepCommitOutcome::Written);
    // What the confirmed request ends is forgotten of the row it was confirmed of.
    assert_eq!(store.step_memory().find_item_memory(saved).unwrap().failed_tries, None);
    let now = row(&mut store, saved);
    assert_eq!(now.onedrive.as_ref().and_then(|onedrive| onedrive.content.clone()), Some(confirmed_content(7, "h-sent")));
    assert_eq!((now.synced_content.clone(), now.onedrive_etag.as_deref()), (Some(confirmed_content(7, "h-sent")), Some("e-sent")));
    assert_eq!(now.local.as_ref().unwrap().content, saved_content, "what was saved since is the local content still, and unsent");
    assert!(now.unsent_since.is_some() && now.confirmed_at_count > 0);
    assert_eq!(find_broken_rule(&store.item_store().list_items().unwrap(), false), None);

    // A commit planned from other versions of OneDrive's side than the row has now writes
    // nothing, whichever row has the binding.
    assert!(is_left_unwritten(&mut store, |items| items.commit_send_content(&planned_from, &sent, None)));
}

/// S1, for a move: the place OneDrive confirmed is written to the row that has the binding
/// now.
#[test]
fn a_move_sent_is_confirmed_of_the_row_that_has_the_binding_now() {
    let mut store = store_with_listed_root();
    let file = show_file(&mut store, "G", "g");
    move_here(&mut store, file, ROOT, "g2");
    let planned_from = versions(&mut store, file);
    let saved = make(&mut store, ItemKind::File, ROOT, "g2.tmp");
    store.item_store().apply(&LocalVersionChange::SavedOver { replacing: saved, replaced: file }).unwrap();
    let moved = confirmed("G", "R", "g2", Some(listed_content(3, "h-G")), "e-moved");
    assert_eq!(store.item_store().commit_send_move(&planned_from, &reference("R"), "g2".as_ref(), &moved).unwrap(), StepCommitOutcome::Written);
    let now = row(&mut store, saved);
    assert_eq!((now.synced_place.clone(), now.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone())), (Some(place(ROOT, "g2")), Some(place(ROOT, "g2"))));
    assert_eq!(now.local.as_ref().and_then(|local| local.place.clone()), Some(place(ROOT, "g2")));
}

/// The review's S4, new: a step is conditional on whether the content is on disk too. A
/// file moved here whose content is on disk gives up its binding when OneDrive removed it,
/// and is sent again; freed meanwhile, its bytes are nowhere, and the step's commit answers
/// that the rows changed.
#[test]
fn a_step_is_not_written_when_the_content_is_not_on_disk_as_it_was_planned_from() {
    let mut store = store_with_listed_root();
    let file = show_file(&mut store, "G", "g");
    store.item_store().set_content_presence(file, ContentPresence::OnDisk).unwrap();
    move_here(&mut store, file, ROOT, "g2");
    write_listing_of_changes(&mut store, &[listed_gone("G")]);
    let planned_from = versions(&mut store, file);
    store.item_store().set_content_presence(file, ContentPresence::NotOnDisk).unwrap();
    assert!(is_left_unwritten(&mut store, |items| items.commit_give_up_binding(&planned_from, false)));
    assert!(row(&mut store, file).onedrive_item.is_some());
}

/// The review's S5, new: a file opened for writing while its content is sent, and closed
/// unchanged, agrees with what was confirmed: the content kept from before the first writer
/// is the one that was sent, and the confirmation is written there too.
#[test]
fn a_file_opened_during_its_upload_and_closed_unchanged_agrees_with_what_was_confirmed() {
    let mut store = store_with_listed_root();
    let file = show_file(&mut store, "G", "g");
    write_here(&mut store, file, 7);
    let planned_from = versions(&mut store, file);
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: file }).unwrap();
    let sent = confirmed("G", "R", "g", Some(confirmed_content(7, "h-sent")), "e-sent");
    assert_eq!(store.item_store().commit_send_content(&planned_from, &sent, None).unwrap(), StepCommitOutcome::Written);
    store.item_store().apply(&LocalVersionChange::ClosedUnchanged { item: file }).unwrap();
    let closed = row(&mut store, file);
    assert!(agrees(&closed), "{closed:?}");
}

/// Found by the larger convergence run: a move whose commit arrives after the directory it
/// was sent into was removed from OneDrive writes nothing: the rows would say OneDrive has
/// the item in a directory it does not have. The next listing says what became of it.
#[test]
fn a_move_into_a_directory_onedrive_does_not_have_any_more_is_not_written() {
    let mut store = store_with_listed_root();
    let directory = show_directory(&mut store, "D", "R", "d");
    let file = show_file(&mut store, "G", "g");
    move_here(&mut store, file, directory, "g");
    let planned_from = versions(&mut store, file);
    move_here(&mut store, file, ROOT, "g");
    store.item_store().apply(&LocalVersionChange::Removed { item: directory }).unwrap();
    let removal = versions(&mut store, directory);
    assert_eq!(store.item_store().commit_send_directory_removal(&removal, &[], 0).unwrap(), StepCommitOutcome::Written);
    let moved = confirmed("G", "D", "g", Some(listed_content(3, "h-G")), "e-moved");
    assert!(is_left_unwritten(&mut store, |items| items.commit_send_move(&planned_from, &reference("D"), "g".as_ref(), &moved)));
}

/// The removals sent a short time ago, counted since their window began.
mod sent_removal_window;
/// The commit of a removal that was sent.
mod removal;
/// A listing that told of an item before the answer to the client's own request came.
mod listed_before_answer;
