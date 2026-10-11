//! The read-only view: what a reader sees of a writer's transactions, and each reader's
//! answer over rows built by the store's own operations.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::*;
use crate::item_store::tests::listed_drive::{listed_directory, listed_file, listed_gone, not_placed, reference, store_with_listed_root, write_listing_of_changes};
use crate::item_store::tests::shown_items::{bound, row, show_listed_directory, show_listed_file, store_in_file_with_listed_root};
use crate::item_store::tests::{made, make_directory, make_file, place};
use crate::item_store::apply_change;
use crate::model::{ContentPresence, ContentVersion, FolderVisibility, ItemKind, LocalVersionChange, SkipReason};
use crate::sync_plan::ItemVersions;
use crate::{FoldedName, StepCommitOutcome};

const ROOT: LocalItemId = LocalItemId::ROOT;

fn ids(items: &[Item]) -> Vec<LocalItemId> {
    items.iter().map(|item| item.local_item_id).collect()
}

/// The names the folder shows in the root, as `reader` reads them.
fn root_names(reader: &ItemReader<'_>) -> Vec<String> {
    reader.list_children(ROOT).unwrap().into_iter().map(|child| child.local.unwrap().place.unwrap().name.to_string_lossy().into_owned()).collect()
}

fn remove(store: &mut TreeStore, item: LocalItemId) {
    store.item_store().apply(&LocalVersionChange::Removed { item }).unwrap();
}

/// A file made here with a content of `size` bytes.
fn make_file_of_size(store: &mut TreeStore, parent: LocalItemId, name: &str, size: u64, is_name_ignored: bool) -> LocalItemId {
    let content = Some(ContentVersion::ChangedHere { size, mtime: 200, mtime_nsec: 0, change_count: 1 });
    store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::File, place: place(parent, name), content, is_name_ignored, replaced: None }).unwrap()
}

/// A connection opened for reading alone reads what the writer's connection committed,
/// through a reader.
#[test]
fn a_reader_of_a_store_opened_for_reading_alone_sees_what_the_writer_committed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.sqlite");
    let mut writer = store_in_file_with_listed_root(&path);
    let reading = TreeStore::open_read_only(&path).unwrap();
    assert_eq!(reading.read_items(|reader| Ok(root_names(reader))).unwrap(), Vec::<String>::new());

    let made_file = make_file(&mut writer, ROOT, "a");
    let seen = reading.read_items(|reader| Ok((reader.find_item(made_file)?, reader.find_item_at(ROOT, OsStr::new("a"))?))).unwrap();
    assert_eq!(seen.0.as_ref().map(|item| item.local_item_id), Some(made_file));
    assert_eq!(seen.0, seen.1);
}

/// While the writer has a transaction open the reader sees nothing of it, and does not
/// wait; once it is committed the reader sees all of it: never half a transaction.
#[test]
fn a_reader_sees_nothing_of_an_open_write_transaction_and_all_of_it_once_committed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.sqlite");
    let writer = store_in_file_with_listed_root(&path);
    let reading = TreeStore::open_read_only(&path).unwrap();

    let transaction = rusqlite::Transaction::new_unchecked(&writer.conn, rusqlite::TransactionBehavior::Immediate).unwrap();
    let directory = apply_change(&transaction, &made(place(ROOT, "d"), ItemKind::Directory)).unwrap();
    assert_eq!(reading.read_items(|reader| Ok(root_names(reader))).unwrap(), Vec::<String>::new(), "the first half is written and not committed");
    let file = apply_change(&transaction, &made(place(directory, "f"), ItemKind::File)).unwrap();
    assert_eq!(reading.read_items(|reader| Ok((root_names(reader), reader.find_item(file)?))).unwrap(), (vec![], None));
    transaction.commit().unwrap();

    let (names, found, path_of_file) = reading.read_items(|reader| Ok((root_names(reader), reader.find_item(file)?, reader.find_path(file)?))).unwrap();
    assert_eq!(names, ["d"]);
    assert!(found.is_some());
    assert_eq!(path_of_file, Some(PathBuf::from("d/f")));
}

/// One closure is one read transaction: what it reads first and what it reads after the
/// writer committed in between are of one moment. The next closure sees the commit.
#[test]
fn a_closure_reads_one_moment_though_the_writer_commits_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.sqlite");
    let mut writer = store_in_file_with_listed_root(&path);
    make_file(&mut writer, ROOT, "a");
    let reading = TreeStore::open_read_only(&path).unwrap();

    let (before, after, counted, late) = reading
        .read_items(|reader| {
            let before = root_names(reader);
            let late = make_file(&mut writer, ROOT, "b");
            Ok((before, root_names(reader), reader.count_items()?.shown, reader.find_item(late)?))
        })
        .unwrap();
    assert_eq!(before, ["a"]);
    assert_eq!(after, before, "the second read is of the moment of the first");
    assert_eq!(counted, 1);
    assert_eq!(late, None);
    assert_eq!(reading.read_items(|reader| Ok(root_names(reader))).unwrap(), ["a", "b"], "the next closure is of a later moment");
}

