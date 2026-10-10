use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use super::item_row::write_item;
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
    LocalVersionChange::Made { kind, place, content, is_name_ignored: false, replaced: None }
}

/// A file made at `place` that the write operation says replaces the removed item `replaced`.
fn made_replacing(place: ItemPlace, replaced: LocalItemId) -> LocalVersionChange {
    LocalVersionChange::Made { kind: ItemKind::File, place, content: Some(made_here()), is_name_ignored: false, replaced: Some(replaced) }
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
    if row.kind == ItemKind::File {
        local.content = Some(hashed(onedrive_id));
    }
    row.synced_place = local.place.clone();
    row.synced_content = local.content.clone();
    row.onedrive_item = Some(reference(onedrive_id));
    write_item(&store.conn, &row).unwrap();
    item(store, id)
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
            kind: ItemKind::Directory,
            local: Some(ItemVersion { place: None, content: None }),
            onedrive: None,
            onedrive_etag: None,
            synced_place: None,
            synced_content: None,
            content_presence: ContentPresence::NotOnDisk,
            content_before_write: None,
            placement: crate::model::Placement::Placed,
            is_name_ignored: false,
            confirmed_at_count: 0,
            unsent_since: None,
        }
    );
    assert_eq!(path(&mut store, ROOT), Some(PathBuf::new()));
    assert_eq!(make_file(&mut store, ROOT, "first").as_i64(), 2, "the first item made is the one after the root");
}

/// The root stays what it is: it is not removed, not moved and not saved over. Nothing is
/// made without a place: a made item's place is not optional.
#[test]
fn the_root_is_not_removed_or_given_a_place() {
    let mut store = TreeStore::in_memory().unwrap();
    let dir = make_directory(&mut store, ROOT, "dir");
    let mut items = store.item_store();
    assert!(is_refused(items.apply(&LocalVersionChange::Removed { item: ROOT })));
    assert!(is_refused(items.apply(&LocalVersionChange::Moved { item: ROOT, place: place(dir, "root"), is_name_ignored: false })));
    assert!(is_refused(items.apply(&LocalVersionChange::SavedOver { replacing: dir, replaced: ROOT })));
    assert!(is_refused(items.exchange_local_places(ROOT, dir)));
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
    assert_eq!((found.kind, found.local.clone()), (ItemKind::File, Some(ItemVersion { place: Some(place(dir, "File.txt")), content: Some(made_here()) })));
    assert_eq!((found.onedrive_item.clone(), found.content_presence, found.is_name_ignored), (None, ContentPresence::OnDisk, false), "a file made here is on disk");
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
        assert!(is_refused(items.apply(&LocalVersionChange::Moved { item: moving, place: place(parent, "moved"), is_name_ignored: false })), "moved into {parent}");
    }
    assert_eq!(path(&mut store, moving), Some(PathBuf::from("moving")), "nothing was changed");
    assert_eq!(names(&mut store, ROOT), ["file", "moving"]);
}

/// A file has a content and a directory has none: a version that says otherwise is refused.
#[test]
fn a_file_is_made_with_a_content_and_a_directory_without() {
    let mut store = TreeStore::in_memory().unwrap();
    let mut items = store.item_store();
    let version = |kind, content| LocalVersionChange::Made { kind, place: place(ROOT, "x"), content, is_name_ignored: false, replaced: None };
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
    assert!(is_refused(items.apply(&LocalVersionChange::Moved { item: b, place: place(ROOT, "a"), is_name_ignored: false })));
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
        assert!(is_refused(items.apply(&LocalVersionChange::Moved { item: top, place: place(parent, "top"), is_name_ignored: false })), "into {parent}");
    }
    items.apply(&LocalVersionChange::Moved { item: bottom, place: place(beside, "up"), is_name_ignored: false }).unwrap();
    items.apply(&LocalVersionChange::Moved { item: top, place: place(bottom, "down"), is_name_ignored: false }).unwrap();
    assert_eq!(path(&mut store, middle), Some(PathBuf::from("beside/up/down/middle")), "what was below it once is not below it now");
}

/// A move gives the item its place, and with it everything below its path. What the upload
/// rules say of the new name is written for the moved item alone: a renamed directory
/// changes no row below it.
#[test]
fn a_move_changes_the_path_of_all_below_and_the_flag_of_the_moved_item_alone() {
    let mut store = TreeStore::in_memory().unwrap();
    let dir = make_directory(&mut store, ROOT, "dir");
    let inner = make_directory(&mut store, dir, "inner");
    let file = make_file(&mut store, inner, "file");
    let beside = make_file(&mut store, ROOT, "beside");
    let moved = store.item_store().apply(&LocalVersionChange::Moved { item: dir, place: place(ROOT, ".cache"), is_name_ignored: true }).unwrap();
    assert_eq!(moved, dir, "the item keeps its id");
    assert_eq!(path(&mut store, file), Some(PathBuf::from(".cache/inner/file")));
    assert_eq!([dir, inner, file, beside].map(|id| item(&mut store, id).is_name_ignored), [true, false, false, false]);
    assert_eq!([dir, inner, file, beside].map(|id| store.item_store().is_ignored(id).unwrap()), [true, true, true, false], "all below a directory that is left here");
    store.item_store().apply(&LocalVersionChange::Moved { item: dir, place: place(ROOT, "dir"), is_name_ignored: false }).unwrap();
    assert_eq!([dir, inner, file].map(|id| store.item_store().is_ignored(id).unwrap()), [false, false, false]);
    let ignored = store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::File, place: place(ROOT, "x.tmp"), content: Some(made_here()), is_name_ignored: true, replaced: None }).unwrap();
    assert!(item(&mut store, ignored).is_name_ignored);
}

/// An item made with `is_name_ignored` as given.
fn make_named(store: &mut TreeStore, kind: ItemKind, parent: LocalItemId, name: &str, is_name_ignored: bool) -> LocalItemId {
    let content = (kind == ItemKind::File).then(made_here);
    store.item_store().apply(&LocalVersionChange::Made { kind, place: place(parent, name), content, is_name_ignored, replaced: None }).unwrap()
}

/// Whether an item is left here is derived, never written over a subtree: its own name is
/// ignored, or that of a directory above it that OneDrive does not have. A directory that
/// holds a lock file and an ignored directory is renamed: the lock file and what is in the
/// ignored directory are still left here, and the rest is not.
#[test]
fn a_renamed_directory_leaves_what_was_ignored_in_it_ignored() {
    let mut store = TreeStore::in_memory().unwrap();
    let proj = make_directory(&mut store, ROOT, "proj");
    let lock = make_named(&mut store, ItemKind::File, proj, ".~lock.x#", true);
    let cache = make_named(&mut store, ItemKind::Directory, proj, ".cache", true);
    let cached = make_file(&mut store, cache, "blob");
    let source = make_file(&mut store, proj, "main.rs");
    let all = [proj, lock, cache, cached, source];
    let before = all.map(|id| store.item_store().is_ignored(id).unwrap());
    assert_eq!(before, [false, true, true, true, false]);

    store.item_store().apply(&LocalVersionChange::Moved { item: proj, place: place(ROOT, "proj2"), is_name_ignored: false }).unwrap();
    assert_eq!(all.map(|id| store.item_store().is_ignored(id).unwrap()), before, "a new name of the directory changes nothing below it");
    assert_eq!(all.map(|id| item(&mut store, id).is_name_ignored), [false, true, true, false, false], "each row says what the rules say of its own name");

    // The directory itself gets an ignored name: all in it is left here, and no row below
    // it is written; with its former name back, each is as before.
    store.item_store().apply(&LocalVersionChange::Moved { item: proj, place: place(ROOT, ".proj"), is_name_ignored: true }).unwrap();
    assert_eq!(all.map(|id| store.item_store().is_ignored(id).unwrap()), [true; 5]);
    store.item_store().apply(&LocalVersionChange::Moved { item: proj, place: place(ROOT, "proj"), is_name_ignored: false }).unwrap();
    assert_eq!(all.map(|id| store.item_store().is_ignored(id).unwrap()), before);
}

