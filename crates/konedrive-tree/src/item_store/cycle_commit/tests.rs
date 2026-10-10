use std::collections::BTreeMap;

use crate::item_store::tests::{item, make_directory, make_file, place};
use crate::model::{Change, ContentPresence, ContentVersion, FileSyncState, Item, ItemKind, Kind, LocalItemId, LocalVersionChange, NewTree, OneDriveItemReference, Placement, Row, SkipReason};
use crate::reconcile::Deferrals;
use crate::{DriveId, OneDriveItemId, TreeStore};

const ROOT: LocalItemId = LocalItemId::ROOT;

fn onedrive_row(id: &str, parent: Option<&str>, name: &str, kind: Kind, ctag: &str) -> Row {
    let size = if kind == Kind::File { 3 } else { 0 };
    Row { id: id.into(), parent_id: parent.map(str::to_owned), name: name.into(), kind, size, mtime: 100, etag: Some(format!("e-{ctag}")), ctag: Some(ctag.into()), quickxor: Some(format!("h-{ctag}")), mime: None, placement: Placement::Placed }
}

fn root() -> Change {
    Change::Root(onedrive_row("R", None, "root", Kind::Folder, "r"))
}

fn directory(id: &str, parent: &str, name: &str) -> Change {
    Change::Upsert(onedrive_row(id, Some(parent), name, Kind::Folder, "d"))
}

/// The file `id` with the content told by `ctag`.
fn file(id: &str, parent: &str, name: &str, ctag: &str) -> Change {
    Change::Upsert(onedrive_row(id, Some(parent), name, Kind::File, ctag))
}

/// The content the rows have for a file of OneDrive whose content is told by `ctag`.
fn content(ctag: &str) -> ContentVersion {
    ContentVersion::Hashed { size: 3, mtime: 100, mtime_nsec: 0, hash: Some(format!("h-{ctag}")), ctag: Some(ctag.into()) }
}

fn reference(id: &str) -> OneDriveItemReference {
    OneDriveItemReference { drive_id: DriveId::new("drive").unwrap(), item_id: OneDriveItemId::new(id) }
}

/// A store that knows its drive, with `changes` committed as a page of a first listing.
fn listed(changes: &[Change]) -> TreeStore {
    let mut store = TreeStore::in_memory().unwrap();
    store.set_drive_id("Drive").unwrap();
    store.commit_page(changes, "next", &[]).unwrap();
    store
}

/// `changes` as a delta of a read-only folder, staged and committed.
fn commit_delta(store: &mut TreeStore, changes: &[Change], copies_on_disk: &[&str]) {
    store.begin_staging(NewTree::Delta).unwrap();
    store.stage(changes).unwrap();
    let copies: Vec<String> = copies_on_disk.iter().map(|id| (*id).to_owned()).collect();
    store.commit_staging("link", &copies).unwrap();
}

/// The row bound to the item `id` of OneDrive.
fn bound(store: &mut TreeStore, id: &str) -> Option<Item> {
    store.item_store().find_bound_item(&reference(id)).unwrap()
}

fn bound_id(store: &mut TreeStore, id: &str) -> LocalItemId {
    bound(store, id).unwrap_or_else(|| panic!("no row is bound to {id}")).local_item_id
}

/// What the folder shows by the rows: each path, with `directory` or the file's sync state.
fn folder(store: &mut TreeStore) -> BTreeMap<String, String> {
    let mut shown = BTreeMap::new();
    let mut directories = vec![ROOT];
    while let Some(at) = directories.pop() {
        for child in store.item_store().list_children(at).unwrap() {
            let path = store.item_store().find_path(child.local_item_id).unwrap().unwrap().to_string_lossy().into_owned();
            let state = match child.file_sync_state() {
                Some(state) => format!("{state:?}"),
                None => {
                    directories.push(child.local_item_id);
                    "directory".to_owned()
                }
            };
            shown.insert(path, state);
        }
    }
    shown
}

fn shows(store: &mut TreeStore) -> Vec<String> {
    folder(store).into_iter().map(|(path, state)| format!("{path}: {state}")).collect()
}