/// A connection opened for reading alone refuses every write: a statement run on it, and
/// an operation of `ItemStore` over it. `ItemReader` itself has no operation that writes.
#[test]
fn a_reader_cannot_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.sqlite");
    drop(store_in_file_with_listed_root(&path));
    let mut reading = TreeStore::open_read_only(&path).unwrap();

    let is_read_only = |error: TreeError| matches!(&error, TreeError::Sql(rusqlite::Error::SqliteFailure(failure, _)) if failure.code == rusqlite::ErrorCode::ReadOnly) || panic!("not refused as read-only: {error:?}");
    assert!(is_read_only(reading.bench_sql("UPDATE meta SET value = 'x' WHERE key = 'drive_id'").unwrap_err()));
    assert!(is_read_only(reading.item_store().apply(&made(place(ROOT, "a"), ItemKind::File)).unwrap_err()));
    assert!(is_read_only(reading.read_items(|reader| Ok(reader.store.conn.execute("DELETE FROM pins", [])?)).unwrap_err()));
    assert_eq!(reading.read_items(|reader| Ok(root_names(reader))).unwrap(), Vec::<String>::new());
}

/// Over the writer's own connection a read sees the last write of the job it is in: with
/// no transaction open, and inside one that is open, which it neither ends nor nests in.
#[test]
fn a_read_over_the_writers_connection_sees_its_own_last_write() {
    let mut store = TreeStore::in_memory().unwrap();
    make_file(&mut store, ROOT, "a");
    assert_eq!(store.read_items(|reader| Ok(root_names(reader))).unwrap(), ["a"]);
    assert_eq!(root_names(&store.item_store().reader()), ["a"]);

    let transaction = rusqlite::Transaction::new_unchecked(&store.conn, rusqlite::TransactionBehavior::Immediate).unwrap();
    apply_change(&transaction, &made(place(ROOT, "b"), ItemKind::File)).unwrap();
    assert_eq!(store.read_items(|reader| Ok(root_names(reader))).unwrap(), ["a", "b"]);
    assert!(!store.conn.is_autocommit(), "the read did not end the transaction it ran in");
    drop(transaction);
    assert_eq!(store.read_items(|reader| Ok(root_names(reader))).unwrap(), ["a"], "what was not committed is gone");
}