/// A directory OneDrive has is OneDrive's whatever its name: what is in it is not left here
/// for the directory's name. When the directory gives its binding up with its name said to
/// be ignored, it is; and the ignored name of an item with no binding counts wherever it
/// is. An id with no row is refused.
#[test]
fn a_directory_of_onedrive_with_an_ignored_name_does_not_ignore_what_is_in_it() {
    let mut store = TreeStore::in_memory().unwrap();
    let cache = make_named(&mut store, ItemKind::Directory, ROOT, ".cache", true);
    let inside = make_file(&mut store, cache, "inside");
    let lock = make_named(&mut store, ItemKind::File, cache, ".~lock", true);
    synced(&mut store, cache, "C");
    assert_eq!([inside, lock].map(|id| store.item_store().is_ignored(id).unwrap()), [false, true]);
    store.item_store().give_up_binding(cache, true).unwrap();
    assert_eq!([inside, lock].map(|id| store.item_store().is_ignored(id).unwrap()), [true, true], "a directory OneDrive does not have any more");
    assert!(is_refused(store.item_store().is_ignored(LocalItemId::new(99))));
    assert!(!store.item_store().is_ignored(ROOT).unwrap());
}

/// An item OneDrive has is never left here, whatever the upload rules say of a name: a file
/// of OneDrive whose own name is ignored, a file of OneDrive below an ignored directory that
/// OneDrive does not have, and a file made with an ignored name where a file of OneDrive
/// was removed, which takes that file's binding over. The same file with no binding is.
#[test]
fn an_item_onedrive_has_is_never_left_here() {
    let mut store = TreeStore::in_memory().unwrap();
    let own_name = make_named(&mut store, ItemKind::File, ROOT, "x.tmp", true);
    let cache = make_named(&mut store, ItemKind::Directory, ROOT, ".cache", true);
    let below = make_file(&mut store, cache, "below");
    let removed = make_file(&mut store, ROOT, "y.tmp");
    synced(&mut store, removed, "Y");
    store.item_store().apply(&LocalVersionChange::Removed { item: removed }).unwrap();
    let made_ignored = LocalVersionChange::Made { kind: ItemKind::File, place: place(ROOT, "y.tmp"), content: Some(made_here()), is_name_ignored: true, replaced: Some(removed) };
    let replacing = store.item_store().apply(&made_ignored).unwrap();
    assert!(item(&mut store, replacing).is_name_ignored, "the row says what the rules say of the name");
    assert_eq!([own_name, below].map(|id| store.item_store().is_ignored(id).unwrap()), [true, true], "with no binding");

    synced(&mut store, own_name, "X");
    synced(&mut store, below, "B");
    assert_eq!([own_name, below, replacing].map(|id| store.item_store().is_ignored(id).unwrap()), [false, false, false]);
    assert!(store.item_store().is_ignored(cache).unwrap(), "the directory OneDrive does not have is left here still");
}

/// Two items exchange their places in one step, in one directory and across two, although
/// the index of local places is checked at each statement; nothing is left under another
/// name. Each takes, with the other's name, what the upload rules say of that name. A
/// directory is not exchanged with what is below it.
#[test]
fn two_items_exchange_their_places() {
    let mut store = TreeStore::in_memory().unwrap();
    let a = make_file(&mut store, ROOT, "a");
    let b = make_named(&mut store, ItemKind::Directory, ROOT, "b", true);
    let inside = make_file(&mut store, b, "inside");
    assert!(store.item_store().is_ignored(inside).unwrap());
    store.item_store().exchange_local_places(a, b).unwrap();
    assert_eq!(item(&mut store, a).local.unwrap().place, Some(place(ROOT, "b")));
    assert_eq!(item(&mut store, b).local.unwrap().place, Some(place(ROOT, "a")));
    assert_eq!(names(&mut store, ROOT), ["a", "b"]);
    assert_eq!(path(&mut store, inside), Some(PathBuf::from("a/inside")));
    assert_eq!([a, b, inside].map(|id| item(&mut store, id).is_name_ignored), [true, false, false], "each has the other's name, and what the rules say of that name");
    assert!(!store.item_store().is_ignored(inside).unwrap(), "the directory it is in has a name that is not ignored now");

    store.item_store().exchange_local_places(inside, a).unwrap();
    assert_eq!(path(&mut store, inside), Some(PathBuf::from("b")));
    assert_eq!(path(&mut store, a), Some(PathBuf::from("a/inside")));
    assert_eq!((names(&mut store, ROOT), names(&mut store, b)), (vec!["a".to_owned(), "b".to_owned()], vec!["inside".to_owned()]));

    let deep = make_directory(&mut store, b, "deep");
    let deeper = make_file(&mut store, deep, "deeper");
    let mut items = store.item_store();
    assert!(is_refused(items.exchange_local_places(b, deeper)), "it would be below itself");
    assert!(is_refused(items.exchange_local_places(deeper, b)));
    assert!(is_refused(items.exchange_local_places(a, a)));
    assert!(is_refused(items.exchange_local_places(a, LocalItemId::new(99))));
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
    let own = make_directory(&mut store, dir, "own");
    let dir_row = synced(&mut store, dir, "D");
    let before = synced(&mut store, file, "F");
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: file }).unwrap();
    assert!(is_refused(store.item_store().apply(&LocalVersionChange::Removed { item: dir })), "a directory that holds something");

    let opened_at = unsent_since(&mut store, file);
    assert_eq!(store.item_store().apply(&LocalVersionChange::Removed { item: file }).unwrap(), file);
    assert_eq!(
        item(&mut store, file),
        Item { local: None, content_presence: ContentPresence::NotOnDisk, content_before_write: None, unsent_since: opened_at, ..before },
        "the binding, the kind and the synced place and content stay"
    );
    assert_eq!(item(&mut store, file).kind, ItemKind::File, "a row the folder does not show says what it is");
    assert_eq!(path(&mut store, file), None, "an item the folder does not show has no path");
    store.item_store().apply(&LocalVersionChange::Removed { item: own }).unwrap();
    assert_eq!(store.item_store().find_item(own).unwrap(), None, "a directory OneDrive never had goes with its removal");
    assert_eq!(names(&mut store, dir), Vec::<String>::new());
    store.item_store().apply(&LocalVersionChange::Removed { item: dir }).unwrap();
    let removed_at = unsent_since(&mut store, dir);
    assert!(removed_at.is_some());
    assert_eq!(item(&mut store, dir), Item { local: None, unsent_since: removed_at, ..dir_row }, "the removed file's synced place still names a row");
    assert!(is_refused(store.item_store().apply(&LocalVersionChange::Removed { item: file })), "removed already");
    assert!(is_refused(store.item_store().apply(&LocalVersionChange::Removed { item: LocalItemId::new(99) })));
}

/// Rows that say the two sides agreed something is in a directory OneDrive does not have
/// (no operation leaves such) do not make a removal fail as SQLite's: the removal of that
/// directory is refused, and nothing changes.
#[test]
fn a_directory_with_no_binding_that_something_was_agreed_to_be_in_is_not_removed() {
    let mut store = TreeStore::in_memory().unwrap();
    let dir = make_directory(&mut store, ROOT, "dir");
    let file = make_file(&mut store, dir, "file");
    synced(&mut store, file, "F");
    store.item_store().apply(&LocalVersionChange::Removed { item: file }).unwrap();
    assert!(is_refused(store.item_store().apply(&LocalVersionChange::Removed { item: dir })));
    assert_eq!(names(&mut store, ROOT), ["dir"]);
}