/// Every row of `items`, as it is: what a commit that changes nothing leaves the same.
fn all_rows(store: &TreeStore) -> Vec<String> {
    let mut statement = store.conn.prepare("SELECT quote(local_item_id) || quote(onedrive_item_id) || quote(local_parent_id) || quote(local_name) || quote(local_ctag) || quote(synced_parent_id) || quote(synced_name) || quote(synced_ctag) || content_presence FROM items ORDER BY local_item_id").unwrap();
    let rows = statement.query_map([], |row| row.get(0)).unwrap().collect::<Result<Vec<String>, _>>().unwrap();
    rows
}

/// A file of the user's own at `name` in the root, as a write operation makes its row.
fn make_own_file(store: &mut TreeStore, parent: LocalItemId, name: &str) -> LocalItemId {
    make_file(store, parent, name)
}

/// A page puts a row for every item it places, bound, with the local and the synced version
/// its `onedrive_items` row says, from one value, and not on disk.
#[test]
fn a_listed_item_has_a_bound_row_that_says_what_onedrive_has() {
    let mut store = listed(&[root(), directory("D", "R", "docs"), file("A", "R", "a.txt", "a1"), file("C", "D", "c.txt", "c1")]);
    assert_eq!(shows(&mut store), ["a.txt: RemoteOnly", "docs: directory", "docs/c.txt: RemoteOnly"]);
    let (docs, c) = (bound(&mut store, "D").unwrap(), bound(&mut store, "C").unwrap());
    assert_eq!((docs.kind, docs.synced_place.clone(), docs.synced_content.clone()), (ItemKind::Directory, Some(place(ROOT, "docs")), None));
    assert_eq!(c.synced_place, Some(place(docs.local_item_id, "c.txt")));
    assert_eq!((c.local.clone().unwrap().content, c.synced_content.clone()), (Some(content("c1")), Some(content("c1"))));
    assert_eq!(item(&mut store, ROOT).onedrive_item, Some(reference("R")));
}

/// The map's fact 5: a directory that becomes placed when its children are in
/// `onedrive_items` already. They are in no list of changed ids, and are shown all the same:
/// a parent that comes on a later page than its child, and a directory the folder could not
/// hold that it can hold now.
#[test]
fn a_directory_that_becomes_placed_shows_what_was_listed_in_it_before() {
    let mut store = listed(&[root(), file("C", "D", "c.txt", "c1"), directory("E", "D", "deep"), file("F", "E", "f.txt", "f1")]);
    assert!(shows(&mut store).is_empty() && bound(&mut store, "C").is_none(), "nothing is placed while its directory has not come");
    store.commit_page(&[directory("D", "R", "docs")], "next", &["F".to_owned()]).unwrap();
    assert_eq!(shows(&mut store), ["docs: directory", "docs/c.txt: RemoteOnly", "docs/deep: directory", "docs/deep/f.txt: Synced"]);

    let skipped = Row { placement: Placement::Skipped(SkipReason::NameTooLong), ..onedrive_row("S", Some("R"), "long", Kind::Folder, "d") };
    let mut store = listed(&[root(), Change::Upsert(skipped), file("G", "S", "g.txt", "g1")]);
    assert!(shows(&mut store).is_empty());
    commit_delta(&mut store, &[directory("S", "R", "short")], &[]);
    assert_eq!(shows(&mut store), ["short: directory", "short/g.txt: RemoteOnly"]);
}

/// OneDrive exchanged two names and rotated three: every row has its new name, and none
/// went through a state the index of one name per directory refuses.
#[test]
fn names_onedrive_exchanged_or_rotated_are_exchanged_and_rotated_in_the_rows() {
    let mut store = listed(&[root(), file("A", "R", "a", "a1"), file("B", "R", "b", "b1"), file("X", "R", "x", "x1"), file("Y", "R", "y", "y1"), file("Z", "R", "z", "z1")]);
    let ids: Vec<LocalItemId> = ["A", "B", "X", "Y", "Z"].iter().map(|id| bound_id(&mut store, id)).collect();
    commit_delta(&mut store, &[file("A", "R", "b", "a1"), file("B", "R", "a", "b1"), file("X", "R", "y", "x1"), file("Y", "R", "z", "y1"), file("Z", "R", "x", "z1")], &[]);
    let names: Vec<String> = ids.iter().map(|id| item(&mut store, *id).local.unwrap().place.unwrap().name.to_string_lossy().into_owned()).collect();
    assert_eq!(names, ["b", "a", "y", "z", "x"]);
    for id in ids {
        let row = item(&mut store, id);
        assert_eq!(row.local.unwrap().place, row.synced_place, "the two sides agree on the place of {id}");
    }
}