/// The shared store reads through its reading connection, from async code and from a
/// plain thread, what its writer committed; a store in memory through its own thread.
#[test]
fn the_shared_store_reads_what_its_writer_committed() {
    let dir = tempfile::tempdir().unwrap();
    let in_file = Store::new(TreeStore::open(&dir.path().join("tree.sqlite")).unwrap());
    let in_memory = Store::new(TreeStore::in_memory().unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    for store in [in_file, in_memory] {
        assert_eq!(store.read_items_blocking(|reader| Ok(root_names(reader))).unwrap(), Vec::<String>::new());
        store.call_blocking(|tree| Ok(make_file(tree, ROOT, "a"))).unwrap();
        assert_eq!(store.read_items_blocking(|reader| Ok(root_names(reader))).unwrap(), ["a"]);
        assert_eq!(runtime.block_on(store.read_items(|reader| Ok(root_names(reader)))).unwrap(), ["a"]);
        runtime.block_on(store.close());
    }
}

#[test]
fn an_item_is_found_by_its_path_and_a_path_through_a_file_finds_nothing() {
    let mut store = TreeStore::in_memory().unwrap();
    let directory = make_directory(&mut store, ROOT, "d");
    let inner = make_directory(&mut store, directory, "inner");
    let file = make_file(&mut store, inner, "f");
    let reader = store.item_store();
    let reader = reader.reader();
    let find = |path: &str| reader.find_item_by_path(Path::new(path)).unwrap().map(|item| item.local_item_id);

    assert_eq!(find(""), Some(ROOT));
    assert_eq!(find("d"), Some(directory));
    assert_eq!(find("d/inner"), Some(inner));
    assert_eq!(find("d/inner/f"), Some(file));
    assert_eq!(find("./d/inner/f"), Some(file));
    assert_eq!(find("d/inner/f/x"), None, "a path through a file");
    assert_eq!(find("d/none"), None);
    assert_eq!(find("d/../d"), None, "no name of the rows is `..`");
    assert_eq!(find("/d"), None, "a path relative to the root of the folder");
}

/// S18: the marks line up with the names of the path, the outermost first, one for each
/// name — the item id of the binding of a directory that has one.
#[test]
fn the_marks_of_a_path_are_one_for_each_name_the_outermost_first() {
    let mut store = store_with_listed_root();
    let a = show_listed_directory(&mut store, "A", "R", "a");
    let b = make_directory(&mut store, a, "b");
    let c = show_listed_directory(&mut store, "C", "A", "c");
    let made_file = make_file(&mut store, b, "f");
    let listed_file_id = show_listed_file(&mut store, "F", "C", "g", "h1");
    write_listing_of_changes(&mut store, &[listed_file("N", "R", "never-shown", (3, "h2"), "e-N")]);
    let never_shown = bound(&mut store, "N").local_item_id;
    let store = store.item_store();
    let reader = store.reader();
    let mark = |id: &str| Some(OneDriveItemId::new(id));

    assert_eq!(reader.find_path_with_marks(ROOT).unwrap(), Some((PathBuf::new(), vec![])));
    assert_eq!(reader.find_path_with_marks(a).unwrap(), Some((PathBuf::from("a"), vec![mark("A")])));
    assert_eq!(reader.find_path_with_marks(b).unwrap(), Some((PathBuf::from("a/b"), vec![mark("A"), None])));
    assert_eq!(reader.find_path_with_marks(made_file).unwrap(), Some((PathBuf::from("a/b/f"), vec![mark("A"), None, None])));
    assert_eq!(reader.find_path_with_marks(c).unwrap(), Some((PathBuf::from("a/c"), vec![mark("A"), mark("C")])));
    assert_eq!(reader.find_path_with_marks(listed_file_id).unwrap(), Some((PathBuf::from("a/c/g"), vec![mark("A"), mark("C"), None])), "a file's own id is no directory's mark");
    assert_eq!(reader.find_path_with_marks(never_shown).unwrap(), None);
    assert_eq!(reader.find_path_with_marks(LocalItemId::new(999)).unwrap(), None);
    for item in [a, b, c, made_file, listed_file_id] {
        let (path, marks) = reader.find_path_with_marks(item).unwrap().unwrap();
        assert_eq!(Some(&path), reader.find_path(item).unwrap().as_ref());
        assert_eq!(marks.len(), path.components().count());
    }
}

#[test]
fn every_reader_answers_what_the_item_stores_reader_of_the_same_name_answers() {
    let mut store = store_with_listed_root();
    let a = show_listed_directory(&mut store, "A", "R", "a");
    let shown_file = show_listed_file(&mut store, "F", "A", "f", "h1");
    let made_file = make_file(&mut store, a, "F");
    let ignored = make_file_of_size(&mut store, ROOT, "x.tmp", 1, true);
    write_listing_of_changes(&mut store, &[not_placed(listed_file("S", "A", "s", (3, "h2"), "e-S"))]);
    let not_shown = bound(&mut store, "S").local_item_id;
    store.set_pin("A", true).unwrap();
    store.item_store().begin_delta_listing().unwrap();
    let all = [ROOT, a, shown_file, made_file, ignored, not_shown];

    let items = store.item_store();
    let reader = items.reader();
    for item in all {
        assert_eq!(reader.find_item(item).unwrap(), items.find_item(item).unwrap());
        assert_eq!(reader.find_path(item).unwrap(), items.find_path(item).unwrap());
        assert_eq!(reader.is_ignored(item).unwrap(), items.is_ignored(item).unwrap());
        assert_eq!(reader.find_folder_visibility(item).unwrap(), items.find_folder_visibility(item).unwrap());
        assert_eq!(reader.is_pinned(item).unwrap(), items.is_pinned(item).unwrap());
        assert_eq!(reader.list_children(item).unwrap(), items.list_children(item).unwrap());
    }
    let name = OsStr::new("f");
    assert_eq!(reader.find_item_at(a, name).unwrap(), items.find_item_at(a, name).unwrap());
    assert_eq!(reader.find_bound_item(&reference("F")).unwrap(), items.find_bound_item(&reference("F")).unwrap());
    assert_eq!(reader.list_unsent(None, 10).unwrap(), items.list_unsent(None, 10).unwrap());
    let folded = FoldedName::of(name);
    assert_eq!(reader.list_children_with_folded_name(a, &folded).unwrap(), items.list_children_with_folded_name(a, &folded).unwrap());
    assert_eq!(reader.find_listing_under_way().unwrap(), items.find_listing_under_way().unwrap());

    // And the answers are those the rows were built for, not two empty ones.
    assert_eq!(reader.find_item_at(a, name).unwrap().map(|item| item.local_item_id), Some(shown_file));
    assert_eq!(reader.find_bound_item(&reference("F")).unwrap().map(|item| item.local_item_id), Some(shown_file));
    assert_eq!(ids(&reader.list_children(a).unwrap()), [made_file, shown_file]);
    assert_eq!(reader.find_path(shown_file).unwrap(), Some(PathBuf::from("a/f")));
    assert_eq!((reader.is_ignored(ignored).unwrap(), reader.is_ignored(made_file).unwrap()), (true, false));
    assert_eq!(reader.find_folder_visibility(not_shown).unwrap(), FolderVisibility::CannotBeShown);
    assert_eq!((reader.is_pinned(shown_file).unwrap(), reader.is_pinned(ignored).unwrap()), (true, false));
    assert_eq!(ids(&reader.list_unsent(None, 10).unwrap()), [made_file]);
    assert_eq!(ids(&reader.list_children_with_folded_name(a, &folded).unwrap()), [shown_file, made_file]);
    assert!(reader.find_listing_under_way().unwrap().is_some());
    assert!(reader.is_ignored(LocalItemId::new(999)).is_err(), "refused for an id with no row, as the store's");
}

#[test]
fn a_removed_file_is_found_by_the_place_the_two_sides_agreed_on() {
    let mut store = store_with_listed_root();
    let a = show_listed_directory(&mut store, "A", "R", "a");
    let file = show_listed_file(&mut store, "F", "A", "f", "h1");
    let directory = show_listed_directory(&mut store, "D", "A", "d");
    let moved = show_listed_file(&mut store, "M", "A", "m", "h2");
    let find = |store: &mut TreeStore, name: &str| store.item_store().reader().find_removed_file_at_synced_place(a, OsStr::new(name)).unwrap().map(|item| item.local_item_id);
    assert_eq!(find(&mut store, "f"), None, "the folder shows the file");

    remove(&mut store, file);
    remove(&mut store, directory);
    store.item_store().apply(&LocalVersionChange::Moved { item: moved, place: place(a, "n"), is_name_ignored: false }).unwrap();
    assert_eq!(find(&mut store, "f"), Some(file));
    assert_eq!(find(&mut store, "d"), None, "the row there is a directory's");
    assert_eq!(find(&mut store, "m"), None, "a file that was moved from there is shown elsewhere");
    assert_eq!(find(&mut store, "F"), None, "the name is compared as it was given");
    assert_eq!(store.item_store().reader().find_removed_file_at_synced_place(ROOT, OsStr::new("f")).unwrap(), None);

    // What it finds is what the store lets a made file replace.
    let replacing = store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::File, place: place(a, "f"), content: Some(ContentVersion::ChangedHere { size: 0, mtime: 1, mtime_nsec: 0, change_count: 1 }), is_name_ignored: false, replaced: Some(file) }).unwrap();
    assert_eq!(row(&mut store, replacing).onedrive_item, Some(reference("F")));
    assert_eq!(find(&mut store, "f"), None, "the made file took the binding, and the removed file's row went");

    // A file removed here that a listing then says is gone from OneDrive too: no binding
    // of an item OneDrive does not have is taken over.
    let gone = show_listed_file(&mut store, "G", "A", "g", "h3");
    remove(&mut store, gone);
    assert_eq!(find(&mut store, "g"), Some(gone));
    write_listing_of_changes(&mut store, &[listed_gone("G")]);
    assert!(row(&mut store, gone).synced_place.is_some(), "the place agreed on stands until the plan agrees on the removal");
    assert_eq!(find(&mut store, "g"), None);
}