/// A file made where a file was removed here whose removal is not sent yet takes over that
/// item's binding when the write operation says which item it replaces: OneDrive sees one
/// item whose content changed. The new file has an id of its own, and the removed row goes.
#[test]
fn a_file_made_as_replacing_a_removed_file_takes_over_its_binding() {
    let mut store = TreeStore::in_memory().unwrap();
    let old = make_file(&mut store, ROOT, "report");
    let old_row = synced(&mut store, old, "A");
    store.item_store().apply(&LocalVersionChange::Removed { item: old }).unwrap();

    let new = store.item_store().apply(&made_replacing(place(ROOT, "report"), old)).unwrap();
    assert_ne!(new, old, "the kernel was told the old file is gone");
    assert_eq!(store.item_store().find_item(old).unwrap(), None, "the removed row went");
    let new_row = item(&mut store, new);
    assert_eq!((new_row.onedrive_item.clone(), new_row.synced_place.clone(), new_row.synced_content.clone()), (old_row.onedrive_item, old_row.synced_place, old_row.synced_content));
    assert_eq!(new_row.local.as_ref().unwrap().content, Some(made_here()));
    assert_eq!(new_row.file_sync_state(), Some(FileSyncState::ModifiedHere));
    assert_eq!(store.item_store().find_bound_item(&reference("A")).unwrap().map(|i| i.local_item_id), Some(new));
}

/// Which item a made file replaces is the write operation's to say, and the store finds
/// none by itself. The cases in which the place alone would say otherwise:
///
/// - a file moved and then removed, and a file made where it was removed from: it replaces
///   that file when the operation says so, though OneDrive has the file at another place;
/// - a file made where OneDrive has a removed file, with nothing said: a new item;
/// - a removal that is being sent, which the operation does not name: a new item;
/// - two removed files that OneDrive has at one place: the one named, and only that one.
#[test]
fn a_made_file_replaces_the_item_the_operation_names_and_no_other() {
    let mut store = TreeStore::in_memory().unwrap();
    let moved = make_file(&mut store, ROOT, "a");
    synced(&mut store, moved, "M");
    store.item_store().apply(&LocalVersionChange::Moved { item: moved, place: place(ROOT, "b"), is_name_ignored: false }).unwrap();
    store.item_store().apply(&LocalVersionChange::Removed { item: moved }).unwrap();
    let being_sent = make_file(&mut store, ROOT, "sent");
    synced(&mut store, being_sent, "S");
    store.item_store().apply(&LocalVersionChange::Removed { item: being_sent }).unwrap();

    let at_synced_place = make_file(&mut store, ROOT, "a");
    assert_eq!(item(&mut store, at_synced_place).onedrive_item, None, "nothing said: a new item where OneDrive has the removed file");
    let at_sent = make_file(&mut store, ROOT, "sent");
    assert_eq!(item(&mut store, at_sent).onedrive_item, None, "nothing said: a new item where a removal is being sent");
    assert_eq!(store.item_store().find_bound_item(&reference("S")).unwrap().map(|i| i.local_item_id), Some(being_sent), "the removed row is as it was");
    let at_removed_place = store.item_store().apply(&made_replacing(place(ROOT, "b"), moved)).unwrap();
    let row = item(&mut store, at_removed_place);
    assert_eq!((row.onedrive_item, row.synced_place), (Some(reference("M")), Some(place(ROOT, "a"))), "said: the file removed from this place, which OneDrive has elsewhere");

    // Two removed files OneDrive has at one place: the synced place is not one item's.
    let dir = make_directory(&mut store, ROOT, "dir");
    let (first, second) = (make_file(&mut store, dir, "first"), make_file(&mut store, dir, "second"));
    synced(&mut store, first, "1");
    let mut second_row = synced(&mut store, second, "2");
    second_row.synced_place = Some(place(dir, "first"));
    write_item(&store.conn, &second_row).unwrap();
    for id in [first, second] {
        store.item_store().apply(&LocalVersionChange::Removed { item: id }).unwrap();
    }
    let new = store.item_store().apply(&made_replacing(place(dir, "first"), second)).unwrap();
    assert_eq!(item(&mut store, new).onedrive_item, Some(reference("2")), "the one named, though it is not the first of the two");
    assert_eq!(store.item_store().find_bound_item(&reference("1")).unwrap().map(|i| i.local_item_id), Some(first), "the other keeps its row");
}

/// What a made item is refused to replace, with nothing made: an item the folder still
/// shows, one OneDrive never had, a directory, the root, an id with no row; and a made
/// directory replaces nothing.
#[test]
fn a_made_file_replaces_only_a_bound_file_the_folder_does_not_show() {
    let mut store = TreeStore::in_memory().unwrap();
    let shown = make_file(&mut store, ROOT, "shown");
    let local = make_file(&mut store, ROOT, "local");
    let (removed_dir, removed_file) = (make_directory(&mut store, ROOT, "removed-dir"), make_file(&mut store, ROOT, "removed-file"));
    for (id, onedrive_id) in [(shown, "S"), (removed_dir, "D"), (removed_file, "F"), (ROOT, "R")] {
        synced(&mut store, id, onedrive_id);
    }
    for id in [removed_dir, removed_file] {
        store.item_store().apply(&LocalVersionChange::Removed { item: id }).unwrap();
    }
    let mut items = store.item_store();
    for replaced in [shown, local, removed_dir, ROOT, LocalItemId::new(99)] {
        assert!(is_refused(items.apply(&made_replacing(place(ROOT, "new"), replaced))), "replacing {replaced}");
    }
    let directory_back = LocalVersionChange::Made { kind: ItemKind::Directory, place: place(ROOT, "new"), content: None, is_name_ignored: false, replaced: Some(removed_dir) };
    assert!(is_refused(items.apply(&directory_back)), "a directory takes no binding over");
    let directory_over_file = LocalVersionChange::Made { kind: ItemKind::Directory, place: place(ROOT, "new"), content: None, is_name_ignored: false, replaced: Some(removed_file) };
    assert!(is_refused(items.apply(&directory_over_file)));
    assert_eq!(names(&mut store, ROOT), ["local", "shown"], "nothing was made");
    assert_eq!(store.item_store().find_bound_item(&reference("F")).unwrap().map(|i| i.local_item_id), Some(removed_file));
}

/// A file saved over another (an editor's temporary file renamed over the original) has its
/// place and takes over its binding; the replaced row goes.
#[test]
fn a_file_saved_over_a_bound_file_takes_over_its_binding() {
    let mut store = TreeStore::in_memory().unwrap();
    let original = make_file(&mut store, ROOT, "report");
    let original_row = synced(&mut store, original, "A");
    let temporary = make_named(&mut store, ItemKind::File, ROOT, ".report.tmp", true);
    let saved = store.item_store().apply(&LocalVersionChange::SavedOver { replacing: temporary, replaced: original }).unwrap();
    assert_eq!(saved, temporary, "the id follows the file the kernel holds");
    assert_eq!(store.item_store().find_item(original).unwrap(), None);
    let row = item(&mut store, temporary);
    assert_eq!(row.local.as_ref().unwrap().place, Some(place(ROOT, "report")));
    assert_eq!((row.onedrive_item.clone(), row.synced_place.clone(), row.synced_content.clone()), (original_row.onedrive_item, original_row.synced_place, original_row.synced_content));
    assert_eq!(row.file_sync_state(), Some(FileSyncState::ModifiedHere));
    assert!(!row.is_name_ignored, "its name is the replaced file's now, and so is what the rules say of it");
    assert_eq!(names(&mut store, ROOT), ["report"]);
}

