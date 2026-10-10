use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use super::*;
use crate::model::{Change, ContentPresence, ContentVersion, DriveId, FileSyncState, ItemKind, ItemPlace, ItemVersion, Kind, OneDriveItemId, Placement, Row};

const ROOT: LocalItemId = LocalItemId::ROOT;

pub(super) fn place(parent: LocalItemId, name: &str) -> ItemPlace {
    ItemPlace { parent_local_item_id: parent, name: name.into() }
}

/// A content OneDrive has, told by `hash`.
pub(super) fn hashed(hash: &str) -> ContentVersion {
    ContentVersion::Hashed { size: 3, mtime: 100, mtime_nsec: 0, hash: Some(hash.to_owned()), ctag: Some(format!("c-{hash}")) }
}

/// The content of a file a program just made.
fn made_here() -> ContentVersion {
    ContentVersion::ChangedHere { size: 0, mtime: 200, mtime_nsec: 7, change_count: 1 }
}

pub(super) fn reference(id: &str) -> OneDriveItemReference {
    OneDriveItemReference { drive_id: DriveId::new("d1").unwrap(), item_id: OneDriveItemId::new(id) }
}

pub(super) fn made(place: ItemPlace, kind: ItemKind) -> LocalVersionChange {
    let content = (kind == ItemKind::File).then(made_here);
    LocalVersionChange::Made { version: ItemVersion { place: Some(place), kind, content }, is_ignored: false }
}

pub(super) fn make_file(store: &mut TreeStore, parent: LocalItemId, name: &str) -> LocalItemId {
    store.item_store().apply(&made(place(parent, name), ItemKind::File)).unwrap()
}

pub(super) fn make_directory(store: &mut TreeStore, parent: LocalItemId, name: &str) -> LocalItemId {
    store.item_store().apply(&made(place(parent, name), ItemKind::Directory)).unwrap()
}

pub(super) fn item(store: &mut TreeStore, id: LocalItemId) -> Item {
    store.item_store().find_item(id).unwrap().unwrap_or_else(|| panic!("item {id} has no row"))
}

/// `id` as it is once OneDrive has it under `onedrive_id` and the two sides agree: bound, on
/// disk, its synced place the local one, and for a file one hashed content on both sides.
pub(super) fn synced(store: &mut TreeStore, id: LocalItemId, onedrive_id: &str) -> Item {
    let mut row = item(store, id);
    let local = row.local.as_mut().expect("shown in the folder");
    if local.kind == ItemKind::File {
        local.content = Some(hashed(onedrive_id));
    }
    row.synced_place = local.place.clone();
    row.synced_content = local.content.clone();
    row.onedrive_item = Some(reference(onedrive_id));
    write_item(&store.conn, &row).unwrap();
    row
}

/// The code refused what would break a rule of the rows.
pub(super) fn is_refused<T: std::fmt::Debug>(result: Result<T, TreeError>) -> bool {
    matches!(&result, Err(TreeError::Refused(_))) || panic!("not refused: {result:?}")
}

/// The schema refused it: a unique index or a check.
pub(super) fn is_refused_by_schema<T: std::fmt::Debug>(result: Result<T, TreeError>) -> bool {
    matches!(&result, Err(TreeError::Sql(rusqlite::Error::SqliteFailure(e, _))) if e.code == rusqlite::ErrorCode::ConstraintViolation)
        || panic!("not refused by the schema: {result:?}")
}

fn names(store: &mut TreeStore, directory: LocalItemId) -> Vec<String> {
    let children = store.item_store().list_children(directory).unwrap();
    children.into_iter().map(|child| child.local.unwrap().place.unwrap().name.to_string_lossy().into_owned()).collect()
}

fn path(store: &mut TreeStore, id: LocalItemId) -> Option<PathBuf> {
    store.item_store().find_path(id).unwrap()
}

/// The root of the folder is item 1 in a new store: a directory the folder shows, with no
/// place, no binding and the empty path.
#[test]
fn the_root_is_item_1_a_directory_with_no_place() {
    let mut store = TreeStore::in_memory().unwrap();
    assert_eq!(ROOT.as_i64(), 1);
    assert_eq!(
        item(&mut store, ROOT),
        Item {
            local_item_id: ROOT,
            onedrive_item: None,
            local: Some(ItemVersion { place: None, kind: ItemKind::Directory, content: None }),
            synced_place: None,
            synced_content: None,
            content_presence: ContentPresence::NotOnDisk,
            content_before_write: None,
            is_ignored: false,
        }
    );
    assert_eq!(path(&mut store, ROOT), Some(PathBuf::new()));
    assert_eq!(make_file(&mut store, ROOT, "first").as_i64(), 2, "the first item made is the one after the root");
}