#[test]
fn a_removed_file_whose_removal_was_sent_is_not_found_by_its_synced_place() {
    let mut store = store_with_listed_root();
    let file = show_listed_file(&mut store, "F", "R", "f", "h1");
    remove(&mut store, file);
    let removed = row(&mut store, file);
    assert_eq!(store.item_store().reader().find_removed_file_at_synced_place(ROOT, OsStr::new("f")).unwrap(), Some(removed.clone()));

    assert_eq!(store.item_store().commit_send_file_removal(&ItemVersions::of(&removed), 0).unwrap(), StepCommitOutcome::Written);
    assert!(store.item_store().find_item(file).unwrap().is_some(), "the row stays, with its binding alone");
    assert_eq!(store.item_store().reader().find_removed_file_at_synced_place(ROOT, OsStr::new("f")).unwrap(), None);
}

#[test]
fn the_shown_files_below_a_directory_are_read_at_any_depth_and_bounded() {
    let mut store = TreeStore::in_memory().unwrap();
    let top = make_directory(&mut store, ROOT, "top");
    let f1 = make_file(&mut store, top, "f1");
    let inner = make_directory(&mut store, top, "inner");
    let f2 = make_file(&mut store, inner, "f2");
    let deep = make_directory(&mut store, inner, "deep");
    let f3 = make_file(&mut store, deep, "f3");
    make_file(&mut store, ROOT, "beside");
    let gone = make_file(&mut store, inner, "gone");
    remove(&mut store, gone);
    let items = store.item_store();
    let reader = items.reader();
    let below = |directory, after, limit| ids(&reader.list_shown_files_below(directory, after, limit).unwrap());

    assert_eq!(below(top, None, 10), [f1, f2, f3], "by local item id, the directories left out");
    assert_eq!(below(top, None, 2), [f1, f2]);
    assert_eq!(below(top, Some(f2), 2), [f3]);
    assert_eq!(below(top, Some(f3), 2), []);
    assert_eq!(below(inner, None, 10), [f2, f3]);
    assert_eq!(below(f1, None, 10), [], "nothing is below a file");
    assert_eq!(below(ROOT, None, 10).len(), 4);
}