/// An item renamed over one OneDrive never had: that row goes, whatever the replacing item
/// is. The replacing item has the replaced one's name, and what the upload rules say of
/// that name; what is below a directory is left here or not by the name the directory has
/// now.
#[test]
fn an_item_renamed_over_one_that_is_not_in_onedrive_takes_its_place_and_its_row_goes() {
    let mut store = TreeStore::in_memory().unwrap();
    let new = make_file(&mut store, ROOT, "new");
    let never_sent = make_named(&mut store, ItemKind::File, ROOT, "never-sent", true);
    store.item_store().apply(&LocalVersionChange::SavedOver { replacing: new, replaced: never_sent }).unwrap();
    assert_eq!(store.item_store().find_item(never_sent).unwrap(), None);
    let row = item(&mut store, new);
    assert_eq!((row.local.unwrap().place, row.onedrive_item, row.is_name_ignored), (Some(place(ROOT, "never-sent")), None, true));

    let bound = make_file(&mut store, ROOT, "bound");
    synced(&mut store, bound, "B");
    let local = make_file(&mut store, ROOT, "local");
    store.item_store().apply(&LocalVersionChange::SavedOver { replacing: bound, replaced: local }).unwrap();
    assert_eq!(store.item_store().find_item(local).unwrap(), None);
    let row = item(&mut store, bound);
    assert_eq!((row.local.unwrap().place, row.onedrive_item, row.synced_place), (Some(place(ROOT, "local")), Some(reference("B")), Some(place(ROOT, "bound"))));

    let (empty, full) = (make_directory(&mut store, ROOT, "empty"), make_named(&mut store, ItemKind::Directory, ROOT, ".full", true));
    let inside = make_file(&mut store, full, "inside");
    assert!(store.item_store().is_ignored(inside).unwrap());
    store.item_store().apply(&LocalVersionChange::SavedOver { replacing: full, replaced: empty }).unwrap();
    assert_eq!(path(&mut store, inside), Some(PathBuf::from("empty/inside")));
    assert!(!store.item_store().is_ignored(inside).unwrap(), "the directory it is in has the replaced one's name");
    assert_eq!(store.item_store().find_item(empty).unwrap(), None);
}

/// A file of OneDrive renamed over another file of OneDrive, as today's rename records it
/// (the one is moved in OneDrive, the other removed there): each keeps its binding. The
/// replacing file is at the new place with its synced place where OneDrive still has it;
/// the replaced one is a row the folder does not show, with what was agreed on. The same
/// for two directories of OneDrive.
#[test]
fn a_file_of_onedrive_renamed_over_another_file_of_onedrive_is_moved_and_the_other_is_removed_here() {
    let mut store = TreeStore::in_memory().unwrap();
    let (a, b) = (make_file(&mut store, ROOT, "a"), make_file(&mut store, ROOT, "b"));
    synced(&mut store, a, "A");
    let b_row = synced(&mut store, b, "B");
    store.item_store().apply(&LocalVersionChange::SavedOver { replacing: a, replaced: b }).unwrap();
    let a_row = item(&mut store, a);
    assert_eq!((a_row.onedrive_item.clone(), a_row.local.clone().unwrap().place, a_row.synced_place.clone()), (Some(reference("A")), Some(place(ROOT, "b")), Some(place(ROOT, "a"))));
    assert_eq!(a_row.file_sync_state(), Some(FileSyncState::Synced), "moved, and its content is the one agreed on");
    let removed_at = unsent_since(&mut store, b);
    assert!(removed_at.is_some());
    assert_eq!(item(&mut store, b), Item { local: None, content_presence: ContentPresence::NotOnDisk, unsent_since: removed_at, ..b_row }, "removed here, to be removed in OneDrive");
    assert_eq!(names(&mut store, ROOT), ["b"]);

    let (c, d) = (make_directory(&mut store, ROOT, "c"), make_directory(&mut store, ROOT, "d"));
    let inside = make_file(&mut store, c, "inside");
    synced(&mut store, c, "C");
    let d_row = synced(&mut store, d, "D");
    store.item_store().apply(&LocalVersionChange::SavedOver { replacing: c, replaced: d }).unwrap();
    assert_eq!(item(&mut store, c).onedrive_item, Some(reference("C")));
    let removed_at = unsent_since(&mut store, d);
    assert!(removed_at.is_some());
    assert_eq!(item(&mut store, d), Item { local: None, unsent_since: removed_at, ..d_row });
    assert_eq!(path(&mut store, inside), Some(PathBuf::from("d/inside")));
}

/// A directory OneDrive does not have, renamed over an empty directory it has: a directory
/// takes no binding over. The replacing one is a new directory at that name, and the
/// replaced one is removed here with its binding kept, to be removed in OneDrive.
#[test]
fn a_directory_that_is_not_in_onedrive_renamed_over_one_that_is_takes_no_binding_over() {
    let mut store = TreeStore::in_memory().unwrap();
    let (new, old) = (make_directory(&mut store, ROOT, "new"), make_directory(&mut store, ROOT, "old"));
    let old_row = synced(&mut store, old, "D");
    store.item_store().apply(&LocalVersionChange::SavedOver { replacing: new, replaced: old }).unwrap();
    let row = item(&mut store, new);
    assert_eq!((row.onedrive_item, row.synced_place, row.local.unwrap().place), (None, None, Some(place(ROOT, "old"))));
    let removed_at = unsent_since(&mut store, old);
    assert!(removed_at.is_some());
    assert_eq!(item(&mut store, old), Item { local: None, unsent_since: removed_at, ..old_row });
}

/// A file saved over a file a program holds open for writing: the replaced file's content
/// before its writer goes with its row, the replacing file keeps its own, and the writer's
/// close of the replaced file is nothing.
#[test]
fn a_file_saved_over_a_file_open_for_writing_takes_over_its_binding() {
    let mut store = TreeStore::in_memory().unwrap();
    let original = make_file(&mut store, ROOT, "report");
    synced(&mut store, original, "A");
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: original }).unwrap();
    let temporary = make_file(&mut store, ROOT, "report.tmp");
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: temporary }).unwrap();
    let before = item(&mut store, temporary);

    store.item_store().apply(&LocalVersionChange::SavedOver { replacing: temporary, replaced: original }).unwrap();
    let saved = item(&mut store, temporary);
    assert_eq!(saved, Item { onedrive_item: Some(reference("A")), synced_place: Some(place(ROOT, "report")), synced_content: Some(hashed("A")), local: Some(ItemVersion { place: Some(place(ROOT, "report")), ..before.local.clone().unwrap() }), ..before });
    assert_eq!(saved.content_before_write, Some(made_here()), "what the replacing file had before its own writer");
    assert_eq!(store.item_store().apply(&LocalVersionChange::ClosedChanged { item: original, size: 5, mtime: 9, mtime_nsec: 0 }).unwrap(), original);
    assert_eq!(store.item_store().find_item(original).unwrap(), None);
    assert_eq!(item(&mut store, temporary), saved, "the close of the replaced file changed no row");
}