/// A new item of OneDrive at a name an item of the user's own holds does not take the name
/// and does not fail the commit: its row is bound and not shown, with nothing agreed on,
/// and takes the name at the commit after the user's own was moved aside.
#[test]
fn a_new_onedrive_item_does_not_take_a_name_a_local_only_item_holds() {
    let mut store = listed(&[root()]);
    let own = make_own_file(&mut store, ROOT, "new.txt");
    commit_delta(&mut store, &[file("N", "R", "new.txt", "n1")], &[]);
    assert_eq!(shows(&mut store), ["new.txt: LocalOnly"]);
    let listed = bound(&mut store, "N").unwrap();
    assert_eq!((listed.local.clone(), listed.synced_place.clone(), listed.synced_content.clone()), (None, None, None));

    store.item_store().apply(&LocalVersionChange::Moved { item: own, place: place(ROOT, "new-fedora.txt"), is_name_ignored: false }).unwrap();
    commit_delta(&mut store, &[], &[]);
    assert_eq!(shows(&mut store), ["new-fedora.txt: LocalOnly", "new.txt: RemoteOnly"]);
    assert_eq!(bound_id(&mut store, "N"), listed.local_item_id, "the row made at the first mention is the one shown");
}

/// The item of a create in flight: a listing brings the item of a file made here before the
/// create's commit bound the file. The listed row stays not shown; the create's commit
/// merges it into the file's row, which keeps its local item id; and the next commit finds
/// the file as OneDrive has it.
#[test]
fn the_listed_item_of_a_create_in_flight_is_merged_by_the_creates_commit() {
    let mut store = listed(&[root()]);
    let made = make_own_file(&mut store, ROOT, "new.txt");
    commit_delta(&mut store, &[file("N", "R", "new.txt", "n1")], &["N"]);
    assert_eq!(shows(&mut store), ["new.txt: LocalOnly"]);
    assert_ne!(bound_id(&mut store, "N"), made);

    store.item_store().bind_item(made, &reference("N")).unwrap();
    assert_eq!(bound_id(&mut store, "N"), made);
    commit_delta(&mut store, &[], &[]);
    assert_eq!(shows(&mut store), ["new.txt: Synced"]);
    let row = item(&mut store, made);
    assert_eq!((row.synced_place, row.synced_content), (Some(place(ROOT, "new.txt")), Some(content("n1"))));
}

/// A row that cannot take its new place keeps the one it has, with what was agreed on
/// before, and so does a row whose new place that one keeps: nothing steps aside for a
/// place it does not get.
#[test]
fn a_row_whose_new_place_is_kept_stays_where_it_is_and_keeps_its_place_from_others() {
    let mut store = listed(&[root(), file("A", "R", "a", "a1"), file("C", "R", "c", "c1"), file("E", "R", "e", "e1")]);
    make_own_file(&mut store, ROOT, "b");
    // A is to take the name the user's own holds, C the name A keeps; E moves freely.
    commit_delta(&mut store, &[file("A", "R", "b", "a1"), file("C", "R", "a", "c1"), file("E", "R", "f", "e1")], &[]);
    assert_eq!(shows(&mut store), ["a: RemoteOnly", "b: LocalOnly", "c: RemoteOnly", "f: RemoteOnly"]);
    let (a, c) = (bound(&mut store, "A").unwrap(), bound(&mut store, "C").unwrap());
    assert_eq!((a.synced_place, c.synced_place), (Some(place(ROOT, "a")), Some(place(ROOT, "c"))));
}