/// A drive with a part the folder cannot show, `vault` with a file and a directory that
/// cannot be shown by itself either, and a file that cannot be shown by itself.
fn store_with_parts_not_shown() -> TreeStore {
    let mut store = store_with_listed_root();
    show_listed_directory(&mut store, "A", "R", "a");
    show_listed_file(&mut store, "F", "A", "f", "h1");
    write_listing_of_changes(
        &mut store,
        &[
            not_placed(listed_directory("V", "A", "vault", "e-V")),
            listed_file("VF", "V", "inside", (3, "h2"), "e-VF"),
            not_placed(listed_directory("VD", "V", "nested", "e-VD")),
            listed_file("VDF", "VD", "deep", (3, "h3"), "e-VDF"),
            not_placed(listed_file("N", "R", "notes.one", (3, "h4"), "e-N")),
            listed_file("W", "R", "waits-to-be-shown", (3, "h5"), "e-W"),
        ],
    );
    store
}

#[test]
fn the_counts_tell_what_onedrive_has_what_is_shown_and_what_cannot_be_shown() {
    let mut store = store_with_parts_not_shown();
    make_file(&mut store, ROOT, "made-here");
    let counts = store.item_store().reader().count_items().unwrap();
    assert_eq!(counts, ItemCounts { in_onedrive: 8, shown: 3, cannot_be_shown: 5, pinned: counts.pinned }, "the vault with the three below it, and the file; an item below two parts once");
    assert_eq!(TreeStore::in_memory().unwrap().item_store().reader().count_items().unwrap(), ItemCounts::default());
}

#[test]
fn of_what_cannot_be_shown_the_top_item_of_each_part_is_listed_with_its_reason() {
    let mut store = store_with_parts_not_shown();
    let (vault, notes) = (bound(&mut store, "V"), bound(&mut store, "N"));
    let items = store.item_store();
    let reader = items.reader();

    let listed = reader.list_items_not_shown(None, 10).unwrap();
    assert_eq!(
        listed,
        [
            ItemNotShown { item: vault.clone(), onedrive_path: PathBuf::from("a/vault"), reason: SkipReason::Unsupported },
            ItemNotShown { item: notes.clone(), onedrive_path: PathBuf::from("notes.one"), reason: SkipReason::Unsupported },
        ],
        "not what is below the vault, also what cannot be shown by itself there"
    );
    assert_eq!(reader.list_items_not_shown(None, 1).unwrap(), listed[..1]);
    assert_eq!(reader.list_items_not_shown(Some(vault.local_item_id), 10).unwrap(), listed[1..]);
    assert_eq!(reader.list_items_not_shown(Some(notes.local_item_id), 10).unwrap(), []);
}

#[test]
fn the_unsent_total_leaves_out_what_is_left_here_and_sums_the_local_content() {
    let mut store = store_with_listed_root();
    assert_eq!(store.item_store().reader().count_unsent().unwrap(), UnsentTotal::default());
    let changed = show_listed_file(&mut store, "C", "R", "changed", "h1");
    let moved = show_listed_file(&mut store, "M", "R", "moved", "h2");
    let removed = show_listed_file(&mut store, "G", "R", "removed", "h3");
    show_listed_file(&mut store, "S", "R", "synced", "h4");
    assert_eq!(store.item_store().reader().count_unsent().unwrap(), UnsentTotal::default(), "everything agrees");

    make_file_of_size(&mut store, ROOT, "new", 10, false);
    make_directory(&mut store, ROOT, "new-directory");
    make_file_of_size(&mut store, ROOT, "x.tmp", 1000, true);
    store.item_store().set_content_presence(changed, ContentPresence::OnDisk).unwrap();
    store.item_store().apply_all(&[LocalVersionChange::OpenedForWriting { item: changed }, LocalVersionChange::ClosedChanged { item: changed, size: 7, mtime: 300, mtime_nsec: 0 }]).unwrap();
    store.item_store().apply(&LocalVersionChange::Moved { item: moved, place: place(ROOT, "moved-here"), is_name_ignored: false }).unwrap();
    remove(&mut store, removed);

    assert_eq!(store.item_store().reader().count_unsent().unwrap(), UnsentTotal { count: 5, bytes: 17 }, "the new file's 10 and the changed file's 7; not the moved file's content, nor what is left here");
}