/// The root stays what it is: it is not removed, not moved, not saved over, and nothing is
/// made without a place.
#[test]
fn the_root_is_not_removed_or_given_a_place() {
    let mut store = TreeStore::in_memory().unwrap();
    let dir = make_directory(&mut store, ROOT, "dir");
    let mut items = store.item_store();
    assert!(is_refused(items.apply(&LocalVersionChange::Removed { item: ROOT })));
    assert!(is_refused(items.apply(&LocalVersionChange::Moved { item: ROOT, place: place(dir, "root"), is_ignored: false })));
    assert!(is_refused(items.apply(&LocalVersionChange::SavedOver { replacing: dir, replaced: ROOT, is_ignored: false })));
    assert!(is_refused(items.apply(&LocalVersionChange::Made { version: ItemVersion { place: None, kind: ItemKind::Directory, content: None }, is_ignored: false })));
    assert!(is_refused(items.exchange_local_places(ROOT, dir, false, false)));
}

/// An item made is found by its id, by its place, among its directory's children, and has
/// the path of the names from the root. A name is kept as it was given, also one that is not
/// UTF-8.
#[test]
fn a_made_item_is_found_by_its_id_its_place_and_in_its_directory() {
    let mut store = TreeStore::in_memory().unwrap();
    let dir = make_directory(&mut store, ROOT, "dir");
    let file = make_file(&mut store, dir, "File.txt");
    let odd = OsString::from_vec(vec![b'o', 0xff, b'd']);
    let odd_place = ItemPlace { parent_local_item_id: dir, name: odd.clone() };
    let odd_file = store.item_store().apply(&made(odd_place.clone(), ItemKind::File)).unwrap();

    let found = item(&mut store, file);
    assert_eq!(found.local, Some(ItemVersion { place: Some(place(dir, "File.txt")), kind: ItemKind::File, content: Some(made_here()) }));
    assert_eq!((found.onedrive_item.clone(), found.content_presence, found.is_ignored), (None, ContentPresence::OnDisk, false), "a file made here is on disk");
    assert_eq!(item(&mut store, dir).content_presence, ContentPresence::NotOnDisk, "a directory has no content");
    assert_eq!(store.item_store().find_item_at(dir, OsStr::new("File.txt")).unwrap(), Some(found.clone()));
    assert_eq!(store.item_store().find_item_at(dir, OsStr::new("file.txt")).unwrap(), None, "a name is not changed to compare it");
    assert_eq!(store.item_store().find_item_at(ROOT, OsStr::new("File.txt")).unwrap(), None);
    assert_eq!(store.item_store().find_item_at(dir, &odd).unwrap().map(|i| i.local_item_id), Some(odd_file));
    assert_eq!(item(&mut store, odd_file).local.unwrap().place, Some(odd_place));
    assert_eq!(store.item_store().list_children(dir).unwrap().iter().map(|i| i.local_item_id).collect::<Vec<_>>().len(), 2);
    assert_eq!(names(&mut store, ROOT), ["dir"]);
    assert_eq!(path(&mut store, file), Some(PathBuf::from("dir/File.txt")));
    assert_eq!(path(&mut store, odd_file), Some(PathBuf::from("dir").join(&odd)));
    assert_eq!(store.item_store().find_item(LocalItemId::new(99)).unwrap(), None);
    assert_eq!(path(&mut store, LocalItemId::new(99)), None);
    assert_eq!(store.item_store().find_bound_item(&reference("A")).unwrap(), None);
    let bound = synced(&mut store, file, "A");
    assert_eq!(store.item_store().find_bound_item(&reference("A")).unwrap(), Some(bound));
    let other_drive = OneDriveItemReference { drive_id: DriveId::new("d2").unwrap(), ..reference("A") };
    assert_eq!(store.item_store().find_bound_item(&other_drive).unwrap(), None, "the same item id in another drive is another item");
}

/// The rule "an item's parent exists and is a directory", on the local side: the code
/// refuses to make or move an item into a directory that has no row, into a file, and into
/// a directory the folder does not show.
#[test]
fn an_item_is_made_and_moved_only_into_a_directory_the_folder_shows() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "file");
    let moving = make_file(&mut store, ROOT, "moving");
    let gone = make_directory(&mut store, ROOT, "gone");
    synced(&mut store, gone, "G");
    store.item_store().apply(&LocalVersionChange::Removed { item: gone }).unwrap();
    for parent in [LocalItemId::new(99), file, gone] {
        let mut items = store.item_store();
        assert!(is_refused(items.apply(&made(place(parent, "new"), ItemKind::File))), "made in {parent}");
        assert!(is_refused(items.apply(&LocalVersionChange::Moved { item: moving, place: place(parent, "moved"), is_ignored: false })), "moved into {parent}");
    }
    assert_eq!(path(&mut store, moving), Some(PathBuf::from("moving")), "nothing was changed");
    assert_eq!(names(&mut store, ROOT), ["file", "moving"]);
}