/// Two items `onedrive_items` has at one name (one the outbox holds, whose row there is
/// kept, and one OneDrive gave the name to): the row that is there keeps the name. And two
/// new ones: the first row takes it.
#[test]
fn of_two_items_onedrive_items_has_at_one_name_one_is_shown() {
    let mut store = listed(&[root(), file("A", "R", "x", "a1"), file("B", "R", "y", "b1")]);
    store.begin_staging(NewTree::Delta).unwrap();
    store.stage(&[Change::Delete("A".into()), file("B", "R", "x", "b1"), file("M", "R", "twice", "m1"), file("N", "R", "twice", "n1")]).unwrap();
    let held = ["A".to_owned()];
    store.commit_staging_deferring("link", &Deferrals { consumed: &[], whole: &held, content: &[], fetched_at: 0, waits: &[], copies_on_disk: &[] }).unwrap();
    assert_eq!(shows(&mut store), ["twice: RemoteOnly", "x: RemoteOnly", "y: RemoteOnly"]);
    assert_eq!(bound(&mut store, "A").unwrap().local.unwrap().place, Some(place(ROOT, "x")));
    assert!(bound(&mut store, "M").unwrap().local.is_some() && bound(&mut store, "N").unwrap().local.is_none());
}

/// A directory that OneDrive moved into one the user moved below it here is not placed
/// below itself: it stays where it is.
#[test]
fn a_directory_is_not_placed_below_itself() {
    let mut store = listed(&[root(), directory("P", "R", "p"), directory("Q", "R", "q")]);
    let (p, q) = (bound_id(&mut store, "P"), bound_id(&mut store, "Q"));
    // Here: q is moved into p. In OneDrive: p is moved into q.
    store.item_store().apply(&LocalVersionChange::Moved { item: q, place: place(p, "q"), is_name_ignored: false }).unwrap();
    commit_delta(&mut store, &[directory("P", "Q", "p")], &[]);
    assert_eq!(shows(&mut store), ["p: directory", "p/q: directory"]);
}

/// An item removed in OneDrive that the folder shows is never deleted by the commit. The
/// cycle step removed it here first: then its row goes, a directory after what was in it.
#[test]
fn an_item_removed_in_onedrive_goes_once_the_folder_does_not_show_it() {
    let mut store = listed(&[root(), directory("D", "R", "docs"), file("C", "D", "c.txt", "c1")]);
    let (docs, c) = (bound_id(&mut store, "D"), bound_id(&mut store, "C"));
    commit_delta(&mut store, &[Change::Delete("D".into()), Change::Delete("C".into())], &[]);
    assert_eq!(shows(&mut store), ["docs: directory", "docs/c.txt: RemoteOnly"], "the commit deletes no row the folder shows");

    for removed in [c, docs] {
        store.item_store().apply(&LocalVersionChange::Removed { item: removed }).unwrap();
    }
    commit_delta(&mut store, &[], &[]);
    assert!(shows(&mut store).is_empty());
    assert!(store.item_store().find_item(docs).unwrap().is_none() && store.item_store().find_item(c).unwrap().is_none());
}

/// A directory removed in OneDrive that holds something of the user's own gave its binding
/// up: it stays with what it holds, as a directory OneDrive does not have, and the row that
/// took the binding goes.
#[test]
fn a_directory_removed_in_onedrive_that_holds_the_users_own_stays_without_its_binding() {
    let mut store = listed(&[root(), directory("D", "R", "docs"), file("C", "D", "c.txt", "c1")]);
    let (docs, c) = (bound_id(&mut store, "D"), bound_id(&mut store, "C"));
    make_own_file(&mut store, docs, "own.txt");
    // The step: the copy of the file went, the directory stays.
    store.item_store().write_step_changes(&[(docs, false)], &[LocalVersionChange::Removed { item: c }], &[], 0).unwrap();
    commit_delta(&mut store, &[Change::Delete("D".into()), Change::Delete("C".into())], &[]);
    assert_eq!(shows(&mut store), ["docs: directory", "docs/own.txt: LocalOnly"]);
    assert_eq!(item(&mut store, docs).onedrive_item, None);
    assert!(bound(&mut store, "D").is_none() && bound(&mut store, "C").is_none());
}