#[test]
fn the_items_left_here_are_read_bounded() {
    let mut store = TreeStore::in_memory().unwrap();
    make_file(&mut store, ROOT, "sent");
    let lock = make_file_of_size(&mut store, ROOT, "x.tmp", 1, true);
    let cache = store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::Directory, place: place(ROOT, ".cache"), content: None, is_name_ignored: true, replaced: None }).unwrap();
    let below = make_file(&mut store, cache, "below");
    let items = store.item_store();
    let reader = items.reader();
    let left = |after, limit| ids(&reader.list_items_left_here(after, limit).unwrap());

    assert_eq!(left(None, 10), [lock, cache, below]);
    assert_eq!(left(None, 2), [lock, cache]);
    assert_eq!(left(Some(cache), 2), [below]);
    assert_eq!(left(Some(below), 2), []);
}

#[test]
fn the_path_of_an_item_the_folder_does_not_show_is_that_of_its_synced_place() {
    let mut store = store_with_listed_root();
    let a = show_listed_directory(&mut store, "A", "R", "a");
    let b = show_listed_directory(&mut store, "B", "A", "b");
    let file = show_listed_file(&mut store, "F", "B", "f", "h1");
    let made_file = make_file(&mut store, a, "made");
    write_listing_of_changes(&mut store, &[listed_file("N", "R", "never-shown", (3, "h2"), "e-N")]);
    let never_shown = bound(&mut store, "N").local_item_id;
    let synced_path = |store: &mut TreeStore, item| store.item_store().reader().find_synced_path(item).unwrap();
    assert_eq!(synced_path(&mut store, file), Some(PathBuf::from("a/b/f")));
    assert_eq!(synced_path(&mut store, made_file), None, "nothing was agreed on");
    assert_eq!(synced_path(&mut store, never_shown), None);
    assert_eq!(synced_path(&mut store, ROOT), None);

    remove(&mut store, file);
    assert_eq!(store.item_store().reader().find_path(file).unwrap(), None);
    assert_eq!(synced_path(&mut store, file), Some(PathBuf::from("a/b/f")));

    // The directory it was in is shown under another name: the path is below it as shown.
    store.item_store().apply(&LocalVersionChange::Moved { item: a, place: place(ROOT, "renamed"), is_name_ignored: false }).unwrap();
    assert_eq!(synced_path(&mut store, file), Some(PathBuf::from("renamed/b/f")));

    // The directory was removed with it: its path is the one agreed on too.
    remove(&mut store, b);
    assert_eq!(synced_path(&mut store, file), Some(PathBuf::from("renamed/b/f")));
    assert_eq!(synced_path(&mut store, b), Some(PathBuf::from("renamed/b")));
}

#[test]
fn the_unsent_removals_are_read_in_the_order_they_came_to_be_unsent() {
    let mut store = store_with_listed_root();
    let first = show_listed_file(&mut store, "F1", "R", "f1", "h1");
    let directory = show_listed_directory(&mut store, "D", "R", "d");
    let inside = show_listed_file(&mut store, "F2", "D", "f2", "h2");
    let second = show_listed_file(&mut store, "F3", "R", "f3", "h3");
    let never_sent = make_file(&mut store, ROOT, "never-sent");
    let moved = show_listed_file(&mut store, "M", "R", "m", "h4");

    remove(&mut store, second);
    remove(&mut store, inside);
    remove(&mut store, directory);
    store.item_store().apply(&LocalVersionChange::Moved { item: moved, place: place(ROOT, "n"), is_name_ignored: false }).unwrap();
    remove(&mut store, never_sent);
    remove(&mut store, first);

    let removals = |store: &mut TreeStore, after, limit| ids(&store.item_store().reader().list_unsent_removals(after, limit).unwrap());
    assert_eq!(removals(&mut store, None, 10), [second, inside, directory, first], "a removed directory and what was removed with it are rows of their own");
    assert_eq!(removals(&mut store, None, 2), [second, inside]);
    let stamp_of_inside = row(&mut store, inside).unsent_since;
    assert_eq!(removals(&mut store, stamp_of_inside, 10), [directory, first]);

    let sent = row(&mut store, second);
    store.item_store().commit_send_file_removal(&ItemVersions::of(&sent), 0).unwrap();
    assert_eq!(removals(&mut store, None, 10), [inside, directory, first], "a removal that was sent is unsent no more");
}