/// A file has a content and a directory has none: a version that says otherwise is refused.
#[test]
fn a_file_is_made_with_a_content_and_a_directory_without() {
    let mut store = TreeStore::in_memory().unwrap();
    let mut items = store.item_store();
    let version = |kind, content| LocalVersionChange::Made { version: ItemVersion { place: Some(place(ROOT, "x")), kind, content }, is_ignored: false };
    assert!(is_refused(items.apply(&version(ItemKind::File, None))));
    assert!(is_refused(items.apply(&version(ItemKind::Directory, Some(made_here())))));
    assert_eq!(names(&mut store, ROOT), Vec::<String>::new());
}

/// The rule "in the folder, one item per name in a directory": a second item made at a name
/// and an item moved onto one are refused before the schema's unique index of local places
/// would refuse them, which holds the rule whoever writes the rows.
/// The same name in another directory, and a name of other letters' case, are other places.
#[test]
fn a_directory_has_one_item_per_name() {
    let mut store = TreeStore::in_memory().unwrap();
    let dir = make_directory(&mut store, ROOT, "dir");
    let a = make_file(&mut store, ROOT, "a");
    let b = make_file(&mut store, ROOT, "b");
    let mut items = store.item_store();
    assert!(is_refused(items.apply(&made(place(ROOT, "a"), ItemKind::File))));
    assert!(is_refused(items.apply(&made(place(ROOT, "a"), ItemKind::Directory))));
    assert!(is_refused(items.apply(&LocalVersionChange::Moved { item: b, place: place(ROOT, "a"), is_ignored: false })));
    assert!(is_refused_by_schema(items.store.bench_sql("UPDATE items SET local_name = 'a' WHERE local_name = 'b'")), "whoever writes the rows");
    items.apply(&made(place(dir, "a"), ItemKind::File)).unwrap();
    items.apply(&made(place(ROOT, "A"), ItemKind::File)).unwrap();
    assert_eq!(names(&mut store, ROOT), ["A", "a", "b", "dir"]);
    assert_eq!(item(&mut store, a).local.unwrap().place, Some(place(ROOT, "a")));
    assert_eq!(item(&mut store, b).local.unwrap().place, Some(place(ROOT, "b")), "the refused move changed nothing");
}

/// The rule "no item is above itself": the code refuses to move a directory into itself or
/// into what is below it, however deep.
#[test]
fn a_directory_is_not_moved_into_itself_or_below() {
    let mut store = TreeStore::in_memory().unwrap();
    let top = make_directory(&mut store, ROOT, "top");
    let middle = make_directory(&mut store, top, "middle");
    let bottom = make_directory(&mut store, middle, "bottom");
    let beside = make_directory(&mut store, ROOT, "beside");
    let mut items = store.item_store();
    for parent in [top, middle, bottom] {
        assert!(is_refused(items.apply(&LocalVersionChange::Moved { item: top, place: place(parent, "top"), is_ignored: false })), "into {parent}");
    }
    items.apply(&LocalVersionChange::Moved { item: bottom, place: place(beside, "up"), is_ignored: false }).unwrap();
    items.apply(&LocalVersionChange::Moved { item: top, place: place(bottom, "down"), is_ignored: false }).unwrap();
    assert_eq!(path(&mut store, middle), Some(PathBuf::from("beside/up/down/middle")), "what was below it once is not below it now");
}

/// A move gives the item its place, and with it everything below its path; what the upload
/// rules say of the new place is written for the item and all below it.
#[test]
fn a_move_changes_the_path_and_what_is_ignored_of_all_below() {
    let mut store = TreeStore::in_memory().unwrap();
    let dir = make_directory(&mut store, ROOT, "dir");
    let inner = make_directory(&mut store, dir, "inner");
    let file = make_file(&mut store, inner, "file");
    let beside = make_file(&mut store, ROOT, "beside");
    let moved = store.item_store().apply(&LocalVersionChange::Moved { item: dir, place: place(ROOT, ".cache"), is_ignored: true }).unwrap();
    assert_eq!(moved, dir, "the item keeps its id");
    assert_eq!(path(&mut store, file), Some(PathBuf::from(".cache/inner/file")));
    assert_eq!([dir, inner, file, beside].map(|id| item(&mut store, id).is_ignored), [true, true, true, false]);
    store.item_store().apply(&LocalVersionChange::Moved { item: dir, place: place(ROOT, "dir"), is_ignored: false }).unwrap();
    assert_eq!([dir, inner, file].map(|id| item(&mut store, id).is_ignored), [false, false, false]);
    let ignored = store.item_store().apply(&LocalVersionChange::Made { version: ItemVersion { place: Some(place(ROOT, "x.tmp")), kind: ItemKind::File, content: Some(made_here()) }, is_ignored: true }).unwrap();
    assert!(item(&mut store, ignored).is_ignored);
}