/// What is refused of one item renamed over another: over itself, a file over a directory
/// or the other way, over a directory that holds something, a directory over an empty
/// directory below itself, and an item the folder does not show on either side.
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
    let below = make_directory(&mut store, dir, "below");
    let mut items = store.item_store();
    for (replacing, replaced) in [(file, file), (file, dir), (dir, file), (dir, full), (dir, below), (file, gone), (gone, file), (file, LocalItemId::new(99))] {
        assert!(is_refused(items.apply(&LocalVersionChange::SavedOver { replacing, replaced })), "{replacing} over {replaced}");
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

/// What one row can say by itself is the schema's to hold, whoever writes it: an item is a
/// file or a directory; a row is not its own parent; a place is a parent and a name; a file
/// the folder shows has a local content, and nothing else has one; a directory has no
/// content on any side, none on disk and none before a write; an item the folder does not
/// show has none on disk and none before a write; a content is on disk or not on disk; a
/// binding is a drive and an item id, neither the empty text; nothing is agreed on, a
/// place or a content, for an item with no binding; and the root is a directory with no
/// place.
#[test]
fn the_schema_refuses_a_row_that_contradicts_itself() {
    let store = TreeStore::in_memory().unwrap();
    store
        .bench_sql(
            "INSERT INTO items (local_item_id, kind, local_parent_id, local_name) VALUES (5, 'directory', 1, 'dir');
             INSERT INTO items (local_item_id, kind, local_parent_id, local_name, local_size) VALUES (6, 'file', 1, 'file', 0);
             INSERT INTO items (local_item_id, kind, onedrive_drive_id, onedrive_item_id, synced_parent_id, synced_name, synced_size) VALUES (7, 'file', 'd1', 'G', 1, 'gone', 3);
             INSERT INTO items (local_item_id, kind, onedrive_drive_id, onedrive_item_id, local_parent_id, local_name) VALUES (8, 'directory', 'd1', 'D', 1, 'sent-dir');",
        )
        .unwrap();
    for (what, sql) in [
        ("no kind", "INSERT INTO items (local_parent_id, local_name) VALUES (1, 'x')"),
        ("a kind that is neither", "UPDATE items SET kind = 'folder' WHERE local_item_id = 5"),
        ("its own parent", "UPDATE items SET local_parent_id = 5 WHERE local_item_id = 5"),
        ("its own synced parent", "UPDATE items SET synced_parent_id = 8, synced_name = 'dir' WHERE local_item_id = 8"),
        ("a parent and no name", "UPDATE items SET local_name = NULL WHERE local_item_id = 5"),
        ("a synced name and no parent", "UPDATE items SET synced_name = 'dir' WHERE local_item_id = 5"),
        ("a directory with a local content", "UPDATE items SET local_size = 1 WHERE local_item_id = 5"),
        ("a directory with a synced content", "UPDATE items SET synced_size = 1 WHERE local_item_id = 8"),
        ("a directory on disk", "UPDATE items SET content_presence = 'on-disk' WHERE local_item_id = 5"),
        ("a directory with a content before a write", "UPDATE items SET before_write_size = 1 WHERE local_item_id = 5"),
        ("a shown file made a directory", "UPDATE items SET kind = 'directory' WHERE local_item_id = 6"),
        ("a shown file with no content", "UPDATE items SET local_size = NULL WHERE local_item_id = 6"),
        ("a file not shown with a local content", "UPDATE items SET local_size = 1 WHERE local_item_id = 7"),
        ("a file not shown on disk", "UPDATE items SET content_presence = 'on-disk' WHERE local_item_id = 7"),
        ("a file not shown with a content before a write", "UPDATE items SET before_write_size = 1 WHERE local_item_id = 7"),
        ("a removed file made a directory", "UPDATE items SET kind = 'directory' WHERE local_item_id = 7"),
        ("an item id and no drive", "UPDATE items SET onedrive_item_id = 'A' WHERE local_item_id = 5"),
        ("a drive and no item id", "UPDATE items SET onedrive_drive_id = 'd1' WHERE local_item_id = 5"),
        ("an item id that is the empty text", "UPDATE items SET onedrive_drive_id = 'd1', onedrive_item_id = '' WHERE local_item_id = 5"),
        ("a place agreed on with no binding", "UPDATE items SET synced_parent_id = 1, synced_name = 'file' WHERE local_item_id = 6"),
        ("a content agreed on with no binding", "UPDATE items SET synced_size = 0 WHERE local_item_id = 6"),
        ("a binding gone from a row with something agreed on", "UPDATE items SET onedrive_drive_id = NULL, onedrive_item_id = NULL WHERE local_item_id = 7"),
        ("a presence that is neither word", "UPDATE items SET content_presence = 'elsewhere' WHERE local_item_id = 6"),
        ("the root with a place", "UPDATE items SET local_parent_id = 5, local_name = 'root' WHERE local_item_id = 1"),
        ("the root with a synced place", "UPDATE items SET synced_parent_id = 5, synced_name = 'root' WHERE local_item_id = 1"),
        ("the root as a file", "UPDATE items SET kind = 'file' WHERE local_item_id = 1"),
    ] {
        assert!(is_refused_by_schema(store.bench_sql(sql)), "{what}");
    }
}

/// A name that is not UTF-8 is kept as its bytes through a move, an exchange and a rename
/// over another item, and is found by them again.
#[test]
fn a_name_that_is_not_utf8_is_kept_through_a_move_an_exchange_and_a_rename_over() {
    let mut store = TreeStore::in_memory().unwrap();
    let odd = |last: u8| OsString::from_vec(vec![b'o', 0xff, last]);
    let at = |name: OsString| ItemPlace { parent_local_item_id: ROOT, name };
    let file = make_file(&mut store, ROOT, "file");
    let other = store.item_store().apply(&made(at(odd(b'1')), ItemKind::File)).unwrap();
    store.item_store().apply(&LocalVersionChange::Moved { item: file, place: at(odd(b'2')), is_name_ignored: false }).unwrap();
    assert_eq!(store.item_store().find_item_at(ROOT, &odd(b'2')).unwrap().map(|i| i.local_item_id), Some(file));
    store.item_store().exchange_local_places(file, other).unwrap();
    assert_eq!([odd(b'1'), odd(b'2')].map(|name| store.item_store().find_item_at(ROOT, &name).unwrap().map(|i| i.local_item_id)), [Some(file), Some(other)]);
    assert_eq!(path(&mut store, other), Some(PathBuf::from(odd(b'2'))));
    store.item_store().apply(&LocalVersionChange::SavedOver { replacing: file, replaced: other }).unwrap();
    assert_eq!(item(&mut store, file).local.unwrap().place, Some(at(odd(b'2'))));
    assert_eq!(store.item_store().list_children(ROOT).unwrap().len(), 1);
}

/// A chain of `count` directories named `name`, the first in the root and each other in
/// the one before: their ids.
fn make_chain(store: &mut TreeStore, name: &str, count: usize) -> Vec<LocalItemId> {
    let mut chain = Vec::new();
    for _ in 0..count {
        chain.push(make_directory(store, *chain.last().unwrap_or(&ROOT), name));
    }
    chain
}

fn refusal<T: std::fmt::Debug>(result: Result<T, TreeError>) -> String {
    match result {
        Err(TreeError::Refused(why)) => why,
        other => panic!("not refused: {other:?}"),
    }
}

/// The folder is as deep as the walks of the rows go, and no deeper: an item made below the
/// last directory that may hold one is refused, and so is a move that would put the item,
/// or anything below it, deeper. Each says that the place is too deep.
#[test]
fn an_item_is_not_made_or_moved_deeper_than_the_rows_are_walked() {
    let mut store = TreeStore::in_memory().unwrap();
    let chain = make_chain(&mut store, "d", crate::MAX_CHAIN - 1);
    let deepest = *chain.last().unwrap();
    let file = make_file(&mut store, deepest, "file");
    assert_eq!(path(&mut store, file).unwrap().components().count(), crate::MAX_CHAIN, "the deepest place has a path");
    assert!(!store.item_store().is_ignored(file).unwrap());
    store.item_store().apply(&LocalVersionChange::Removed { item: file }).unwrap();
    let last = make_directory(&mut store, deepest, "last");
    assert!(refusal(store.item_store().apply(&made(place(last, "too-deep"), ItemKind::File))).contains("deeper"));

    let pair = make_chain(&mut store, "p", 2);
    let single = make_directory(&mut store, ROOT, "single");
    let moved = |item, parent| LocalVersionChange::Moved { item, place: place(parent, "moved"), is_name_ignored: false };
    assert!(refusal(store.item_store().apply(&moved(single, last))).contains("deeper"), "the moved item itself");
    assert!(refusal(store.item_store().apply(&moved(pair[0], deepest))).contains("deeper"), "what is below the moved item");
    store.item_store().apply(&moved(single, deepest)).unwrap();

    // Renamed over an empty directory, and exchanged: what is below comes along.
    let empty = make_directory(&mut store, deepest, "empty");
    assert!(refusal(store.item_store().apply(&LocalVersionChange::SavedOver { replacing: pair[0], replaced: empty })).contains("deeper"));
    assert!(refusal(store.item_store().exchange_local_places(pair[0], empty)).contains("deeper"));
    assert_eq!(path(&mut store, pair[1]), Some(PathBuf::from("p/p")), "nothing was changed");
}

/// The path of an item: none for an id with no row and for an item the folder does not
/// show, which `find_item` tells apart; and refused, not none, for rows whose directories
/// do not end at the root within the bound, which no operation leaves.
#[test]
fn a_path_is_none_for_an_item_that_is_not_shown_and_refused_for_rows_that_are_too_deep() {
    let mut store = TreeStore::in_memory().unwrap();
    let gone = make_file(&mut store, ROOT, "gone");
    synced(&mut store, gone, "G");
    store.item_store().apply(&LocalVersionChange::Removed { item: gone }).unwrap();
    assert_eq!(path(&mut store, gone), None);
    assert!(store.item_store().find_item(gone).unwrap().is_some(), "it has a row");
    assert_eq!(path(&mut store, LocalItemId::new(999)), None);
    assert_eq!(store.item_store().find_item(LocalItemId::new(999)).unwrap(), None);

    let chain = make_chain(&mut store, "d", crate::MAX_CHAIN);
    let deepest = *chain.last().unwrap();
    store.bench_sql(&format!("INSERT INTO items (local_item_id, kind, local_parent_id, local_name) VALUES (5000, 'directory', {deepest}, 'below')")).unwrap();
    assert!(refusal(store.item_store().find_path(LocalItemId::new(5000))).contains("do not end at the root"));
    assert!(refusal(store.item_store().is_ignored(LocalItemId::new(5000))).contains("do not end at the root"));
}

/// A name is checked where it enters the rows: the empty name, `.`, `..`, a name with a
/// `/` and a name with a NUL are no names of a directory entry, and a path built from them
/// would be another item's or outside the folder. Refused when an item is made, when it is
/// moved, and when a row is written with such a name as its synced one.
#[test]
fn a_name_that_is_no_name_of_a_directory_entry_is_refused() {
    let mut store = TreeStore::in_memory().unwrap();
    let dir = make_directory(&mut store, ROOT, "dir");
    let file = make_file(&mut store, dir, "file");
    let row = synced(&mut store, file, "F");
    for name in ["", ".", "..", "a/b", "/x", "a\0b"] {
        let mut items = store.item_store();
        assert!(is_refused(items.apply(&made(place(dir, name), ItemKind::File))), "made as {name:?}");
        assert!(is_refused(items.apply(&made(place(dir, name), ItemKind::Directory))), "a directory made as {name:?}");
        assert!(is_refused(items.apply(&LocalVersionChange::Moved { item: file, place: place(dir, name), is_name_ignored: false })), "moved to {name:?}");
        assert!(is_refused(write_item(&store.conn, &Item { synced_place: Some(place(dir, name)), ..row.clone() })), "agreed on as {name:?}");
    }
    assert_eq!(names(&mut store, dir), ["file"]);
    assert_eq!(path(&mut store, file), Some(PathBuf::from("dir/file")));
    for name in ["...", ".hidden", "a\\b", " "] {
        store.item_store().apply(&made(place(dir, name), ItemKind::File)).unwrap();
    }
}

/// A drive id is never the empty text, in a row as in a `DriveId`: a row that says so would
/// read as one with no binding while the index of bindings counts it as one.
#[test]
fn the_schema_refuses_a_binding_with_an_empty_drive_id() {
    let store = TreeStore::in_memory().unwrap();
    assert!(is_refused_by_schema(store.bench_sql("INSERT INTO items (kind, onedrive_drive_id, onedrive_item_id) VALUES ('file', '', 'A')")));
    store.bench_sql("INSERT INTO items (kind, onedrive_drive_id, onedrive_item_id) VALUES ('file', 'd1', 'A')").unwrap();
}

/// A child's parent exists, whoever deletes a row: the schema refuses to delete a row that
/// another names as its parent in the folder, or as the directory the two sides agreed it
/// is in. With nothing in it on either side, the row goes; the root's never does.
#[test]
fn the_schema_refuses_to_delete_a_row_from_under_what_is_in_it() {
    let mut store = TreeStore::in_memory().unwrap();
    let local_parent = make_directory(&mut store, ROOT, "local-parent");
    let child = make_file(&mut store, local_parent, "child");
    let synced_parent = store.item_store().make_onedrive_item(&reference("P"), ItemKind::Directory).unwrap();
    let mut child_row = synced(&mut store, child, "C");
    child_row.synced_place = Some(place(synced_parent, "child"));
    write_item(&store.conn, &child_row).unwrap();

    let delete = |store: &TreeStore, id: LocalItemId| store.bench_sql(&format!("DELETE FROM items WHERE local_item_id = {id}"));
    assert!(is_refused_by_schema(delete(&store, local_parent)), "the folder shows something in it");
    assert!(is_refused_by_schema(delete(&store, synced_parent)), "the two sides agreed something is in it");
    assert!(is_refused_by_schema(store.bench_sql("DELETE FROM items WHERE local_item_id != 1")), "several rows at once, a parent among them");
    assert_eq!(names(&mut store, local_parent), ["child"]);
    delete(&store, child).unwrap();
    delete(&store, local_parent).unwrap();
    delete(&store, synced_parent).unwrap();
    assert_eq!(store.item_store().list_children(ROOT).unwrap(), Vec::new());
    assert!(is_refused_by_schema(delete(&store, ROOT)), "the root, with nothing in it");
    assert_eq!(item(&mut store, ROOT).kind, ItemKind::Directory);
}

/// An item's parent exists and is a directory, on both sides and whoever writes a row: the
/// schema refuses a row made with, or moved to, a local or a synced parent that has no row
/// or is a file. A directory's row is taken as either parent.
#[test]
fn the_schema_refuses_a_parent_that_has_no_row_or_is_a_file() {
    let store = TreeStore::in_memory().unwrap();
    store
        .bench_sql(
            "INSERT INTO items (local_item_id, kind, local_parent_id, local_name) VALUES (5, 'directory', 1, 'dir');
             INSERT INTO items (local_item_id, kind, local_parent_id, local_name, local_size) VALUES (6, 'file', 1, 'file', 0);
             INSERT INTO items (local_item_id, kind, onedrive_drive_id, onedrive_item_id, local_parent_id, local_name, local_size, synced_parent_id, synced_name, synced_size)
                 VALUES (7, 'file', 'd1', 'A', 1, 'sent', 3, 1, 'sent', 3);",
        )
        .unwrap();
    for (what, sql) in [
        ("made in a directory that has no row", "INSERT INTO items (kind, local_parent_id, local_name) VALUES ('directory', 99, 'x')"),
        ("made in a file", "INSERT INTO items (kind, local_parent_id, local_name) VALUES ('directory', 6, 'x')"),
        ("moved into a directory that has no row", "UPDATE items SET local_parent_id = 99 WHERE local_item_id = 7"),
        ("moved into a file", "UPDATE items SET local_parent_id = 6 WHERE local_item_id = 7"),
        ("made as agreed to be in a directory that has no row", "INSERT INTO items (kind, onedrive_drive_id, onedrive_item_id, synced_parent_id, synced_name) VALUES ('directory', 'd1', 'B', 99, 'x')"),
        ("made as agreed to be in a file", "INSERT INTO items (kind, onedrive_drive_id, onedrive_item_id, synced_parent_id, synced_name) VALUES ('directory', 'd1', 'B', 6, 'x')"),
        ("agreed to be in a directory that has no row", "UPDATE items SET synced_parent_id = 99 WHERE local_item_id = 7"),
        ("agreed to be in a file", "UPDATE items SET synced_parent_id = 6 WHERE local_item_id = 7"),
        ("several rows at once, into a file", "UPDATE items SET synced_parent_id = 6 WHERE synced_parent_id = 1"),
    ] {
        assert!(is_refused_by_schema(store.bench_sql(sql)), "{what}");
    }
    store
        .bench_sql(
            "UPDATE items SET local_parent_id = 5, synced_parent_id = 5 WHERE local_item_id = 7;
             INSERT INTO items (kind, onedrive_drive_id, onedrive_item_id, local_parent_id, local_name, synced_parent_id, synced_name) VALUES ('directory', 'd1', 'B', 5, 'x', 5, 'x');",
        )
        .unwrap();
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

/// The root's OneDrive item reference is the root's, whatever row was made first: a page
/// that names children before the root makes a row for their parent, and when the store
/// learns that this parent is the drive's root, that row goes and what was agreed to be in
/// it is in row 1.
#[test]
fn the_roots_reference_is_row_1s_also_when_another_row_had_it_first() {
    let mut store = TreeStore::in_memory().unwrap();
    store.set_drive_id("d1").unwrap();
    let mentioned = store.item_store().make_onedrive_item(&reference("R"), ItemKind::Directory).unwrap();
    let child = make_file(&mut store, ROOT, "child");
    let mut child_row = synced(&mut store, child, "C");
    child_row.synced_place = Some(place(mentioned, "child"));
    write_item(&store.conn, &child_row).unwrap();

    store.begin_staging(crate::NewTree::Whole).unwrap();
    store.stage(&[onedrive_root("R")]).unwrap();
    assert_eq!(item(&mut store, ROOT).onedrive_item, Some(reference("R")));
    assert_eq!(store.item_store().find_item(mentioned).unwrap(), None);
    assert_eq!(item(&mut store, child).synced_place, Some(place(ROOT, "child")));
    store.set_drive_id("D1").unwrap();
    assert_eq!(item(&mut store, ROOT).onedrive_item, Some(reference("R")), "learnt again, nothing changes");
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

/// The row count of `items`, as a transaction that is not committed sees it.
fn count_rows(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM items", [], |row| row.get(0)).unwrap()
}

/// What a made file may not replace is refused before the made file's row is written, so
/// that a caller's transaction holds nothing of the refused change: a file the folder still
/// shows, a file with no binding, a removed directory, and an id with no row.
#[test]
fn a_made_file_that_replaces_what_it_may_not_is_refused_before_its_row_is_written() {
    let mut store = TreeStore::in_memory().unwrap();
    let (shown, local) = (make_file(&mut store, ROOT, "shown"), make_file(&mut store, ROOT, "local"));
    let directory = make_directory(&mut store, ROOT, "directory");
    synced(&mut store, shown, "S");
    synced(&mut store, directory, "D");
    store.item_store().apply(&LocalVersionChange::Removed { item: directory }).unwrap();

    let tx = store.conn.transaction().unwrap();
    let rows = count_rows(&tx);
    for replaced in [shown, local, directory, LocalItemId::new(99)] {
        assert!(is_refused(apply_change(&tx, &made_replacing(place(ROOT, "new"), replaced))), "replacing {replaced}");
        assert_eq!(count_rows(&tx), rows, "replacing {replaced}: no row was made");
        assert_eq!(find_item_at(&tx, ROOT, OsStr::new("new")).unwrap(), None);
    }
}

/// An error inside a caller's transaction may come after a write, and what was written is
/// in the transaction: here an exchange that SQLite fails after its first step, which
/// leaves the first item under the name it stepped aside to. The caller drops the
/// transaction, and the rows are as they were.
#[test]
fn an_error_after_a_write_in_a_callers_transaction_is_undone_by_dropping_it() {
    let mut store = TreeStore::in_memory().unwrap();
    let (first, second) = (make_file(&mut store, ROOT, "first"), make_file(&mut store, ROOT, "second"));
    let before = [first, second].map(|id| item(&mut store, id));
    // The failure: the second item's place cannot be written.
    store.bench_sql(&format!("CREATE TEMP TRIGGER second_fails BEFORE UPDATE ON main.items WHEN OLD.local_item_id = {second} BEGIN SELECT RAISE(ABORT, 'a failure of the test'); END")).unwrap();

    let tx = store.conn.transaction().unwrap();
    let result = exchange_local_places(&tx, first, second);
    assert!(matches!(result, Err(TreeError::Sql(_))), "no refusal: {result:?}");
    assert_eq!(find_item_at(&tx, ROOT, OsStr::new("first")).unwrap(), None, "the first step was written");
    assert_ne!(find_item(&tx, first).unwrap().as_ref(), Some(&before[0]));
    drop(tx);
    assert_eq!([first, second].map(|id| item(&mut store, id)), before);
    assert_eq!(names(&mut store, ROOT), ["first", "second"]);
}

/// The store's call that learns the drive's root refuses when a row that is not the root
/// has that reference and is a file or an item the folder shows; its transaction is
/// dropped, so the store has learnt nothing and no row changed.
#[test]
fn the_root_is_not_bound_to_a_reference_that_a_shown_item_or_a_file_has() {
    for shown in [true, false] {
        let mut store = TreeStore::in_memory().unwrap();
        let holder = make_file(&mut store, ROOT, "holder");
        // The drive's id is put where `set_drive_id` would, without binding the root.
        store.set_meta(crate::meta::DRIVE_ID, Some("d1")).unwrap();
        synced(&mut store, holder, "R");
        if !shown {
            store.item_store().apply(&LocalVersionChange::Removed { item: holder }).unwrap();
        }
        let before = item(&mut store, holder);

        store.begin_staging(crate::NewTree::Whole).unwrap();
        assert!(is_refused(store.stage(&[onedrive_root("R")])), "shown: {shown}");
        assert!(is_refused(store.commit_page(&[onedrive_root("R")], "next")), "shown: {shown}");
        assert_eq!(store.root_item_id().unwrap(), None, "what the call wrote before it was refused is not kept");
        assert_eq!((item(&mut store, ROOT).onedrive_item, item(&mut store, holder)), (None, before));

        store.set_meta(crate::meta::ROOT_ITEM_ID, Some("R")).unwrap();
        assert!(is_refused(store.set_drive_id("d1")), "shown: {shown}");
        assert_eq!(item(&mut store, ROOT).onedrive_item, None);
    }
}

fn unsent_since(store: &mut TreeStore, id: LocalItemId) -> Option<u64> {
    item(store, id).unsent_since
}

fn apply(store: &mut TreeStore, change: LocalVersionChange) {
    store.item_store().apply(&change).unwrap();
}

/// An item made here is unsent from the moment it is made, a file and a directory alike,
/// each with the next stamp of one counter; what is done to it afterwards keeps the stamp.
/// An item whose name the upload rules ignore has it like any other. The root, which has no
/// place on either side, never has one.
#[test]
fn an_item_made_here_is_unsent_since_it_was_made() {
    let mut store = TreeStore::in_memory().unwrap();
    assert_eq!(store.item_store().last_unsent_since().unwrap(), 0);
    let file = make_file(&mut store, ROOT, "file");
    let dir = make_directory(&mut store, ROOT, "dir");
    let ignored = make_named(&mut store, ItemKind::File, ROOT, "ignored.tmp", true);
    assert_eq!((unsent_since(&mut store, file), unsent_since(&mut store, dir), unsent_since(&mut store, ignored)), (Some(1), Some(2), Some(3)));
    assert_eq!((unsent_since(&mut store, ROOT), store.item_store().last_unsent_since().unwrap()), (None, 3));

    apply(&mut store, LocalVersionChange::Moved { item: file, place: place(dir, "moved"), is_name_ignored: false });
    apply(&mut store, LocalVersionChange::OpenedForWriting { item: file });
    apply(&mut store, LocalVersionChange::ClosedChanged { item: file, size: 5, mtime: 300, mtime_nsec: 0 });
    apply(&mut store, LocalVersionChange::Truncated { item: file, size: 1, mtime: 301, mtime_nsec: 0 });
    store.item_store().bind_item(file, &reference("A")).unwrap();
    assert_eq!((unsent_since(&mut store, file), store.item_store().last_unsent_since().unwrap()), (Some(1), 3), "the first stamp, and no other given");
}

/// Each kind of change of the local version of an item the two sides agree on gives the
/// stamp once, a later change keeps the first stamp, and a change back to what was agreed
/// on clears it: a move and the move back; a writer's open and its close that changed
/// nothing; a close that changed the content, and a truncation; a removal.
#[test]
fn a_change_of_the_local_version_is_unsent_since_the_first_and_no_more_once_it_is_taken_back() {
    let mut store = TreeStore::in_memory().unwrap();
    let dir = make_directory(&mut store, ROOT, "dir");
    let file = make_file(&mut store, ROOT, "file");
    synced(&mut store, dir, "D");
    synced(&mut store, file, "A");
    assert_eq!((unsent_since(&mut store, dir), unsent_since(&mut store, file)), (None, None), "the two sides agree");
    let given = |store: &mut TreeStore| store.item_store().last_unsent_since().unwrap();
    let before = given(&mut store);

    apply(&mut store, LocalVersionChange::Moved { item: file, place: place(dir, "file"), is_name_ignored: false });
    assert_eq!(unsent_since(&mut store, file), Some(before + 1));
    apply(&mut store, LocalVersionChange::Moved { item: file, place: place(dir, "renamed"), is_name_ignored: false });
    apply(&mut store, LocalVersionChange::OpenedForWriting { item: file });
    assert_eq!(unsent_since(&mut store, file), Some(before + 1), "the first stamp is kept through later changes");
    apply(&mut store, LocalVersionChange::ClosedUnchanged { item: file });
    apply(&mut store, LocalVersionChange::Moved { item: file, place: place(ROOT, "file"), is_name_ignored: false });
    assert_eq!(unsent_since(&mut store, file), None, "moved back, and the content as it was");

    apply(&mut store, LocalVersionChange::OpenedForWriting { item: file });
    assert_eq!(unsent_since(&mut store, file), Some(before + 2), "not clean from the open on");
    apply(&mut store, LocalVersionChange::ClosedUnchanged { item: file });
    assert_eq!(unsent_since(&mut store, file), None);

    apply(&mut store, LocalVersionChange::OpenedForWriting { item: file });
    apply(&mut store, LocalVersionChange::ClosedChanged { item: file, size: 9, mtime: 300, mtime_nsec: 5 });
    apply(&mut store, LocalVersionChange::Truncated { item: file, size: 0, mtime: 301, mtime_nsec: 0 });
    assert_eq!(unsent_since(&mut store, file), Some(before + 3));

    apply(&mut store, LocalVersionChange::Moved { item: dir, place: place(ROOT, "renamed"), is_name_ignored: false });
    assert_eq!(unsent_since(&mut store, dir), Some(before + 4), "a directory has a place to differ in");
    apply(&mut store, LocalVersionChange::Removed { item: file });
    assert_eq!(unsent_since(&mut store, file), Some(before + 3), "removed here, with a change before it");
    let other = make_file(&mut store, ROOT, "other");
    synced(&mut store, other, "B");
    apply(&mut store, LocalVersionChange::Removed { item: other });
    assert_eq!(unsent_since(&mut store, other), Some(given(&mut store)), "removed here: the folder shows nothing where the two sides agreed on something");
    assert_eq!(given(&mut store), before + 6, "the file made here had the fifth");
}

/// An exchange of two names leaves both items unsent, and the exchange back neither. A file
/// saved over another by a rename keeps the stamp it had as a new file, with the binding it
/// takes over.
#[test]
fn an_exchange_and_a_save_over_keep_the_stamps_of_what_is_unsent() {
    let mut store = TreeStore::in_memory().unwrap();
    let (first, second) = (make_file(&mut store, ROOT, "first"), make_file(&mut store, ROOT, "second"));
    synced(&mut store, first, "A");
    synced(&mut store, second, "B");
    store.item_store().exchange_local_places(first, second).unwrap();
    let (first_since, second_since) = (unsent_since(&mut store, first), unsent_since(&mut store, second));
    assert!(first_since.is_some() && second_since.is_some() && first_since != second_since);
    store.item_store().exchange_local_places(first, second).unwrap();
    assert_eq!((unsent_since(&mut store, first), unsent_since(&mut store, second)), (None, None));

    let temporary = make_file(&mut store, ROOT, "first.tmp");
    let made_at = unsent_since(&mut store, temporary);
    assert!(made_at.is_some());
    apply(&mut store, LocalVersionChange::SavedOver { replacing: temporary, replaced: first });
    let saved = item(&mut store, temporary);
    assert_eq!((saved.onedrive_item.clone(), saved.unsent_since), (Some(reference("A")), made_at), "the content differs from the one agreed on, since the file was made");
}

/// The operations on bindings write what was agreed on, and the stamp follows: an item that
/// gives its binding up is a new item to send, and the row that takes the binding has
/// nothing here to send; a new item merged with one of OneDrive is still unsent, since it
/// was made; an item whose removal OneDrive confirmed is not.
#[test]
fn an_operation_on_a_binding_leaves_the_stamp_saying_what_differs() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "file");
    synced(&mut store, file, "A");
    let taker = store.item_store().give_up_binding(file, false).unwrap();
    assert!(unsent_since(&mut store, file).is_some(), "nothing is agreed on for it any more");
    assert_eq!(unsent_since(&mut store, taker), None);

    let made = make_file(&mut store, ROOT, "made");
    let made_at = unsent_since(&mut store, made);
    let listed = store.item_store().make_onedrive_item(&reference("B"), ItemKind::File).unwrap();
    store.item_store().merge_items(made, listed).unwrap();
    assert_eq!(unsent_since(&mut store, made), made_at);

    let removed = make_file(&mut store, ROOT, "removed");
    synced(&mut store, removed, "C");
    apply(&mut store, LocalVersionChange::Removed { item: removed });
    assert!(unsent_since(&mut store, removed).is_some());
    store.item_store().confirm_onedrive_version(removed, &crate::ConfirmedOneDriveVersion::Removed).unwrap();
    assert_eq!(unsent_since(&mut store, removed), None, "gone on both sides");
}

/// The unsent items are read in the order in which each came to be unsent, a bounded number
/// at a time, after a given stamp. What is not unsent is not among them.
#[test]
fn the_unsent_items_are_read_in_the_order_they_came_to_be_unsent() {
    let mut store = TreeStore::in_memory().unwrap();
    let agreed = make_file(&mut store, ROOT, "agreed");
    let (third, first, second) = (make_file(&mut store, ROOT, "third"), make_file(&mut store, ROOT, "first"), make_file(&mut store, ROOT, "second"));
    for (id, onedrive_id) in [(agreed, "S"), (third, "C"), (first, "A"), (second, "B")] {
        synced(&mut store, id, onedrive_id);
    }
    assert_eq!(store.item_store().list_unsent(None, 10).unwrap(), Vec::new());
    for id in [first, second, third] {
        apply(&mut store, LocalVersionChange::OpenedForWriting { item: id });
    }
    let ids = |items: Vec<Item>| items.into_iter().map(|item| item.local_item_id).collect::<Vec<_>>();
    assert_eq!(ids(store.item_store().list_unsent(None, 10).unwrap()), [first, second, third]);
    let page = store.item_store().list_unsent(None, 2).unwrap();
    let after = page[1].unsent_since;
    assert_eq!(ids(page), [first, second]);
    assert_eq!(ids(store.item_store().list_unsent(after, 2).unwrap()), [third]);
    apply(&mut store, LocalVersionChange::ClosedUnchanged { item: second });
    assert_eq!(ids(store.item_store().list_unsent(None, 10).unwrap()), [first, third]);
}