#[test]
fn the_pin_that_holds_for_an_item_is_that_of_the_nearest_directory_above_before_its_own() {
    let mut store = store_with_listed_root();
    let a = show_listed_directory(&mut store, "A", "R", "a");
    let b = show_listed_directory(&mut store, "B", "A", "b");
    let f = show_listed_file(&mut store, "F", "B", "f", "h1");
    let made_here = make_file(&mut store, b, "made-here");
    let beside = show_listed_file(&mut store, "G", "R", "beside", "h2");
    let gone = show_listed_file(&mut store, "X", "A", "gone", "h3");
    let holders = |store: &mut TreeStore| {
        let items = store.item_store();
        let reader = items.reader();
        let holders: Vec<Option<LocalItemId>> = [a, b, f, made_here, beside, gone].into_iter().map(|item| reader.find_pin_holder(item).unwrap()).collect();
        // Whether a pin holds is whether somebody holds it.
        for (item, holder) in [a, b, f, made_here, beside, gone].into_iter().zip(&holders) {
            assert_eq!(reader.is_pinned(item).unwrap(), holder.is_some(), "item {item}");
        }
        holders
    };
    assert_eq!(holders(&mut store), [None; 6], "no pin");

    store.item_store().set_item_pin(f, true).unwrap();
    assert_eq!(holders(&mut store), [None, None, Some(f), None, None, None], "the file's own pin, and nobody else's");

    store.item_store().set_item_pin(a, true).unwrap();
    assert_eq!(holders(&mut store), [Some(a), Some(a), Some(a), Some(a), None, Some(a)], "a directory above comes before the item's own pin; a file made here is held by it too");

    store.item_store().set_item_pin(b, true).unwrap();
    assert_eq!(holders(&mut store), [Some(a), Some(a), Some(b), Some(b), None, Some(a)], "the nearest directory above; a directory with a pin of its own is held by the one above it");

    store.item_store().set_item_pin(a, false).unwrap();
    assert_eq!(holders(&mut store), [None, Some(b), Some(b), Some(b), None, None]);

    // An item the folder does not show is held by nobody, and neither is an id with no row.
    store.item_store().set_item_pin(gone, true).unwrap();
    assert_eq!(holders(&mut store)[5], Some(gone));
    remove(&mut store, gone);
    assert_eq!(store.item_store().reader().find_pin_holder(gone).unwrap(), None);
    assert_eq!(store.item_store().reader().find_pin_holder(LocalItemId::new(999)).unwrap(), None);
}

fn never_shown_below(store: &mut TreeStore, directory: LocalItemId) -> ItemsNeverShown {
    store.item_store().reader().find_onedrive_items_never_shown_below(directory).unwrap()
}

#[test]
fn what_onedrive_has_below_a_directory_that_was_never_shown_is_told_from_what_was_removed_here() {
    let mut store = store_with_listed_root();
    let a = show_listed_directory(&mut store, "A", "R", "a");
    let sub = show_listed_directory(&mut store, "S", "A", "sub");
    let f = show_listed_file(&mut store, "F", "A", "f", "h1");
    let deep = show_listed_file(&mut store, "D", "S", "deep", "h2");
    let other = show_listed_directory(&mut store, "O", "R", "other");
    assert_eq!(never_shown_below(&mut store, a), ItemsNeverShown::Nothing, "all OneDrive has in it the folder shows");

    // An ordinary `rm -r`: what was shown and removed here, its removal unsent, is none of it.
    remove(&mut store, deep);
    remove(&mut store, sub);
    remove(&mut store, f);
    assert_eq!(never_shown_below(&mut store, a), ItemsNeverShown::Nothing, "the last rmdir of an rm -r finds nothing");

    // A listing brings a file no step has shown yet, at any depth.
    write_listing_of_changes(&mut store, &[listed_file("W", "S", "waits", (3, "h3"), "e-W")]);
    assert_eq!(never_shown_below(&mut store, a), ItemsNeverShown::NotShownYet);
    assert_eq!(never_shown_below(&mut store, other), ItemsNeverShown::Nothing, "another directory");

    // And an item the folder cannot show: that one decides.
    write_listing_of_changes(&mut store, &[not_placed(listed_file("N", "A", "notes.one", (3, "h4"), "e-N"))]);
    assert_eq!(never_shown_below(&mut store, a), ItemsNeverShown::CannotBeShown);
    write_listing_of_changes(&mut store, &[listed_gone("N")]);
    assert_eq!(never_shown_below(&mut store, a), ItemsNeverShown::NotShownYet);

    // What is below a directory that cannot be shown cannot be shown either.
    write_listing_of_changes(&mut store, &[not_placed(listed_directory("V", "O", "vault", "e-V")), listed_file("VF", "V", "inside", (3, "h5"), "e-VF")]);
    assert_eq!(never_shown_below(&mut store, other), ItemsNeverShown::CannotBeShown);
    assert_eq!(never_shown_below(&mut store, f), ItemsNeverShown::Nothing, "nothing is below a file");
}