/// Two items exchange their places in one step, in one directory and across two, although
/// the index of local places is checked at each statement; nothing is left under another
/// name. A directory is not exchanged with what is below it.
#[test]
fn two_items_exchange_their_places() {
    let mut store = TreeStore::in_memory().unwrap();
    let a = make_file(&mut store, ROOT, "a");
    let b = make_directory(&mut store, ROOT, "b");
    let inside = make_file(&mut store, b, "inside");
    store.item_store().exchange_local_places(a, b, false, true).unwrap();
    assert_eq!(item(&mut store, a).local.unwrap().place, Some(place(ROOT, "b")));
    assert_eq!(item(&mut store, b).local.unwrap().place, Some(place(ROOT, "a")));
    assert_eq!(names(&mut store, ROOT), ["a", "b"]);
    assert_eq!(path(&mut store, inside), Some(PathBuf::from("a/inside")));
    assert_eq!([a, b, inside].map(|id| item(&mut store, id).is_ignored), [false, true, true]);

    store.item_store().exchange_local_places(inside, a, false, false).unwrap();
    assert_eq!(path(&mut store, inside), Some(PathBuf::from("b")));
    assert_eq!(path(&mut store, a), Some(PathBuf::from("a/inside")));
    assert_eq!((names(&mut store, ROOT), names(&mut store, b)), (vec!["a".to_owned(), "b".to_owned()], vec!["inside".to_owned()]));

    let deep = make_directory(&mut store, b, "deep");
    let deeper = make_file(&mut store, deep, "deeper");
    let mut items = store.item_store();
    assert!(is_refused(items.exchange_local_places(b, deeper, false, false)), "it would be below itself");
    assert!(is_refused(items.exchange_local_places(deeper, b, false, false)));
    assert!(is_refused(items.exchange_local_places(a, a, false, false)));
    assert!(is_refused(items.exchange_local_places(a, LocalItemId::new(99), false, false)));
    assert_eq!(path(&mut store, deeper), Some(PathBuf::from("a/deep/deeper")), "a refused exchange changed nothing");
}

/// A local item id is not given again after its row went: the kernel's node id of a removed
/// file never names another.
#[test]
fn an_id_is_not_reused_after_its_row_went() {
    let mut store = TreeStore::in_memory().unwrap();
    let first = make_file(&mut store, ROOT, "a");
    let second = make_file(&mut store, ROOT, "b");
    for id in [second, first] {
        store.item_store().apply(&LocalVersionChange::Removed { item: id }).unwrap();
        assert_eq!(store.item_store().find_item(id).unwrap(), None, "an item OneDrive never had goes with its removal");
    }
    let third = make_file(&mut store, ROOT, "a");
    assert!(third > second && third > first, "{third} after {first} and {second}");
}

/// A removal takes the local version. The row of an item OneDrive never had goes; the row
/// of a bound item stays, with what the two sides last agreed on: that is what is unsent.
/// A directory is removed after what is in it, never before.
#[test]
fn a_removal_takes_the_local_version_and_keeps_a_bound_row() {
    let mut store = TreeStore::in_memory().unwrap();
    let dir = make_directory(&mut store, ROOT, "dir");
    let file = make_file(&mut store, dir, "file");
    let before = synced(&mut store, file, "F");
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: file }).unwrap();
    assert!(is_refused(store.item_store().apply(&LocalVersionChange::Removed { item: dir })), "a directory that holds something");

    assert_eq!(store.item_store().apply(&LocalVersionChange::Removed { item: file }).unwrap(), file);
    assert_eq!(
        item(&mut store, file),
        Item { local: None, content_presence: ContentPresence::NotOnDisk, content_before_write: None, ..before },
        "the binding and the synced place and content stay"
    );
    assert_eq!(names(&mut store, dir), Vec::<String>::new());
    assert_eq!(path(&mut store, file), None, "an item the folder does not show has no path");
    store.item_store().apply(&LocalVersionChange::Removed { item: dir }).unwrap();
    assert_eq!(store.item_store().find_item(dir).unwrap(), None);
    assert!(is_refused(store.item_store().apply(&LocalVersionChange::Removed { item: file })), "removed already");
    assert!(is_refused(store.item_store().apply(&LocalVersionChange::Removed { item: LocalItemId::new(99) })));
}