/// A commit that fails leaves the rows as they were, and the same cycle staged and
/// committed again, twice, leaves them as one commit does.
#[test]
fn a_commit_that_fails_and_is_staged_again_changes_the_rows_once() {
    let mut store = listed(&[root(), file("A", "R", "a", "a1"), file("B", "R", "b", "b1")]);
    let before = all_rows(&store);
    let delta = [file("A", "R", "b", "a2"), file("B", "R", "a", "b1"), file("N", "R", "n", "n1")];
    store.begin_staging(NewTree::Delta).unwrap();
    store.stage(&delta).unwrap();
    // The commit fails at its end, where the link is written.
    store.conn.execute_batch("CREATE TEMP TRIGGER fail BEFORE UPDATE ON meta WHEN NEW.key = 'delta_link' BEGIN SELECT RAISE(ABORT, 'no link'); END; CREATE TEMP TRIGGER fail_new BEFORE INSERT ON meta WHEN NEW.key = 'delta_link' BEGIN SELECT RAISE(ABORT, 'no link'); END;").unwrap();
    assert!(store.commit_staging("link", &[]).is_err());
    assert_eq!(all_rows(&store), before);

    store.conn.execute_batch("DROP TRIGGER fail; DROP TRIGGER fail_new;").unwrap();
    commit_delta(&mut store, &delta, &[]);
    let once = all_rows(&store);
    assert_eq!(shows(&mut store), ["a: RemoteOnly", "b: RemoteOnly", "n: RemoteOnly"]);
    commit_delta(&mut store, &delta, &[]);
    assert_eq!(all_rows(&store), once);
}

/// What the outbox holds: a read-write cycle keeps the item's row in `onedrive_items`, so
/// the commit writes nothing of the item's row, and what the folder shows of it is the
/// user's. Once the folder is read-only and nothing is held, the change that waited is
/// staged by the next cycle and reaches the row: the content is OneDrive's, and the copy
/// the file had is not on disk any more.
#[test]
fn a_change_that_waited_for_a_file_with_a_copy_reaches_its_row_in_a_read_only_cycle() {
    let mut store = listed(&[root()]);
    commit_delta(&mut store, &[file("A", "R", "a.txt", "a1")], &["A"]);
    let a = bound_id(&mut store, "A");
    assert_eq!(shows(&mut store), ["a.txt: Synced"]);
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: a }).unwrap();

    store.begin_staging(NewTree::Delta).unwrap();
    store.stage(&[file("A", "R", "a.txt", "a2")]).unwrap();
    let held = ["A".to_owned()];
    store.commit_staging_deferring("link", &Deferrals { consumed: &[], whole: &held, content: &[], fetched_at: 0, waits: &[], copies_on_disk: &[] }).unwrap();
    assert_eq!(shows(&mut store), ["a.txt: ModifiedHere"]);
    assert_eq!(item(&mut store, a).synced_content, Some(content("a1")));

    // The switch to read-only dropped the outbox's row; the local version stays.
    assert!(store.stage_waiting(&[], false).unwrap(), "what waits is staged");
    assert_eq!(store.changed_ids().unwrap(), ["A"]);
    store.commit_staging("link", &[]).unwrap();
    assert_eq!(shows(&mut store), ["a.txt: RemoteOnly"]);
    let row = item(&mut store, a);
    assert_eq!((row.local.unwrap().content, row.synced_content, row.content_presence), (Some(content("a2")), Some(content("a2")), ContentPresence::NotOnDisk));
    assert!(store.deferred_ids().unwrap().is_empty() && !store.stage_waiting(&[], false).unwrap());
}

/// A first listing over a folder that has copies already: a file whose whole copy the cycle
/// step found is on disk, and synced. Another content from OneDrive leaves it not on disk,
/// and a move without one leaves it as the step found it.
#[test]
fn the_content_presence_is_what_the_cycle_step_found_on_disk() {
    let mut store = TreeStore::in_memory().unwrap();
    store.set_drive_id("drive").unwrap();
    store.commit_page(&[root(), file("A", "R", "a", "a1"), file("B", "R", "b", "b1")], "next", &["A".to_owned()]).unwrap();
    assert_eq!(shows(&mut store), ["a: Synced", "b: RemoteOnly"]);
    commit_delta(&mut store, &[file("A", "R", "moved", "a1")], &["A"]);
    assert_eq!(shows(&mut store), ["b: RemoteOnly", "moved: Synced"]);
    commit_delta(&mut store, &[file("A", "R", "moved", "a2")], &[]);
    assert_eq!(shows(&mut store), ["b: RemoteOnly", "moved: RemoteOnly"]);
    assert_eq!(FileSyncState::RemoteOnly, bound(&mut store, "A").unwrap().file_sync_state().unwrap());
}