#[test]
fn an_item_an_upload_left_below_a_directory_is_none_the_folder_never_showed() {
    use crate::step_memory::{MismatchedItem, MismatchedItemStanding};
    let mut store = store_with_listed_root();
    let a = show_listed_directory(&mut store, "A", "R", "a");
    let f = show_listed_file(&mut store, "F", "A", "f", "h1");
    // A listing brings the item an upload of `f` left with other content: no step shows it.
    write_listing_of_changes(&mut store, &[listed_file("BAD", "A", "f 1", (4, "h9"), "e-BAD")]);
    assert_eq!(never_shown_below(&mut store, a), ItemsNeverShown::NotShownYet, "before the step memory says whose it is");
    let left = MismatchedItem::of_answer(reference("BAD"), Some("c-h9"), None);
    assert_eq!(store.step_memory().remember_mismatched_item(f, &left).unwrap(), MismatchedItemStanding::LeftToRemove);
    assert_eq!(never_shown_below(&mut store, a), ItemsNeverShown::Nothing, "the folder's own leftover goes with the directory");

    // Beside it, what was never shown still counts.
    write_listing_of_changes(&mut store, &[listed_file("W", "A", "waits", (3, "h3"), "e-W")]);
    assert_eq!(never_shown_below(&mut store, a), ItemsNeverShown::NotShownYet);
    write_listing_of_changes(&mut store, &[not_placed(listed_file("N", "A", "notes.one", (3, "h4"), "e-N"))]);
    assert_eq!(never_shown_below(&mut store, a), ItemsNeverShown::CannotBeShown);
}

#[test]
fn the_shown_directories_are_read_the_root_first_bounded_and_past_rows_with_no_path() {
    let mut store = TreeStore::in_memory().unwrap();
    let d1 = make_directory(&mut store, ROOT, "d1");
    let d2 = make_directory(&mut store, d1, "d2");
    make_file(&mut store, d1, "a-file");
    let orphan = make_directory(&mut store, ROOT, "orphan");
    let c1 = make_directory(&mut store, orphan, "c1");
    let c2 = make_directory(&mut store, orphan, "c2");
    let d3 = make_directory(&mut store, ROOT, "d3");
    let gone = make_directory(&mut store, ROOT, "gone");
    remove(&mut store, gone);
    let list = |store: &mut TreeStore, after, limit| store.item_store().reader().list_shown_directories(after, limit).unwrap();
    let path = |text: &str| PathBuf::from(text);

    let all = vec![(ROOT, path("")), (d1, path("d1")), (d2, path("d1/d2")), (orphan, path("orphan")), (c1, path("orphan/c1")), (c2, path("orphan/c2")), (d3, path("d3"))];
    assert_eq!(list(&mut store, None, 100), all, "by local item id, the root first with the empty path; no file, nothing removed here");
    assert_eq!(list(&mut store, None, 2), all[..2]);
    assert_eq!(list(&mut store, Some(d1), 2), all[2..4]);
    assert_eq!(list(&mut store, Some(d3), 2), []);

    // Rows that say a local place below a directory the folder does not show have no path
    // (no operation leaves such rows: written here past the store's operations). A page
    // that is all such rows does not end the read: it goes on to the next that has one.
    store.conn.execute("UPDATE items SET local_parent_id = NULL, local_name = NULL, local_folded_name = NULL WHERE local_item_id = ?1", [orphan.as_i64()]).unwrap();
    assert_eq!(store.item_store().reader().find_path(c1).unwrap(), None);
    let shown = vec![(ROOT, path("")), (d1, path("d1")), (d2, path("d1/d2")), (d3, path("d3"))];
    assert_eq!(list(&mut store, None, 100), shown);
    assert_eq!(list(&mut store, Some(d2), 1), [(d3, path("d3"))], "the pages of c1 and of c2 are passed over");
    assert_eq!(list(&mut store, Some(d2), 2), [(d3, path("d3"))]);
    assert_eq!(list(&mut store, None, 3), shown[..3], "never more than the limit");
    assert_eq!(list(&mut store, Some(d1), 2), shown[2..]);
    assert_eq!(list(&mut store, Some(d3), 1), []);
}