/// A file made at the place of a file removed here whose removal is not sent yet (a bound
/// row the folder does not show, whose synced place is that place) takes over its binding:
/// OneDrive sees one item whose content changed. The new file has an id of its own, and the
/// removed row goes.
#[test]
fn a_file_made_where_one_was_removed_and_not_sent_takes_over_its_binding() {
    let mut store = TreeStore::in_memory().unwrap();
    let old = make_file(&mut store, ROOT, "report");
    let old_row = synced(&mut store, old, "A");
    store.item_store().apply(&LocalVersionChange::Removed { item: old }).unwrap();

    let new = make_file(&mut store, ROOT, "report");
    assert_ne!(new, old, "the kernel was told the old file is gone");
    assert_eq!(store.item_store().find_item(old).unwrap(), None, "the removed row went");
    let new_row = item(&mut store, new);
    assert_eq!((new_row.onedrive_item.clone(), new_row.synced_place.clone(), new_row.synced_content.clone()), (old_row.onedrive_item, old_row.synced_place, old_row.synced_content));
    assert_eq!(new_row.local.as_ref().unwrap().content, Some(made_here()));
    assert_eq!(new_row.file_sync_state(), Some(FileSyncState::ModifiedHere));
    assert_eq!(store.item_store().find_bound_item(&reference("A")).unwrap().map(|i| i.local_item_id), Some(new));
}

/// What does not take a binding over when it is made: a directory at a removed file's
/// place, a file at a removed directory's, a file at another name, a file at the place of a
/// bound row that has no synced place (a directory a listing only mentioned), and a file at
/// the place a removed file was moved to before it was removed (its synced place is where
/// OneDrive has it).
#[test]
fn only_a_file_at_a_removed_files_synced_place_takes_a_binding_over() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "file");
    let dir = make_directory(&mut store, ROOT, "dir");
    let moved = make_file(&mut store, ROOT, "moved");
    for (id, onedrive_id) in [(file, "F"), (dir, "D"), (moved, "M")] {
        synced(&mut store, id, onedrive_id);
    }
    store.item_store().apply(&LocalVersionChange::Moved { item: moved, place: place(ROOT, "elsewhere"), is_ignored: false }).unwrap();
    for id in [file, dir, moved] {
        store.item_store().apply(&LocalVersionChange::Removed { item: id }).unwrap();
    }
    let mentioned = Item {
        local_item_id: LocalItemId::new(50),
        onedrive_item: Some(reference("P")),
        local: None,
        synced_place: None,
        synced_content: None,
        content_presence: ContentPresence::NotOnDisk,
        content_before_write: None,
        is_ignored: false,
    };
    insert_item(&store.conn, &mentioned).unwrap();

    let made_ids = [
        make_directory(&mut store, ROOT, "file"),
        make_file(&mut store, ROOT, "dir"),
        make_file(&mut store, ROOT, "other"),
        make_file(&mut store, ROOT, "elsewhere"),
    ];
    for id in made_ids {
        assert_eq!(item(&mut store, id).onedrive_item, None, "{id}");
    }
    for (id, onedrive_id) in [(file, "F"), (dir, "D"), (moved, "M")] {
        assert_eq!(store.item_store().find_bound_item(&reference(onedrive_id)).unwrap().map(|i| i.local_item_id), Some(id), "{onedrive_id} keeps its row");
    }
    let at_moved = make_file(&mut store, ROOT, "moved");
    assert_eq!(item(&mut store, at_moved).onedrive_item, Some(reference("M")), "where OneDrive has the removed file");
}

/// A file saved over another (an editor's temporary file renamed over the original) has its
/// place and takes over its binding; the replaced row goes.
#[test]
fn a_file_saved_over_a_bound_file_takes_over_its_binding() {
    let mut store = TreeStore::in_memory().unwrap();
    let original = make_file(&mut store, ROOT, "report");
    let original_row = synced(&mut store, original, "A");
    let temporary = make_file(&mut store, ROOT, ".report.tmp");
    let saved = store.item_store().apply(&LocalVersionChange::SavedOver { replacing: temporary, replaced: original, is_ignored: false }).unwrap();
    assert_eq!(saved, temporary, "the id follows the file the kernel holds");
    assert_eq!(store.item_store().find_item(original).unwrap(), None);
    let row = item(&mut store, temporary);
    assert_eq!(row.local.as_ref().unwrap().place, Some(place(ROOT, "report")));
    assert_eq!((row.onedrive_item.clone(), row.synced_place.clone(), row.synced_content.clone()), (original_row.onedrive_item, original_row.synced_place, original_row.synced_content));
    assert_eq!(row.file_sync_state(), Some(FileSyncState::ModifiedHere));
    assert_eq!(names(&mut store, ROOT), ["report"]);
}