/// An item removed here whose removal is not sent takes what OneDrive changed of it as what
/// the two sides agree on, and stays not shown. A file whose content was changed here, that
/// the outbox does not hold and that has another content in OneDrive, has OneDrive's: the
/// cycle step removed its copy.
#[test]
fn what_was_removed_here_is_not_shown_again() {
    let mut store = listed(&[root()]);
    commit_delta(&mut store, &[file("A", "R", "a", "a1"), file("B", "R", "b", "b1")], &["A", "B"]);
    let (a, b) = (bound_id(&mut store, "A"), bound_id(&mut store, "B"));
    store.item_store().apply(&LocalVersionChange::Removed { item: a }).unwrap();
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: b }).unwrap();
    commit_delta(&mut store, &[file("A", "R", "renamed", "a1"), file("B", "R", "b", "b2")], &[]);
    assert_eq!(shows(&mut store), ["b: RemoteOnly"]);
    let row = item(&mut store, a);
    assert_eq!((row.local, row.synced_place), (None, Some(place(ROOT, "renamed"))));
    assert_eq!(item(&mut store, b).local.unwrap().content, Some(content("b2")));
}

/// Nothing is written while the store does not know its drive: no row can be bound.
#[test]
fn without_the_drive_no_row_is_made() {
    let mut store = TreeStore::in_memory().unwrap();
    store.commit_page(&[root(), file("A", "R", "a", "a1")], "next", &[]).unwrap();
    assert!(shows(&mut store).is_empty());
    store.set_drive_id("drive").unwrap();
    store.commit_page(&[], "next", &[]).unwrap();
    assert_eq!(shows(&mut store), ["a: RemoteOnly"]);
}

/// A directory of the user's own is no place for an item of OneDrive, whatever the rows
/// said before: an item of OneDrive below a directory that gave its binding up waits.
#[test]
fn a_local_only_directory_in_the_way_keeps_what_comes_below_it_not_shown() {
    let mut store = listed(&[root()]);
    make_directory(&mut store, ROOT, "docs");
    commit_delta(&mut store, &[directory("D", "R", "docs"), file("C", "D", "c.txt", "c1")], &[]);
    assert_eq!(shows(&mut store), ["docs: directory"]);
    assert!(bound(&mut store, "C").unwrap().local.is_none());
}

/// One delta makes a directory and moves a file into it, and makes a file at the name
/// another leaves: the file moves once its new directory is shown, and the new file takes
/// the name once it is free, in the same commit.
#[test]
fn what_one_delta_makes_and_moves_into_or_out_of_its_way_is_placed_in_one_commit() {
    let mut store = listed(&[root(), file("A", "R", "a", "a1"), file("B", "R", "b", "b1")]);
    commit_delta(&mut store, &[directory("P", "R", "p"), directory("Q", "P", "q"), file("A", "Q", "a", "a1"), file("B", "R", "c", "b1"), file("N", "R", "b", "n1")], &[]);
    assert_eq!(shows(&mut store), ["b: RemoteOnly", "c: RemoteOnly", "p: directory", "p/q: directory", "p/q/a: RemoteOnly"]);
    let q = bound_id(&mut store, "Q");
    assert_eq!(bound(&mut store, "A").unwrap().synced_place, Some(place(q, "a")));
}

/// A file that OneDrive moved into a directory the folder cannot show, because an item of
/// the user's own keeps the directory's name, stays where it is: it is not placed in a
/// directory the folder does not show.
#[test]
fn a_file_moved_into_a_directory_the_folder_does_not_show_stays_where_it_is() {
    let mut store = listed(&[root(), file("A", "R", "a", "a1")]);
    make_directory(&mut store, ROOT, "p");
    commit_delta(&mut store, &[directory("P", "R", "p"), file("A", "P", "a", "a1")], &[]);
    assert_eq!(shows(&mut store), ["a: RemoteOnly", "p: directory"]);
    assert_eq!(bound(&mut store, "A").unwrap().synced_place, Some(place(ROOT, "a")));
}