/// The other cases of one item renamed over another. Over an item OneDrive never had: its
/// row goes. A bound item over a bound one: each binding stays with its item, and the
/// replaced one is removed here. What `is_ignored` says is the replacing item's from then.
#[test]
fn an_item_renamed_over_another_takes_its_place() {
    let mut store = TreeStore::in_memory().unwrap();
    let new = make_file(&mut store, ROOT, "new");
    let never_sent = make_file(&mut store, ROOT, "never-sent");
    store.item_store().apply(&LocalVersionChange::SavedOver { replacing: new, replaced: never_sent, is_ignored: true }).unwrap();
    assert_eq!(store.item_store().find_item(never_sent).unwrap(), None);
    let row = item(&mut store, new);
    assert_eq!((row.local.unwrap().place, row.onedrive_item, row.is_ignored), (Some(place(ROOT, "never-sent")), None, true));

    let (a, b) = (make_file(&mut store, ROOT, "a"), make_file(&mut store, ROOT, "b"));
    synced(&mut store, a, "A");
    let b_row = synced(&mut store, b, "B");
    store.item_store().apply(&LocalVersionChange::SavedOver { replacing: a, replaced: b, is_ignored: false }).unwrap();
    let a_row = item(&mut store, a);
    assert_eq!((a_row.onedrive_item, a_row.local.unwrap().place, a_row.synced_place), (Some(reference("A")), Some(place(ROOT, "b")), Some(place(ROOT, "a"))));
    assert_eq!(item(&mut store, b), Item { local: None, content_presence: ContentPresence::NotOnDisk, ..b_row }, "removed here, to be removed in OneDrive");

    let (empty, full) = (make_directory(&mut store, ROOT, "empty"), make_directory(&mut store, ROOT, "full"));
    let inside = make_file(&mut store, full, "inside");
    store.item_store().apply(&LocalVersionChange::SavedOver { replacing: full, replaced: empty, is_ignored: false }).unwrap();
    assert_eq!(path(&mut store, inside), Some(PathBuf::from("empty/inside")));
    assert_eq!(store.item_store().find_item(empty).unwrap(), None);
}

/// What is refused of one item renamed over another: over itself, a file over a directory
/// or the other way, over a directory that holds something, and an item the folder does
/// not show on either side.
#[test]
fn an_item_is_renamed_only_over_one_of_its_kind_that_holds_nothing() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "file");
    let dir = make_directory(&mut store, ROOT, "dir");
    let full = make_directory(&mut store, ROOT, "full");
    make_file(&mut store, full, "inside");
    let gone = make_file(&mut store, ROOT, "gone");
    synced(&mut store, gone, "G");
    store.item_store().apply(&LocalVersionChange::Removed { item: gone }).unwrap();
    let mut items = store.item_store();
    for (replacing, replaced) in [(file, file), (file, dir), (dir, file), (dir, full), (file, gone), (gone, file), (file, LocalItemId::new(99))] {
        assert!(is_refused(items.apply(&LocalVersionChange::SavedOver { replacing, replaced, is_ignored: false })), "{replacing} over {replaced}");
    }
    assert_eq!(names(&mut store, ROOT), ["dir", "file", "full"]);
}

/// The first writer's open raises the local content before any byte is written and keeps
/// what the content was; a close that changed nothing puts it back, and a close that
/// changed it writes the new size and time and lets the former content go.
#[test]
fn a_writers_open_and_close_change_the_local_content() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "file");
    let clean = synced(&mut store, file, "A");
    assert_eq!(clean.file_sync_state(), Some(FileSyncState::Synced));

    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: file }).unwrap();
    let open = item(&mut store, file);
    assert_eq!(open.local.as_ref().unwrap().content, Some(ContentVersion::ChangedHere { size: 3, mtime: 100, mtime_nsec: 0, change_count: 1 }));
    assert_eq!(open.content_before_write, Some(hashed("A")));
    assert_eq!(open.file_sync_state(), Some(FileSyncState::ModifiedHere), "not clean from the open on");

    store.item_store().apply(&LocalVersionChange::ClosedUnchanged { item: file }).unwrap();
    assert_eq!(item(&mut store, file), clean, "as it was before the open");

    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: file }).unwrap();
    store.item_store().apply(&LocalVersionChange::ClosedChanged { item: file, size: 9, mtime: 300, mtime_nsec: 5 }).unwrap();
    let changed = item(&mut store, file);
    let first = ContentVersion::ChangedHere { size: 9, mtime: 300, mtime_nsec: 5, change_count: 1 };
    assert_eq!((changed.local.as_ref().unwrap().content.clone(), changed.content_before_write.clone()), (Some(first.clone()), None));
    assert_eq!(changed.synced_content, Some(hashed("A")), "what the two sides agreed on is not the writer's to change");

    // Changed again before it was sent: another content, told by the count.
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: file }).unwrap();
    let again = item(&mut store, file);
    assert_eq!(again.local.as_ref().unwrap().content, Some(ContentVersion::ChangedHere { size: 9, mtime: 300, mtime_nsec: 5, change_count: 2 }));
    assert_eq!(again.content_before_write, Some(first.clone()));
    // An open that finds a former content kept (no close was written: the daemon died)
    // keeps that one, and raises the count.
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: file }).unwrap();
    let reopened = item(&mut store, file);
    assert_eq!(reopened.content_before_write, Some(first.clone()));
    assert_eq!(reopened.local.as_ref().unwrap().content.as_ref().map(ContentVersion::change_count), Some(3));
    store.item_store().apply(&LocalVersionChange::ClosedUnchanged { item: file }).unwrap();
    assert_eq!(item(&mut store, file).local.unwrap().content, Some(first));
    store.item_store().apply(&LocalVersionChange::ClosedUnchanged { item: file }).unwrap();
    assert_eq!(item(&mut store, file), Item { content_before_write: None, ..changed }, "a close with no open before it changes nothing");
}

/// A truncation is a change of the content at once: another content of the new size, and no
/// former content to put back, so that a close that itself changed nothing leaves it.
#[test]
fn a_truncation_is_another_content_that_no_close_puts_back() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "file");
    synced(&mut store, file, "A");
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: file }).unwrap();
    store.item_store().apply(&LocalVersionChange::Truncated { item: file, size: 0, mtime: 400, mtime_nsec: 1 }).unwrap();
    let cut = ContentVersion::ChangedHere { size: 0, mtime: 400, mtime_nsec: 1, change_count: 2 };
    let row = item(&mut store, file);
    assert_eq!((row.local.as_ref().unwrap().content.clone(), row.content_before_write.clone()), (Some(cut.clone()), None));
    store.item_store().apply(&LocalVersionChange::ClosedUnchanged { item: file }).unwrap();
    assert_eq!(item(&mut store, file).local.unwrap().content, Some(cut));
    assert_eq!(item(&mut store, file).file_sync_state(), Some(FileSyncState::ModifiedHere));
}

/// Only a file has a content to change: a directory and the root are refused.
#[test]
fn only_a_file_has_its_content_changed() {
    let mut store = TreeStore::in_memory().unwrap();
    let dir = make_directory(&mut store, ROOT, "dir");
    let mut items = store.item_store();
    for item in [dir, ROOT] {
        assert!(is_refused(items.apply(&LocalVersionChange::OpenedForWriting { item })), "{item}");
        assert!(is_refused(items.apply(&LocalVersionChange::ClosedChanged { item, size: 1, mtime: 1, mtime_nsec: 0 })), "{item}");
        assert!(is_refused(items.apply(&LocalVersionChange::ClosedUnchanged { item })), "{item}");
        assert!(is_refused(items.apply(&LocalVersionChange::Truncated { item, size: 1, mtime: 1, mtime_nsec: 0 })), "{item}");
    }
}

/// A program that holds a file open goes on after the file was removed or saved over: its
/// open, its truncation and its close then name an item the folder does not show any more,
/// or whose row went. Each is nothing, and no error: no row is changed.
#[test]
fn a_change_of_the_content_of_a_file_that_is_gone_is_nothing() {
    let mut store = TreeStore::in_memory().unwrap();
    let removed = make_file(&mut store, ROOT, "removed");
    synced(&mut store, removed, "G");
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: removed }).unwrap();
    store.item_store().apply(&LocalVersionChange::Removed { item: removed }).unwrap();
    let before = item(&mut store, removed);
    let no_row = LocalItemId::new(99);
    for id in [removed, no_row] {
        let mut items = store.item_store();
        assert_eq!(items.apply(&LocalVersionChange::OpenedForWriting { item: id }).unwrap(), id);
        assert_eq!(items.apply(&LocalVersionChange::Truncated { item: id, size: 1, mtime: 1, mtime_nsec: 0 }).unwrap(), id);
        assert_eq!(items.apply(&LocalVersionChange::ClosedChanged { item: id, size: 1, mtime: 1, mtime_nsec: 0 }).unwrap(), id);
        assert_eq!(items.apply(&LocalVersionChange::ClosedUnchanged { item: id }).unwrap(), id);
    }
    assert_eq!(item(&mut store, removed), before, "the removed row is as the removal left it");
    assert_eq!(store.item_store().find_item(no_row).unwrap(), None);
}

/// What one row can say by itself is the schema's to hold, whoever writes it: a row is not
/// its own parent; a place is a parent and a name; only the root is shown without a place;
/// a file has a local content and a directory none; only a file the folder shows has
/// content on disk; a binding is a drive and an item id; and the root is a directory.
#[test]
fn the_schema_refuses_a_row_that_contradicts_itself() {
    let store = TreeStore::in_memory().unwrap();
    store.bench_sql("INSERT INTO items (local_item_id, local_parent_id, local_name, local_kind) VALUES (5, 1, 'dir', 'directory')").unwrap();
    for (what, sql) in [
        ("its own parent", "UPDATE items SET local_parent_id = 5 WHERE local_item_id = 5"),
        ("its own synced parent", "UPDATE items SET synced_parent_id = 5, synced_name = 'dir' WHERE local_item_id = 5"),
        ("a parent and no name", "UPDATE items SET local_name = NULL WHERE local_item_id = 5"),
        ("a synced name and no parent", "UPDATE items SET synced_name = 'dir' WHERE local_item_id = 5"),
        ("shown with no place", "UPDATE items SET local_parent_id = NULL, local_name = NULL WHERE local_item_id = 5"),
        ("a place and not shown", "UPDATE items SET local_kind = NULL WHERE local_item_id = 5"),
        ("a directory with a content", "UPDATE items SET local_size = 1 WHERE local_item_id = 5"),
        ("a file with no content", "UPDATE items SET local_kind = 'file' WHERE local_item_id = 5"),
        ("a directory on disk", "UPDATE items SET content_presence = 'on-disk' WHERE local_item_id = 5"),
        ("a directory with a content before a write", "UPDATE items SET before_write_size = 1 WHERE local_item_id = 5"),
        ("an item id and no drive", "UPDATE items SET onedrive_item_id = 'A' WHERE local_item_id = 5"),
        ("a drive and no item id", "UPDATE items SET onedrive_drive_id = 'd1' WHERE local_item_id = 5"),
        ("the root with a place", "UPDATE items SET local_parent_id = 5, local_name = 'root' WHERE local_item_id = 1"),
        ("the root as a file", "UPDATE items SET local_kind = 'file', local_size = 0 WHERE local_item_id = 1"),
        ("the root not shown", "UPDATE items SET local_kind = NULL WHERE local_item_id = 1"),
    ] {
        assert!(is_refused_by_schema(store.bench_sql(sql)), "{what}");
    }
}

fn onedrive_root(id: &str) -> Change {
    Change::Root(Row {
        id: id.into(),
        parent_id: None,
        name: String::new(),
        kind: Kind::Folder,
        size: 0,
        mtime: 0,
        etag: None,
        ctag: None,
        quickxor: None,
        mime: None,
        placement: Placement::Placed,
    })
}

/// The root gets its binding where the store learns the drive's root: once it knows the
/// drive and the root's item id, whichever comes first, and from a page of a first listing
/// as from a staged tree.
#[test]
fn the_root_is_bound_once_the_drive_and_its_root_are_known() {
    let root_of = |drive: &str, id: &str| OneDriveItemReference { drive_id: DriveId::new(drive).unwrap(), item_id: OneDriveItemId::new(id) };

    let mut store = TreeStore::in_memory().unwrap();
    store.set_drive_id("DRIVE").unwrap();
    assert_eq!(item(&mut store, ROOT).onedrive_item, None, "the drive alone names no item");
    store.begin_staging(crate::NewTree::Whole).unwrap();
    store.stage(&[onedrive_root("R")]).unwrap();
    assert_eq!(item(&mut store, ROOT).onedrive_item, Some(root_of("drive", "R")), "the drive's id as a drive id is made");
    assert_eq!(store.item_store().find_bound_item(&root_of("Drive", "R")).unwrap().map(|i| i.local_item_id), Some(ROOT));

    let mut store = TreeStore::in_memory().unwrap();
    store.commit_page(&[onedrive_root("R")], "next").unwrap();
    assert_eq!(item(&mut store, ROOT).onedrive_item, None, "the root's id alone names no drive");
    store.set_drive_id("d9").unwrap();
    assert_eq!(item(&mut store, ROOT).onedrive_item, Some(root_of("d9", "R")));
}

/// A change of a local version and the outbox's rows of the same operation are written in
/// one transaction: both are there once it is committed, and neither when it is not.
#[test]
fn a_change_and_its_outbox_rows_are_written_in_one_transaction() {
    use crate::outbox::{apply_outbox_ops, Detection, OutboxKind, OutboxOp, OutboxState};
    let mut store = TreeStore::in_memory().unwrap();
    let record = |local_item_id| {
        OutboxOp::Record(Detection {
            kind: OutboxKind::Create,
            item_id: None,
            object: None,
            rel: "new".into(),
            base: None,
            target_parent: None,
            target_name: Some("new".into()),
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
            size: Some(0),
            local_item_id: Some(local_item_id),
        })
    };
    for committed in [false, true] {
        let tx = store.conn.transaction().unwrap();
        let new = apply_change(&tx, &made(place(ROOT, "new"), ItemKind::File)).unwrap();
        let applied = apply_outbox_ops(&tx, &[record(new)], 0).unwrap();
        assert_eq!(applied.queued.len(), 1);
        if committed {
            tx.commit().unwrap();
        } else {
            drop(tx);
        }
        let rows = store.outbox_rows().unwrap();
        assert_eq!((names(&mut store, ROOT).len(), rows.len()), if committed { (1, 1) } else { (0, 0) });
        if committed {
            assert_eq!(rows[0].local_item_id, Some(new), "the row names its local item");
            assert_eq!(item(&mut store, new).local_item_id, new);
        }
    }
}
