use std::collections::BTreeMap;

use super::{ListedChangeOutcome, ListedChangeRefusal, ListedPageOutcome, ListingKind, ListingLink, ListingUnderWay};
use crate::item_store::item_row::write_item;
use crate::item_store::tests::{is_refused, is_refused_by_schema, item, make_directory, make_file, place, reference, synced};
use crate::model::{Change, ContentPresence, ContentVersion, FolderVisibility, Item, ItemKind, ItemVersion, Kind, LocalItemId, LocalVersionChange, OneDriveItemId, Placement, Row, SkipReason};
use crate::{ConfirmedOneDriveVersion, TreeStore};

const ROOT: LocalItemId = LocalItemId::ROOT;

/// The root of the drive as a listing gives it.
fn root() -> Change {
    Change::Root(Row { id: "R".into(), parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: Some("e-R".into()), ctag: None, quickxor: None, mime: None, placement: Placement::Placed })
}

/// The directory `id` as a listing gives it: in `parent` under `name`.
fn directory(id: &str, parent: &str, name: &str) -> Change {
    Change::Upsert(Row { id: id.into(), parent_id: Some(parent.into()), name: name.into(), kind: Kind::Folder, size: 0, mtime: 500, etag: Some(format!("e-{id}")), ctag: Some(format!("c-{id}")), quickxor: None, mime: None, placement: Placement::Placed })
}

/// The file `id` as a listing gives it: in `parent` under `name`, with the content `version`.
fn file(id: &str, parent: &str, name: &str, version: &str) -> Change {
    Change::Upsert(Row {
        id: id.into(),
        parent_id: Some(parent.into()),
        name: name.into(),
        kind: Kind::File,
        size: version.len() as u64,
        mtime: 1000,
        etag: Some(format!("e-{id}-{version}")),
        ctag: Some(format!("c-{version}")),
        quickxor: Some(format!("h-{version}")),
        mime: Some("text/plain".into()),
        placement: Placement::Placed,
    })
}

fn gone(id: &str) -> Change {
    Change::Delete(id.into())
}

/// The content [`file`] gives as `version`, as the row has it.
fn content(version: &str) -> ContentVersion {
    ContentVersion::Hashed { size: version.len() as u64, mtime: 1000, mtime_nsec: 0, hash: Some(format!("h-{version}")), ctag: Some(format!("c-{version}")) }
}

fn version(parent: LocalItemId, name: &str, content_version: Option<&str>) -> Option<ItemVersion> {
    Some(ItemVersion { place: Some(place(parent, name)), content: content_version.map(content) })
}

/// What follows a page of a listing that goes on.
fn next_link(link: &str) -> ListingLink {
    ListingLink::NextLink(link.to_owned())
}

/// What follows the last page of a listing.
fn delta_link(link: &str) -> ListingLink {
    ListingLink::DeltaLink(link.to_owned())
}

/// A store that knows its drive, `d1`, in which a full listing has begun.
fn store_of_drive() -> TreeStore {
    let mut store = TreeStore::in_memory().unwrap();
    store.set_drive_id("d1").unwrap();
    store.item_store().begin_full_listing().unwrap();
    store
}

/// A store that knows its drive, whose root, `R`, was listed by a full listing that ended
/// with the delta link `link-0`.
fn listed_store() -> TreeStore {
    let mut store = store_of_drive();
    write_fetched_at(&mut store, &[root()], 0, &delta_link("link-0"));
    store
}

fn count(store: &mut TreeStore) -> u64 {
    store.item_store().confirmation_count().unwrap()
}

fn ids(read_again: Vec<OneDriveItemId>) -> Vec<String> {
    read_again.iter().map(|id| id.as_str().to_owned()).collect()
}

/// Writes a page of a fetch that began at the count `fetch_began_at_count`, followed by
/// `link`: what became of it.
fn write_page(store: &mut TreeStore, changes: &[Change], fetch_began_at_count: u64, link: &ListingLink) -> ListedPageOutcome {
    store.item_store().write_listed_changes(changes, fetch_began_at_count, link).unwrap()
}

/// Writes a page fetched now, of a listing that goes on; what is to be read again.
fn write(store: &mut TreeStore, changes: &[Change]) -> Vec<String> {
    let fetch_began_at_count = count(store);
    write_fetched_at(store, changes, fetch_began_at_count, &next_link("next"))
}

/// [`write_page`]; what is to be read again.
fn write_fetched_at(store: &mut TreeStore, changes: &[Change], fetch_began_at_count: u64, link: &ListingLink) -> Vec<String> {
    ids(write_page(store, changes, fetch_began_at_count, link).outdated_items)
}

fn under_way(store: &mut TreeStore) -> Option<ListingUnderWay> {
    store.item_store().find_listing_under_way().unwrap()
}

fn bound(store: &mut TreeStore, onedrive_id: &str) -> Option<Item> {
    store.item_store().find_bound_item(&reference(onedrive_id)).unwrap()
}

/// The row of the item of OneDrive `onedrive_id`.
fn listed(store: &mut TreeStore, onedrive_id: &str) -> Item {
    bound(store, onedrive_id).unwrap_or_else(|| panic!("the item of OneDrive {onedrive_id} has no row"))
}

fn onedrive_place(store: &mut TreeStore, onedrive_id: &str) -> Option<crate::model::ItemPlace> {
    listed(store, onedrive_id).onedrive.and_then(|onedrive| onedrive.place)
}

fn every_row(store: &mut TreeStore) -> Vec<Item> {
    let numbers: Vec<i64> = store.conn.prepare("SELECT local_item_id FROM items ORDER BY local_item_id").unwrap().query_map([], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    numbers.into_iter().map(|number| item(store, LocalItemId::new(number))).collect()
}

/// What the rows say OneDrive has, in OneDrive's own words: for each bound row, by its item
/// id, its kind, its place there with the parent named by its item id, its content, eTag
/// and placement. Two stores that were given the same listing in another order say the
/// same here, whatever local item ids they gave.
fn onedrive_side(store: &mut TreeStore) -> BTreeMap<String, String> {
    let rows = every_row(store);
    let item_id_of = |id: LocalItemId| rows.iter().find(|row| row.local_item_id == id).and_then(|row| row.onedrive_item.as_ref()).map(|reference| reference.item_id.as_str().to_owned());
    let said = |row: &Item| {
        let reference = row.onedrive_item.as_ref()?;
        let onedrive = row.onedrive.as_ref().map(|onedrive| (onedrive.place.as_ref().map(|place| (item_id_of(place.parent_local_item_id), place.name.clone())), onedrive.content.clone()));
        Some((reference.item_id.as_str().to_owned(), format!("{:?} {onedrive:?} {:?} {:?}", row.kind, row.onedrive_etag, row.placement)))
    };
    rows.iter().filter_map(said).collect()
}

/// A step of the client confirmed something of `item` with OneDrive: OneDrive has
/// `onedrive` of it now, under `etag`, or does not have it; the row is stamped, and the step
/// writes that the two sides agree on it. The count the row was stamped with.
fn confirm(store: &mut TreeStore, item_id: LocalItemId, onedrive: Option<ItemVersion>, etag: Option<&str>) -> u64 {
    let confirmed = match (onedrive, etag) {
        (Some(version), Some(etag)) => ConfirmedOneDriveVersion::Present { version, etag: etag.to_owned() },
        (None, None) => ConfirmedOneDriveVersion::Removed,
        said => panic!("a confirmation is a version with its eTag, or a removal: {said:?}"),
    };
    let count = store.item_store().confirm_onedrive_version(item_id, &confirmed).unwrap();
    let mut row = item(store, item_id);
    row.synced_place = row.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone());
    row.synced_content = row.onedrive.as_ref().and_then(|onedrive| onedrive.content.clone());
    write_item(&store.conn, &row).unwrap();
    count
}

/// What one row says of the OneDrive side cannot contradict itself, whoever writes it: a
/// place is a parent and a name; a file OneDrive has, has a content there, and nothing else
/// has one; a content OneDrive gave is told by no change count; no row is its own parent;
/// the root has no place; and there is no OneDrive version and no eTag without a binding.
#[test]
fn the_schema_refuses_an_onedrive_version_that_contradicts_its_row() {
    let store = TreeStore::in_memory().unwrap();
    store
        .bench_sql(
            "INSERT INTO items (local_item_id, kind, onedrive_drive_id, onedrive_item_id, onedrive_parent_id, onedrive_name, onedrive_folded_name) VALUES (5, 'directory', 'd1', 'D', 1, 'dir', 'dir');
             INSERT INTO items (local_item_id, kind, onedrive_drive_id, onedrive_item_id, onedrive_parent_id, onedrive_name, onedrive_folded_name, onedrive_size) VALUES (6, 'file', 'd1', 'F', 5, 'file', 'file', 3);
             INSERT INTO items (local_item_id, kind, local_parent_id, local_name, local_folded_name, local_size) VALUES (7, 'file', 1, 'made-here', 'made-here', 0);
             INSERT INTO items (local_item_id, kind, onedrive_drive_id, onedrive_item_id) VALUES (8, 'file', 'd1', 'G');",
        )
        .unwrap();
    for (what, sql) in [
        ("a parent and no name", "UPDATE items SET onedrive_name = NULL WHERE local_item_id = 5"),
        ("a name and no parent", "UPDATE items SET onedrive_name = 'gone', onedrive_folded_name = 'gone' WHERE local_item_id = 8"),
        ("its own parent", "UPDATE items SET onedrive_parent_id = 5 WHERE local_item_id = 5"),
        ("a directory with a content", "UPDATE items SET onedrive_size = 1 WHERE local_item_id = 5"),
        ("a file OneDrive has with no content", "UPDATE items SET onedrive_size = NULL WHERE local_item_id = 6"),
        ("a file OneDrive does not have with a content", "UPDATE items SET onedrive_size = 1 WHERE local_item_id = 8"),
        ("a content of OneDrive told by a change count", "UPDATE items SET onedrive_change_count = 1 WHERE local_item_id = 6"),
        ("a place with no binding", "UPDATE items SET onedrive_parent_id = 1, onedrive_name = 'made-here', onedrive_folded_name = 'made-here' WHERE local_item_id = 7"),
        ("an eTag with no binding", "UPDATE items SET onedrive_etag = 'e' WHERE local_item_id = 7"),
        ("a binding gone from a row with an OneDrive version", "UPDATE items SET onedrive_drive_id = NULL, onedrive_item_id = NULL WHERE local_item_id = 5"),
        ("the root with a place", "UPDATE items SET onedrive_parent_id = 5, onedrive_name = 'root', onedrive_folded_name = 'root' WHERE local_item_id = 1"),
        ("no placement", "UPDATE items SET placement = NULL WHERE local_item_id = 5"),
        ("no count of the confirmation", "UPDATE items SET confirmed_at_count = NULL WHERE local_item_id = 5"),
    ] {
        assert!(is_refused_by_schema(store.bench_sql(sql)), "{what}");
    }
}

/// An item's parent exists and is a directory on the OneDrive side too: the schema refuses
/// a row made with, or moved to, a parent there that has no row or is a file, and refuses
/// to delete a row another names as its parent there.
#[test]
fn the_schema_refuses_an_onedrive_parent_that_has_no_row_or_is_a_file_and_keeps_one_that_is_named() {
    let store = TreeStore::in_memory().unwrap();
    store
        .bench_sql(
            "INSERT INTO items (local_item_id, kind, onedrive_drive_id, onedrive_item_id) VALUES (5, 'directory', 'd1', 'D');
             INSERT INTO items (local_item_id, kind, onedrive_drive_id, onedrive_item_id, onedrive_parent_id, onedrive_name, onedrive_folded_name, onedrive_size) VALUES (6, 'file', 'd1', 'F', 5, 'file', 'file', 3);",
        )
        .unwrap();
    for (what, sql) in [
        ("made in a directory that has no row", "INSERT INTO items (kind, onedrive_drive_id, onedrive_item_id, onedrive_parent_id, onedrive_name, onedrive_folded_name) VALUES ('directory', 'd1', 'B', 99, 'x', 'x')"),
        ("made in a file", "INSERT INTO items (kind, onedrive_drive_id, onedrive_item_id, onedrive_parent_id, onedrive_name, onedrive_folded_name) VALUES ('directory', 'd1', 'B', 6, 'x', 'x')"),
        ("moved into a directory that has no row", "UPDATE items SET onedrive_parent_id = 99 WHERE local_item_id = 6"),
        ("moved into a file", "UPDATE items SET onedrive_parent_id = 6 WHERE local_item_id = 5"),
        ("a parent deleted from over its child", "DELETE FROM items WHERE local_item_id = 5"),
    ] {
        assert!(is_refused_by_schema(store.bench_sql(sql)), "{what}");
    }
    store.bench_sql("DELETE FROM items WHERE local_item_id = 6; DELETE FROM items WHERE local_item_id = 5;").unwrap();
}

/// The root's item is row 1: the root a listing gives is bound to it, has no place and has
/// its eTag. A row that had the root's reference first, as the parent of a child listed
/// before it, goes, and what OneDrive has in it is in row 1. The root is not written as an
/// item with a place, and is not taken as gone.
#[test]
fn the_roots_item_is_row_1() {
    let mut store = store_of_drive();
    write(&mut store, &[file("A", "R", "a", "1")]);
    let holder = listed(&mut store, "R");
    assert_ne!(holder.local_item_id, ROOT);
    assert_eq!(holder.onedrive, None);

    write(&mut store, &[root()]);
    let root_row = item(&mut store, ROOT);
    assert_eq!(root_row.onedrive_item, Some(reference("R")));
    assert_eq!(root_row.onedrive, Some(ItemVersion { place: None, content: None }));
    assert_eq!(root_row.onedrive_etag.as_deref(), Some("e-R"));
    assert_eq!(store.item_store().find_item(holder.local_item_id).unwrap(), None);
    assert_eq!(onedrive_place(&mut store, "A"), Some(place(ROOT, "a")));

    write(&mut store, &[directory("D", "R", "D")]);
    write(&mut store, &[directory("R", "D", "root")]);
    write(&mut store, &[gone("R")]);
    assert_eq!(item(&mut store, ROOT), root_row);
    assert_eq!(onedrive_place(&mut store, "A"), Some(place(ROOT, "a")), "nothing goes with a root listed as gone");
}

/// An item the client did not know gets a row with its binding and its kind, and its
/// OneDrive version is what the listing says: its place, for a file its content, the eTag
/// and the placement. Nothing else is in the row: the folder does not show the item, and
/// nothing is agreed on.
#[test]
fn an_item_the_client_did_not_know_gets_a_row_with_what_the_listing_says() {
    let mut store = listed_store();
    let Change::Upsert(long) = file("L", "R", "long", "1") else { unreachable!() };
    write(&mut store, &[directory("D", "R", "Docs"), file("A", "D", "a.txt", "1"), Change::Upsert(Row { placement: Placement::Skipped(SkipReason::NameTooLong), ..long })]);
    let docs = listed(&mut store, "D");
    assert_eq!(
        docs,
        Item {
            local_item_id: docs.local_item_id,
            onedrive_item: Some(reference("D")),
            kind: ItemKind::Directory,
            local: None,
            onedrive: version(ROOT, "Docs", None),
            onedrive_etag: Some("e-D".to_owned()),
            onedrive_mime: None,
            synced_place: None,
            synced_content: None,
            content_presence: ContentPresence::NotOnDisk,
            content_before_write: None,
            placement: Placement::Placed,
            is_name_ignored: false,
            confirmed_at_count: 0,
            unsent_since: None,
        }
    );
    let a = listed(&mut store, "A");
    assert_eq!(
        a,
        Item { local_item_id: a.local_item_id, onedrive_item: Some(reference("A")), kind: ItemKind::File, onedrive: version(docs.local_item_id, "a.txt", Some("1")), onedrive_etag: Some("e-A-1".to_owned()), onedrive_mime: Some("text/plain".to_owned()), ..docs.clone() }
    );
    assert_eq!(listed(&mut store, "L").placement, Placement::Skipped(SkipReason::NameTooLong));
}

/// A file OneDrive gives no hash for has a content told by its cTag alone, and a time
/// OneDrive gave is whole seconds: written to the synced content from the same value, the
/// two are equal.
#[test]
fn a_file_with_no_hash_has_a_content_told_by_its_ctag() {
    let mut store = listed_store();
    let Change::Upsert(row) = file("A", "R", "a", "1") else { unreachable!() };
    write(&mut store, &[Change::Upsert(Row { quickxor: None, ..row })]);
    let mut a = listed(&mut store, "A");
    let listed_content = a.onedrive.clone().unwrap().content;
    assert_eq!(listed_content, Some(ContentVersion::Hashed { size: 1, mtime: 1000, mtime_nsec: 0, hash: None, ctag: Some("c-1".to_owned()) }));
    a.synced_place = a.onedrive.clone().unwrap().place;
    a.synced_content = listed_content;
    write_item(&store.conn, &a).unwrap();
    assert_eq!(store.item_store().list_changed_in_onedrive(None, 10).unwrap(), Vec::new(), "the two sides agree on what OneDrive has");
}

/// The MIME type OneDrive gives for a file's content is written with its version, follows
/// what a later listing says, also that there is none, and goes with the version of a file
/// listed gone. A directory has none, whatever the listing says. It is OneDrive's of the
/// item, and goes with the binding to the row that takes it.
#[test]
fn the_mime_type_of_a_files_content_is_written_with_its_version() {
    let mut store = listed_store();
    let kept = make_file(&mut store, ROOT, "kept");
    synced(&mut store, kept, "K");
    let Change::Upsert(folder) = directory("D", "R", "D") else { unreachable!() };
    let Change::Upsert(picture) = file("K", "R", "kept", "1") else { unreachable!() };
    let with_mime = |row: &Row, mime: Option<&str>| Change::Upsert(Row { mime: mime.map(str::to_owned), ..row.clone() });
    write(&mut store, &[with_mime(&folder, Some("inode/directory")), with_mime(&picture, Some("image/jpeg")), file("A", "D", "a", "1")]);
    assert_eq!((item(&mut store, kept).onedrive_mime.as_deref(), listed(&mut store, "A").onedrive_mime.as_deref(), listed(&mut store, "D").onedrive_mime), (Some("image/jpeg"), Some("text/plain"), None));

    write(&mut store, &[with_mime(&picture, Some("image/png"))]);
    assert_eq!(item(&mut store, kept).onedrive_mime.as_deref(), Some("image/png"));
    let taker = store.item_store().give_up_binding(kept, false).unwrap();
    assert_eq!((item(&mut store, kept).onedrive_mime, item(&mut store, taker).onedrive_mime.as_deref()), (None, Some("image/png")));
    write(&mut store, &[with_mime(&picture, None)]);
    assert_eq!(item(&mut store, taker).onedrive_mime, None);

    let other = make_file(&mut store, ROOT, "other");
    synced(&mut store, other, "O");
    write(&mut store, &[file("O", "R", "other", "1")]);
    write(&mut store, &[gone("O")]);
    assert_eq!(item(&mut store, other).onedrive_mime, None);
    for (what, sql) in [
        ("a MIME type with no binding", "INSERT INTO items (kind, onedrive_mime) VALUES ('file', 'text/plain')"),
        ("a MIME type of a file OneDrive does not have", "UPDATE items SET onedrive_mime = 'text/plain' WHERE onedrive_item_id = 'O'"),
        ("a MIME type of a directory", "UPDATE items SET onedrive_mime = 'text/plain' WHERE onedrive_item_id = 'D'"),
    ] {
        assert!(is_refused_by_schema(store.bench_sql(sql)), "{what}");
    }
}

/// A parent named before it arrives gets its row at that first mention, with its binding
/// alone, and is filled when it arrives, in a later page or in the same one. One that never
/// arrives stays as it is while a row names it, and what is in it cannot be shown.
#[test]
fn a_parent_named_before_it_arrives_gets_its_row_at_its_first_mention() {
    let mut store = listed_store();
    write(&mut store, &[file("A", "D", "a.txt", "1")]);
    let docs = listed(&mut store, "D");
    assert_eq!((docs.kind, docs.onedrive.clone(), docs.onedrive_etag.clone(), docs.local.clone()), (ItemKind::Directory, None, None, None));
    let a = listed(&mut store, "A");
    assert_eq!(a.onedrive, version(docs.local_item_id, "a.txt", Some("1")));
    assert_eq!(store.item_store().find_folder_visibility(a.local_item_id).unwrap(), FolderVisibility::NotKnownYet, "its directory was never listed");

    write(&mut store, &[directory("D", "R", "Docs")]);
    assert_eq!(listed(&mut store, "D"), Item { onedrive: version(ROOT, "Docs", None), onedrive_etag: Some("e-D".to_owned()), ..docs });
    assert_eq!(store.item_store().find_folder_visibility(a.local_item_id).unwrap(), FolderVisibility::CanBeShown);

    write(&mut store, &[file("C", "N", "c", "1"), directory("N", "M", "N"), directory("M", "R", "M")]);
    let (m, n) = (listed(&mut store, "M").local_item_id, listed(&mut store, "N").local_item_id);
    assert_eq!((onedrive_place(&mut store, "C"), onedrive_place(&mut store, "N"), onedrive_place(&mut store, "M")), (Some(place(n, "c")), Some(place(m, "N")), Some(place(ROOT, "M"))));
}

/// Nothing of a row is touched but what is OneDrive's: the local version, the synced place
/// and content, whether the content is on disk and what it was before a write stay as they
/// were through a listed change and a listed removal.
#[test]
fn a_listing_writes_nothing_of_a_row_but_what_onedrive_has() {
    let mut store = listed_store();
    let report = make_file(&mut store, ROOT, "report");
    synced(&mut store, report, "A");
    store.item_store().apply(&LocalVersionChange::OpenedForWriting { item: report }).unwrap();
    let before = item(&mut store, report);
    assert!(before.content_before_write.is_some() && before.content_presence == ContentPresence::OnDisk && before.synced_content.is_some());

    write(&mut store, &[file("A", "R", "renamed", "22")]);
    assert_eq!(item(&mut store, report), Item { onedrive: version(ROOT, "renamed", Some("22")), onedrive_etag: Some("e-A-22".to_owned()), onedrive_mime: Some("text/plain".to_owned()), ..before.clone() });

    write(&mut store, &[gone("A")]);
    assert_eq!(item(&mut store, report), before, "gone from OneDrive, and the row stays for the plan");
}

/// Two items of OneDrive may hold one name within a listing that has not ended: an
/// exchange of two names spread over two pages, and a rotation of three across two
/// directories in one.
#[test]
fn two_items_may_hold_one_name_while_a_listing_goes_on() {
    let mut store = listed_store();
    write(&mut store, &[file("A", "R", "a", "1"), file("B", "R", "b", "1"), directory("D", "R", "D"), file("C", "D", "c", "1")]);
    let d = listed(&mut store, "D").local_item_id;

    write(&mut store, &[file("A", "R", "b", "1")]);
    assert_eq!((onedrive_place(&mut store, "A"), onedrive_place(&mut store, "B")), (Some(place(ROOT, "b")), Some(place(ROOT, "b"))));
    write(&mut store, &[file("B", "R", "a", "1")]);
    assert_eq!((onedrive_place(&mut store, "A"), onedrive_place(&mut store, "B")), (Some(place(ROOT, "b")), Some(place(ROOT, "a"))));

    write(&mut store, &[file("A", "D", "c", "1"), file("C", "R", "a", "1"), file("B", "R", "b", "1")]);
    assert_eq!(
        (onedrive_place(&mut store, "A"), onedrive_place(&mut store, "C"), onedrive_place(&mut store, "B")),
        (Some(place(d, "c")), Some(place(ROOT, "a")), Some(place(ROOT, "b")))
    );
}

/// A directory moved under what was its child, in either order of the two changes.
#[test]
fn a_directory_is_moved_under_what_was_its_child() {
    for moved in [[directory("C", "R", "C"), directory("P", "C", "P")], [directory("P", "C", "P"), directory("C", "R", "C")]] {
        let mut store = listed_store();
        write(&mut store, &[directory("P", "R", "P"), directory("C", "P", "C")]);
        write(&mut store, &moved);
        let c = listed(&mut store, "C").local_item_id;
        assert_eq!((onedrive_place(&mut store, "C"), onedrive_place(&mut store, "P")), (Some(place(ROOT, "C")), Some(place(c, "P"))));
    }
}

/// An item the listing says is gone has no OneDrive version from then on. Its row stays
/// while the folder shows the item or something is agreed on; a row left with its binding
/// and nothing else goes, a directory with what was in it, and a parent that was never
/// listed with its last child.
#[test]
fn an_item_listed_gone_has_no_onedrive_version_and_a_row_left_with_its_binding_alone_goes() {
    let mut store = listed_store();
    let kept = make_file(&mut store, ROOT, "kept");
    synced(&mut store, kept, "K");
    write(&mut store, &[file("K", "R", "kept", "1"), directory("D", "R", "D"), file("A", "D", "a", "1"), file("X", "U", "x", "1")]);
    let before = item(&mut store, kept);
    assert!(bound(&mut store, "U").is_some());

    write(&mut store, &[gone("D"), gone("A"), gone("K"), gone("X"), gone("never-known")]);
    assert_eq!(item(&mut store, kept), Item { onedrive: None, onedrive_etag: None, onedrive_mime: None, ..before });
    for id in ["D", "A", "X", "U", "never-known"] {
        assert_eq!(bound(&mut store, id), None, "{id}");
    }
    assert_eq!(every_row(&mut store).len(), 2, "the root and the file the folder shows");
}

/// What OneDrive has below a directory listed gone is gone with it, also when the listing
/// does not name it. What the same page moves out of the directory stays, and what it moves
/// into it is gone, in either order of the changes.
#[test]
fn what_is_below_a_directory_listed_gone_is_gone_with_it() {
    for at_first in [true, false] {
        let mut store = listed_store();
        write(&mut store, &[directory("D", "R", "D"), directory("E", "D", "E"), file("A", "E", "a", "1"), file("B", "D", "b", "1"), file("O", "D", "out", "1"), file("N", "R", "in", "1")]);
        let mut page = vec![file("O", "R", "out", "1"), file("N", "E", "in", "1")];
        page.insert(if at_first { 0 } else { 2 }, gone("D"));
        write(&mut store, &page);
        for id in ["D", "E", "A", "B", "N"] {
            assert_eq!(bound(&mut store, id), None, "{id}, the removal listed first: {at_first}");
        }
        assert_eq!(onedrive_place(&mut store, "O"), Some(place(ROOT, "out")));
    }
}

/// An item is deleted and another is created at its name, in either order of the two.
#[test]
fn an_item_is_deleted_and_another_created_at_its_name() {
    for page in [[gone("A"), file("B", "R", "name", "2")], [file("B", "R", "name", "2"), gone("A")]] {
        let mut store = listed_store();
        write(&mut store, &[file("A", "R", "name", "1")]);
        write(&mut store, &page);
        assert_eq!(bound(&mut store, "A"), None);
        assert_eq!(listed(&mut store, "B").onedrive, version(ROOT, "name", Some("2")));
    }
}

/// Of several changes of one item in a page the last one counts.
#[test]
fn the_last_change_of_an_item_in_a_page_counts() {
    let mut store = listed_store();
    write(&mut store, &[file("A", "R", "a", "1"), gone("A"), file("B", "R", "b", "1"), gone("B"), file("B", "R", "b", "2")]);
    assert_eq!(bound(&mut store, "A"), None);
    assert_eq!(listed(&mut store, "B").onedrive, version(ROOT, "b", Some("2")));
}

/// What becomes of each change is answered, in the order they were given: written, or
/// refused with its reason, and the rest of the page is written. The kind of a bound item
/// never changes: a listing that says otherwise is refused for that item; so is an item
/// whose parent is a file, is itself or is not said, one with no id, and the root given a
/// place or listed gone. Of several changes of one item the last counts, and each has its
/// outcome. No row is made for an item that is refused, nor for its parent.
#[test]
fn each_change_is_answered_written_or_refused_with_its_reason_and_a_refused_one_makes_no_row() {
    use ListedChangeOutcome::{Refused, Written};
    let mut store = listed_store();
    write(&mut store, &[directory("D", "R", "D"), file("A", "R", "a", "1")]);
    let mut expected = onedrive_side(&mut store);
    let rows_made_so_far = every_row(&mut store).last().unwrap().local_item_id.as_i64();
    let Change::Upsert(orphan) = file("N", "R", "no-parent", "1") else { unreachable!() };
    let now = count(&mut store);
    let outcome = write_page(
        &mut store,
        &[
            file("D", "R", "D", "1"),
            directory("A", "R", "a"),
            file("X", "A", "in-a-file", "1"),
            Change::Upsert(Row { parent_id: None, ..orphan.clone() }),
            Change::Upsert(Row { parent_id: Some(String::new()), ..orphan }),
            file("S", "S", "in-itself", "1"),
            file("", "NEVER-NAMED", "no-id", "1"),
            directory("R", "D", "root"),
            gone("C"),
            file("C", "R", "c", "1"),
            gone("C"),
            gone("never-known"),
            gone(""),
        ],
        now,
        &next_link("next"),
    );
    assert_eq!(
        outcome.changes,
        [
            Refused(ListedChangeRefusal::KindChanged),
            Refused(ListedChangeRefusal::KindChanged),
            Refused(ListedChangeRefusal::ParentIsFile),
            Refused(ListedChangeRefusal::NoParent),
            Refused(ListedChangeRefusal::NoParent),
            Refused(ListedChangeRefusal::PlacedInItself),
            Refused(ListedChangeRefusal::NoItemId),
            Refused(ListedChangeRefusal::RootGivenPlace),
            Written,
            Written,
            Written,
            Written,
            Refused(ListedChangeRefusal::NoItemId),
        ]
    );
    assert_eq!(outcome.outdated_items, Vec::new());
    assert!(outcome.is_link_written());
    assert_eq!(onedrive_side(&mut store), expected, "the refused items left no row and changed none");
    assert_eq!(make_file(&mut store, ROOT, "made-next").as_i64(), rows_made_so_far + 1, "the page made no row: none for a refused item, and none for its parent");

    assert_eq!(write_page(&mut store, &[gone("R"), file("B", "R", "b", "1")], now, &next_link("next")).changes, [Refused(ListedChangeRefusal::RootListedGone), Written]);
    expected.insert("B".to_owned(), onedrive_side(&mut store)["B"].clone());
    assert_eq!(onedrive_side(&mut store), expected);
}

/// N1 of the review: a listing that contradicts itself about the root is refused for the
/// root, and the rest of the page is written: a root with the empty text for an id, and a
/// root whose id is that of a file or of an item the folder shows. Row 1 stays as it was.
#[test]
fn a_root_no_row_can_be_is_refused_for_itself_and_the_rest_of_the_page_is_written() {
    use ListedChangeOutcome::{Refused, Written};
    let root_with_id = |id: &str| {
        let Change::Root(row) = root() else { unreachable!() };
        Change::Root(Row { id: id.to_owned(), ..row })
    };
    let mut store = store_of_drive();
    assert_eq!(write_page(&mut store, &[root_with_id(""), file("A", "R", "a", "1")], 0, &next_link("next")).changes, [Refused(ListedChangeRefusal::NoItemId), Written]);
    assert_eq!(item(&mut store, ROOT).onedrive_item, None);
    assert!(bound(&mut store, "A").is_some());

    write(&mut store, &[root()]);
    let shown = make_directory(&mut store, ROOT, "shown");
    synced(&mut store, shown, "S");
    let root_row = item(&mut store, ROOT);
    assert_eq!(write_page(&mut store, &[root_with_id("A"), file("B", "R", "b", "1")], 0, &next_link("next")).changes, [Refused(ListedChangeRefusal::RootIdOfAnotherItem), Written]);
    assert_eq!(write_page(&mut store, &[root_with_id("S"), file("C", "R", "c", "1")], 0, &next_link("next")).changes, [Refused(ListedChangeRefusal::RootIdOfAnotherItem), Written]);
    assert_eq!(item(&mut store, ROOT), root_row);
    assert_eq!((onedrive_place(&mut store, "B"), onedrive_place(&mut store, "C")), (Some(place(ROOT, "b")), Some(place(ROOT, "c"))));
}

/// What a store holds before the pages of [`the_order_of_the_changes_in_a_page_does_not_matter`].
fn listed_tree() -> Vec<Change> {
    vec![
        root(),
        directory("D1", "R", "D1"),
        file("A", "D1", "a", "1"),
        directory("D2", "R", "D2"),
        file("B", "R", "b", "1"),
        directory("D3", "R", "D3"),
        file("C", "D3", "c", "1"),
        file("E", "D3", "e", "1"),
    ]
}

fn permutations(of: &[usize]) -> Vec<Vec<usize>> {
    if of.len() <= 1 {
        return vec![of.to_vec()];
    }
    let mut all = Vec::new();
    for at in 0..of.len() {
        let mut rest = of.to_vec();
        let first = rest.remove(at);
        all.extend(permutations(&rest).into_iter().map(|mut order| {
            order.insert(0, first);
            order
        }));
    }
    all
}

/// The order of the changes within a page does not matter: a new directory, a file moved,
/// a directory moved with what is in it, a file gone, and a directory gone while a file is
/// moved out of it into the new one, written in every order, leave the same rows.
#[test]
fn the_order_of_the_changes_in_a_page_does_not_matter() {
    let page = [directory("N", "R", "New"), file("A", "D2", "a", "1"), directory("D1", "D2", "D1"), gone("B"), gone("D3"), file("E", "N", "e", "1")];
    let written = |order: &[usize]| {
        let mut store = store_of_drive();
        write(&mut store, &listed_tree());
        let changes: Vec<Change> = order.iter().map(|&at| page[at].clone()).collect();
        write(&mut store, &changes);
        onedrive_side(&mut store)
    };
    let orders = permutations(&(0..page.len()).collect::<Vec<_>>());
    assert_eq!(orders.len(), 720);
    let expected = written(&orders[0]);
    assert_eq!(expected.keys().map(String::as_str).collect::<Vec<_>>(), ["A", "D1", "D2", "E", "N", "R"]);
    for order in &orders[1..] {
        assert_eq!(written(order), expected, "the changes in the order {order:?}");
    }
}

/// The same for a larger page, shuffled: sixty changes over a tree of forty items, with
/// new directories whose parents come after them, moves, removals and a directory gone
/// with what is in it.
#[test]
fn a_larger_page_shuffled_leaves_the_same_rows() {
    let mut tree = vec![root()];
    tree.extend((0..10).map(|n| directory(&format!("D{n}"), &if n == 0 { "R".to_owned() } else { format!("D{}", n / 2) }, &format!("d{n}"))));
    tree.extend((0..30).map(|n| file(&format!("F{n}"), &format!("D{}", n % 10), &format!("f{n}"), "1")));
    let mut page = Vec::new();
    page.extend((0..10).map(|n| file(&format!("F{n}"), &format!("D{}", (n + 3) % 10), &format!("f{n}"), "2")));
    page.extend((10..16).map(|n| gone(&format!("F{n}"))));
    page.extend((0..5).map(|n| directory(&format!("N{n}"), &if n == 0 { "R".to_owned() } else { format!("N{}", n - 1) }, &format!("n{n}"))));
    page.extend((0..10).map(|n| file(&format!("G{n}"), &format!("N{}", n % 5), &format!("g{n}"), "1")));
    page.extend([directory("D9", "N4", "d9"), gone("D8"), directory("D3", "R", "d3")]);
    let written = |page: &[Change]| {
        let mut store = store_of_drive();
        write(&mut store, &tree);
        write(&mut store, page);
        onedrive_side(&mut store)
    };
    let expected = written(&page);
    assert!(!expected.contains_key("F18") && !expected.contains_key("D8") && expected.contains_key("F8"), "what was in the directory that is gone went with it, and what was moved out stayed");
    // A generator of the test's own, so that a failure shows the same orders again.
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = |below: usize| {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) as usize % below
    };
    for round in 0..25 {
        for at in (1..page.len()).rev() {
            page.swap(at, next(at + 1));
        }
        assert_eq!(written(&page), expected, "shuffle {round}");
    }
}

/// A page written twice leaves the rows as written once, and answers the same.
#[test]
fn a_page_written_twice_leaves_the_rows_as_once() {
    let mut store = store_of_drive();
    write(&mut store, &listed_tree());
    let page = [directory("N", "R", "New"), file("F", "N", "f", "1"), file("A", "D2", "a", "2"), gone("B"), gone("D3"), file("X", "U", "x", "1")];
    let first_answer = write(&mut store, &page);
    let once = every_row(&mut store);
    assert_eq!(write(&mut store, &page), first_answer);
    assert_eq!(every_row(&mut store), once);
}

/// A listing that moves a directory with ten thousand items below it writes one row.
#[test]
fn a_directory_moved_with_ten_thousand_items_below_it_is_one_row_written() {
    let mut store = listed_store();
    let mut tree = vec![directory("D", "R", "D"), directory("E", "R", "E")];
    tree.extend((0..100).map(|n| directory(&format!("S{n}"), "D", &format!("s{n}"))));
    tree.extend((0..9900).map(|n| file(&format!("F{n}"), &format!("S{}", n % 100), &format!("f{n}"), "1")));
    write(&mut store, &tree);
    store
        .bench_sql(
            "CREATE TEMP TABLE written (item INTEGER);
             CREATE TEMP TRIGGER written_update AFTER UPDATE ON items BEGIN INSERT INTO written VALUES (NEW.local_item_id); END;
             CREATE TEMP TRIGGER written_insert AFTER INSERT ON items BEGIN INSERT INTO written VALUES (NEW.local_item_id); END;
             CREATE TEMP TRIGGER written_delete AFTER DELETE ON items BEGIN INSERT INTO written VALUES (OLD.local_item_id); END;",
        )
        .unwrap();
    write(&mut store, &[directory("D", "E", "D")]);
    let written: Vec<i64> = store.conn.prepare("SELECT item FROM written").unwrap().query_map([], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    let (d, e) = (listed(&mut store, "D").local_item_id, listed(&mut store, "E").local_item_id);
    assert_eq!(written, [d.as_i64()]);
    assert_eq!(onedrive_place(&mut store, "D"), Some(place(e, "D")));
    let deep = listed(&mut store, "F9899").local_item_id;
    assert_eq!(store.item_store().find_folder_visibility(deep).unwrap(), FolderVisibility::CanBeShown);
}

fn present(parent: LocalItemId, name: &str, content_version: Option<&str>, etag: &str) -> ConfirmedOneDriveVersion {
    ConfirmedOneDriveVersion::Present { version: version(parent, name, content_version).unwrap(), etag: etag.to_owned() }
}

/// A confirmation writes, in one transaction, the OneDrive version as Graph answered it and
/// that answer's eTag, raises the count of confirmations and stamps the row with it. Nothing
/// else of the row is written: what the two sides agree on is the step's to write.
#[test]
fn a_confirmation_writes_the_version_and_the_etag_and_stamps_the_row_with_the_next_count() {
    let mut store = listed_store();
    write(&mut store, &[file("A", "R", "a", "1"), file("B", "R", "b", "1"), directory("D", "R", "D")]);
    let (a, b, d) = (listed(&mut store, "A").local_item_id, listed(&mut store, "B").local_item_id, listed(&mut store, "D").local_item_id);
    let before = item(&mut store, a);
    assert_eq!(count(&mut store), 0);

    assert_eq!(store.item_store().confirm_onedrive_version(a, &present(d, "moved", Some("22"), "e-A-22")).unwrap(), 1);
    assert_eq!(item(&mut store, a), Item { onedrive: version(d, "moved", Some("22")), onedrive_etag: Some("e-A-22".to_owned()), confirmed_at_count: 1, ..before });
    assert_eq!(store.item_store().confirm_onedrive_version(b, &present(ROOT, "b", Some("1"), "e-B-1")).unwrap(), 2);
    assert_eq!(store.item_store().confirm_onedrive_version(a, &present(ROOT, "a", Some("333"), "e-A-333")).unwrap(), 3);
    assert_eq!((item(&mut store, a).confirmed_at_count, item(&mut store, b).confirmed_at_count, count(&mut store)), (3, 2, 3));
    assert_eq!(store.item_store().confirm_onedrive_version(d, &present(ROOT, "Renamed", None, "e-D-2")).unwrap(), 4);
    assert_eq!(listed(&mut store, "D").onedrive, version(ROOT, "Renamed", None));
}

/// A confirmed removal leaves the row with its binding alone, stamped: no version on either
/// side, no eTag, nothing agreed on. The row stays, so that a page fetched before the
/// removal finds the binding taken.
#[test]
fn a_confirmed_removal_leaves_the_row_with_its_binding_alone_stamped() {
    let mut store = listed_store();
    let kept = make_file(&mut store, ROOT, "kept");
    synced(&mut store, kept, "K");
    write(&mut store, &[file("K", "R", "kept", "1")]);
    store.item_store().apply(&LocalVersionChange::Removed { item: kept }).unwrap();
    assert!(item(&mut store, kept).synced_place.is_some());

    assert_eq!(store.item_store().confirm_onedrive_version(kept, &ConfirmedOneDriveVersion::Removed).unwrap(), 1);
    let tombstone = item(&mut store, kept);
    assert_eq!(
        (tombstone.onedrive_item.clone(), tombstone.local.clone(), tombstone.onedrive.clone(), tombstone.onedrive_etag.clone(), tombstone.synced_place.clone(), tombstone.synced_content.clone(), tombstone.confirmed_at_count),
        (Some(reference("K")), None, None, None, None, None, 1)
    );
    assert_eq!(bound(&mut store, "K"), Some(tombstone));
}

/// What OneDrive cannot have confirmed is refused, and is no confirmation: of an id with no
/// row, of an item with no binding, of the root as removed or with a place, a removal of an
/// item the folder still shows, a file with no content or a directory with one, a content
/// told by a change count, and a place in a file, in itself or in a directory with no row.
#[test]
fn a_confirmation_that_cannot_be_is_refused_and_raises_no_count() {
    let mut store = listed_store();
    write(&mut store, &[file("A", "R", "a", "1"), directory("D", "R", "D")]);
    let (a, d) = (listed(&mut store, "A").local_item_id, listed(&mut store, "D").local_item_id);
    let here = make_file(&mut store, ROOT, "made-here");
    let shown = make_file(&mut store, ROOT, "shown");
    synced(&mut store, shown, "S");
    let changed_here = ConfirmedOneDriveVersion::Present { version: ItemVersion { place: Some(place(ROOT, "a")), content: Some(ContentVersion::ChangedHere { size: 1, mtime: 1, mtime_nsec: 0, change_count: 1 }) }, etag: "e".to_owned() };
    let rows = every_row(&mut store);
    for (what, item_id, confirmed) in [
        ("an id with no row", LocalItemId::new(999), present(ROOT, "x", Some("1"), "e")),
        ("an item with no binding", here, present(ROOT, "made-here", Some("1"), "e")),
        ("the root removed", ROOT, ConfirmedOneDriveVersion::Removed),
        ("the root with a place", ROOT, present(d, "root", None, "e")),
        ("a removal of an item the folder shows", shown, ConfirmedOneDriveVersion::Removed),
        ("a file with no content", a, present(ROOT, "a", None, "e")),
        ("a directory with a content", d, present(ROOT, "D", Some("1"), "e")),
        ("a content told by a change count", a, changed_here),
        ("a place in a file", d, present(a, "D", None, "e")),
        ("a place in itself", d, present(d, "D", None, "e")),
        ("a place in a directory with no row", a, present(LocalItemId::new(999), "a", Some("1"), "e")),
    ] {
        assert!(is_refused(store.item_store().confirm_onedrive_version(item_id, &confirmed)), "{what}");
    }
    assert_eq!(count(&mut store), 0, "a refused confirmation is none");
    assert_eq!(every_row(&mut store), rows);
}

/// The guard against an outdated listing: a page fetched before a confirmation and written
/// after it. What it says of the row is not taken, and the item's id is answered for it to
/// be read again; the same page saying what was confirmed is taken; so is a page of a fetch
/// that began after the confirmation.
#[test]
fn a_page_fetched_before_a_confirmation_does_not_overwrite_what_was_confirmed() {
    let mut store = listed_store();
    write(&mut store, &[file("A", "R", "a", "1"), file("B", "R", "b", "1")]);
    let a = listed(&mut store, "A").local_item_id;
    let fetch_began_at_count = count(&mut store);
    confirm(&mut store, a, version(ROOT, "a", Some("333")), Some("e-A-333"));
    let confirmed = item(&mut store, a);

    assert_eq!(write_fetched_at(&mut store, &[file("A", "R", "a", "22"), file("B", "R", "b", "22")], fetch_began_at_count, &next_link("next")), ["A"]);
    assert_eq!(item(&mut store, a), confirmed);
    assert_eq!(listed(&mut store, "B").onedrive, version(ROOT, "b", Some("22")), "the rest of the page is written");

    assert_eq!(write_fetched_at(&mut store, &[gone("A")], fetch_began_at_count, &next_link("next")), ["A"]);
    assert_eq!(item(&mut store, a), confirmed);

    assert_eq!(write_fetched_at(&mut store, &[file("A", "R", "a", "333")], fetch_began_at_count, &next_link("next")), Vec::<String>::new());
    assert_eq!(item(&mut store, a), confirmed);

    assert_eq!(write(&mut store, &[file("A", "R", "a", "4444")]), Vec::<String>::new());
    assert_eq!(item(&mut store, a), Item { onedrive: version(ROOT, "a", Some("4444")), onedrive_etag: Some("e-A-4444".to_owned()), ..confirmed });
}

/// The guard for a confirmed removal: a page fetched before it does not bring the item
/// back, and its removal there is what was confirmed. The row stays, with no version on
/// either side, while that listing goes on, at its end, and through the item read again;
/// it goes at the end of a listing that began after the confirmation.
#[test]
fn a_page_fetched_before_a_confirmed_removal_does_not_bring_the_item_back() {
    let mut store = listed_store();
    write(&mut store, &[file("A", "R", "a", "1")]);
    let a = listed(&mut store, "A").local_item_id;
    let fetch_began_at_count = count(&mut store);
    confirm(&mut store, a, None, None);
    let removed = item(&mut store, a);
    assert_eq!((removed.local.clone(), removed.onedrive.clone(), removed.synced_place.clone()), (None, None, None));

    assert_eq!(write_fetched_at(&mut store, &[file("A", "R", "a", "1")], fetch_began_at_count, &next_link("next")), ["A"]);
    assert_eq!(write_fetched_at(&mut store, &[gone("A")], fetch_began_at_count, &next_link("next")), Vec::<String>::new());
    assert_eq!(write_fetched_at(&mut store, &[], fetch_began_at_count, &delta_link("link-1")), Vec::<String>::new());
    assert_eq!(under_way(&mut store), None);
    assert_eq!(write(&mut store, &[gone("A")]), Vec::<String>::new(), "the item read again, and not found");
    assert_eq!(item(&mut store, a), removed);

    let later = count(&mut store);
    write_fetched_at(&mut store, &[], later, &delta_link("link-2"));
    assert_eq!(store.item_store().find_item(a).unwrap(), None);
}

/// The row of a confirmed removal is kept by the count the listing began at, not by the
/// count of the call that ends it: a listing that began before the removal and ends with a
/// page fetched after it leaves the row, and the next listing takes it.
#[test]
fn the_row_of_a_confirmed_removal_stays_until_a_listing_ends_that_began_after_it() {
    let mut store = listed_store();
    write_fetched_at(&mut store, &[file("A", "R", "a", "1"), file("B", "R", "b", "1")], 0, &delta_link("link-1"));
    let a = listed(&mut store, "A").local_item_id;
    let began_at_count = count(&mut store);
    write_fetched_at(&mut store, &[file("B", "R", "b", "22")], began_at_count, &next_link("next-1"));
    confirm(&mut store, a, None, None);

    let later = count(&mut store);
    assert!(later > began_at_count);
    write_fetched_at(&mut store, &[], later, &delta_link("link-2"));
    assert_eq!(under_way(&mut store), None);
    assert!(bound(&mut store, "A").is_some(), "the listing began before the removal was confirmed");

    write_fetched_at(&mut store, &[], later, &delta_link("link-3"));
    assert_eq!(bound(&mut store, "A"), None);
}

/// What a directory listed gone takes with it is guarded too: a file confirmed in it after
/// the fetch began keeps its version and is read again.
#[test]
fn a_directory_listed_gone_does_not_take_what_was_confirmed_in_it_after_the_fetch_began() {
    let mut store = listed_store();
    write(&mut store, &[directory("D", "R", "D"), file("A", "D", "a", "1"), file("B", "D", "b", "1")]);
    let (d, a) = (listed(&mut store, "D").local_item_id, listed(&mut store, "A").local_item_id);
    let fetch_began_at_count = count(&mut store);
    confirm(&mut store, a, version(d, "a", Some("22")), Some("e-A-22"));

    assert_eq!(write_fetched_at(&mut store, &[gone("D")], fetch_began_at_count, &next_link("next")), ["A"]);
    assert_eq!(item(&mut store, a).onedrive, version(d, "a", Some("22")));
    assert_eq!(bound(&mut store, "B"), None);
    assert_eq!(listed(&mut store, "D").onedrive, None, "its row stays while a row names it");
}

/// A full listing in several pages over the rows of an earlier one: it marks what it sees,
/// and at its end what it did not see has no OneDrive version, as if listed gone. Nothing
/// is taken before its last page. What was confirmed after it began and it did not see is
/// answered to be read again, and the listing does not end before that was given: its link
/// is not written. A listing of changes after it takes nothing it does not name.
#[test]
fn a_full_listing_takes_what_it_did_not_see_at_its_end_and_nothing_before() {
    let mut store = listed_store();
    let kept = make_file(&mut store, ROOT, "kept");
    synced(&mut store, kept, "K");
    write_fetched_at(&mut store, &[directory("D", "R", "D"), file("A", "D", "a", "1"), file("B", "R", "b", "1"), file("K", "R", "kept", "1"), file("U", "R", "u", "1")], 0, &delta_link("link-1"));
    let u = listed(&mut store, "U").local_item_id;
    let before = onedrive_side(&mut store);

    let began_at_count = store.item_store().begin_full_listing().unwrap();
    assert_eq!(began_at_count, count(&mut store));
    confirm(&mut store, u, version(ROOT, "u", Some("22")), Some("e-U-22"));
    let confirmed = onedrive_side(&mut store);
    assert_eq!(write_fetched_at(&mut store, &[root(), directory("D", "R", "D")], began_at_count, &next_link("next-1")), Vec::<String>::new());
    assert_eq!(onedrive_side(&mut store), confirmed, "nothing is taken before the listing's last page");
    assert_ne!(confirmed, before);

    let last_page = write_page(&mut store, &[file("B", "R", "b", "22")], began_at_count, &delta_link("link-2"));
    assert_eq!((last_page.changes.clone(), ids(last_page.outdated_items.clone()), last_page.is_link_written()), (vec![ListedChangeOutcome::Written], vec!["U".to_owned()], false));
    assert_eq!(onedrive_side(&mut store).keys().map(String::as_str).collect::<Vec<_>>(), ["B", "D", "K", "R", "U"]);
    assert_eq!(bound(&mut store, "A"), None);
    assert_eq!(listed(&mut store, "K").onedrive, None, "the folder shows it: its row stays");
    assert_eq!(listed(&mut store, "B").onedrive, version(ROOT, "b", Some("22")));
    assert_eq!(listed(&mut store, "U").onedrive, version(ROOT, "u", Some("22")));
    assert_eq!(store.delta_link().unwrap(), None, "the listing has not ended: U is to be read again");
    assert_eq!(under_way(&mut store), Some(ListingUnderWay { kind: ListingKind::Full, began_at_count, next_link: Some("next-1".to_owned()) }));

    let later = count(&mut store);
    assert!(write_page(&mut store, &[file("U", "R", "u", "22")], later, &delta_link("link-2")).is_link_written());
    assert_eq!(store.delta_link().unwrap().as_deref(), Some("link-2"));
    assert_eq!(under_way(&mut store), None);
    assert_eq!(onedrive_side(&mut store).keys().map(String::as_str).collect::<Vec<_>>(), ["B", "D", "K", "R", "U"]);

    let after = onedrive_side(&mut store);
    write_fetched_at(&mut store, &[], later, &delta_link("link-3"));
    assert_eq!(onedrive_side(&mut store), after, "the full listing ended: a listing of changes takes nothing it does not name");
}

/// M1 of the review, the order the entry promises. A step's upload of X is confirmed; a
/// listing of changes fetched before that, one page with its link, says another client
/// changed X since. The call leaves X to be read again, and so writes no link and ends no
/// listing. The reading of X fails and the daemon stops: the store still has the old link,
/// the listing asked from it again names X again, and the row then says what OneDrive has.
#[test]
fn a_page_that_leaves_an_item_to_read_again_writes_no_link_so_that_a_failed_reading_loses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.sqlite");
    let mut store = TreeStore::open(&path).unwrap();
    store.set_drive_id("d1").unwrap();
    store.item_store().begin_full_listing().unwrap();
    write_fetched_at(&mut store, &[root(), file("X", "R", "x", "1"), file("B", "R", "b", "1")], 0, &delta_link("link-0"));
    let x = listed(&mut store, "X").local_item_id;
    let fetch_began_at_count = count(&mut store);
    confirm(&mut store, x, version(ROOT, "x", Some("22")), Some("e-X-22"));
    let confirmed = item(&mut store, x);

    let page = [file("X", "R", "x", "333"), file("B", "R", "b", "22")];
    let outcome = write_page(&mut store, &page, fetch_began_at_count, &delta_link("link-1"));
    assert_eq!(outcome.changes, [ListedChangeOutcome::Outdated, ListedChangeOutcome::Written]);
    assert_eq!((ids(outcome.outdated_items.clone()), outcome.is_link_written()), (vec!["X".to_owned()], false));
    assert_eq!(item(&mut store, x), confirmed);
    assert_eq!(listed(&mut store, "B").onedrive, version(ROOT, "b", Some("22")), "the rest of the page is written");
    assert_eq!(store.delta_link().unwrap().as_deref(), Some("link-0"), "the link of a page that left something to read again is not written");
    let not_ended = Some(ListingUnderWay { kind: ListingKind::Delta, began_at_count: fetch_began_at_count, next_link: None });
    assert_eq!(under_way(&mut store), not_ended);
    drop(store);

    let mut store = TreeStore::open(&path).unwrap();
    assert_eq!((store.delta_link().unwrap().as_deref(), under_way(&mut store)), (Some("link-0"), not_ended));
    let later = count(&mut store);
    let outcome = write_page(&mut store, &page, later, &delta_link("link-1"));
    assert_eq!((outcome.changes.clone(), outcome.is_link_written()), (vec![ListedChangeOutcome::Written; 2], true));
    let row = listed(&mut store, "X");
    assert_eq!((row.onedrive, row.onedrive_etag.as_deref()), (version(ROOT, "x", Some("333")), Some("e-X-333")));
    assert_eq!((store.delta_link().unwrap().as_deref(), under_way(&mut store)), (Some("link-1"), None));
}

/// The other end of the same order: what the caller read again is given with the link of
/// the page that left it, and that call, which leaves nothing, writes the link. So for an
/// item a page says is gone, and for a page a listing goes on after.
#[test]
fn what_was_read_again_is_given_with_the_same_link_which_is_then_written() {
    let mut store = listed_store();
    write_fetched_at(&mut store, &[file("X", "R", "x", "1")], 0, &delta_link("link-1"));
    let x = listed(&mut store, "X").local_item_id;
    let fetch_began_at_count = count(&mut store);
    confirm(&mut store, x, version(ROOT, "x", Some("22")), Some("e-X-22"));

    let outcome = write_page(&mut store, &[gone("X")], fetch_began_at_count, &delta_link("link-2"));
    assert_eq!((outcome.changes.clone(), ids(outcome.outdated_items.clone())), (vec![ListedChangeOutcome::Outdated], vec!["X".to_owned()]));
    assert_eq!(store.delta_link().unwrap().as_deref(), Some("link-1"));
    let later = count(&mut store);
    assert!(write_page(&mut store, &[file("X", "R", "x", "333")], later, &delta_link("link-2")).is_link_written());
    assert_eq!((store.delta_link().unwrap().as_deref(), under_way(&mut store)), (Some("link-2"), None));
    assert_eq!(listed(&mut store, "X").onedrive, version(ROOT, "x", Some("333")));

    confirm(&mut store, x, version(ROOT, "x", Some("4444")), Some("e-X-4444"));
    assert!(!write_page(&mut store, &[file("X", "R", "x", "333")], later, &next_link("next-1")).is_link_written());
    assert_eq!(under_way(&mut store), Some(ListingUnderWay { kind: ListingKind::Delta, began_at_count: later, next_link: None }));
    let read_at = count(&mut store);
    assert!(write_page(&mut store, &[file("X", "R", "x", "4444")], read_at, &next_link("next-1")).is_link_written());
    assert_eq!(under_way(&mut store), Some(ListingUnderWay { kind: ListingKind::Delta, began_at_count: later, next_link: Some("next-1".to_owned()) }));
    assert_eq!(store.delta_link().unwrap().as_deref(), Some("link-2"), "a link to the next page ends no listing");
}

/// S3 of the review: a full listing is resumed after a stop, and its remaining pages come
/// with a later count. A new file a step sent, confirmed after the listing had passed its
/// place, was not seen and is not taken: the sweep is bounded by the count the listing
/// began at, which the store kept, and the file is read again.
#[test]
fn a_full_listing_resumed_with_a_later_count_does_not_take_what_was_confirmed_since_it_began() {
    let mut store = listed_store();
    write_fetched_at(&mut store, &[file("A", "R", "a", "1")], 0, &delta_link("link-1"));
    let began_at_count = store.item_store().begin_full_listing().unwrap();
    write_fetched_at(&mut store, &[root(), file("A", "R", "a", "1")], began_at_count, &next_link("next-1"));
    let sent = make_file(&mut store, ROOT, "n");
    store.item_store().bind_item(sent, &reference("N")).unwrap();
    confirm(&mut store, sent, version(ROOT, "n", Some("1")), Some("e-N-1"));
    assert_eq!(under_way(&mut store), Some(ListingUnderWay { kind: ListingKind::Full, began_at_count, next_link: Some("next-1".to_owned()) }));

    let later = count(&mut store);
    assert!(later > began_at_count);
    let outcome = write_page(&mut store, &[], later, &delta_link("link-2"));
    assert_eq!((ids(outcome.outdated_items.clone()), outcome.is_link_written()), (vec!["N".to_owned()], false));
    assert_eq!(item(&mut store, sent).onedrive, version(ROOT, "n", Some("1")));
    assert_eq!(store.delta_link().unwrap(), None);

    assert!(write_page(&mut store, &[file("N", "R", "n", "1")], later, &delta_link("link-2")).is_link_written());
    assert_eq!(item(&mut store, sent).onedrive, version(ROOT, "n", Some("1")));
    assert_eq!((store.delta_link().unwrap().as_deref(), under_way(&mut store)), (Some("link-2"), None));
    assert_eq!(onedrive_side(&mut store).keys().map(String::as_str).collect::<Vec<_>>(), ["A", "N", "R"]);
}

/// Whether a listing is under way, a case each: none in a new store; a full one from its
/// beginning, with the count it began at; with the link to its next page once a page gave
/// one; none after the page with the delta link; and a listing of changes between its
/// pages.
#[test]
fn the_store_says_whether_a_listing_is_under_way_and_where_it_goes_on() {
    let mut store = TreeStore::in_memory().unwrap();
    store.set_drive_id("d1").unwrap();
    assert_eq!(under_way(&mut store), None);
    let began_at_count = store.item_store().begin_full_listing().unwrap();
    assert_eq!(under_way(&mut store), Some(ListingUnderWay { kind: ListingKind::Full, began_at_count, next_link: None }));
    write_fetched_at(&mut store, &[root()], 0, &next_link("next-1"));
    assert_eq!(under_way(&mut store), Some(ListingUnderWay { kind: ListingKind::Full, began_at_count, next_link: Some("next-1".to_owned()) }));
    write_fetched_at(&mut store, &[], 0, &delta_link("link-0"));
    assert_eq!(under_way(&mut store), None);

    write_fetched_at(&mut store, &[file("A", "R", "a", "1")], 0, &next_link("next-2"));
    assert_eq!(under_way(&mut store), Some(ListingUnderWay { kind: ListingKind::Delta, began_at_count: 0, next_link: Some("next-2".to_owned()) }));
    assert_eq!(store.delta_link().unwrap().as_deref(), Some("link-0"));
    write_fetched_at(&mut store, &[], 0, &delta_link("link-1"));
    assert_eq!((under_way(&mut store), store.delta_link().unwrap().as_deref()), (None, Some("link-1")));
}

/// One listing at a time is the caller's rule, and the entry holds what it can of it: a
/// page is refused whole, with nothing written, in a store that does not know its drive;
/// when no listing is under way and the store has no delta link, so that what is listed
/// can only be a full listing that was not begun; when its fetch is said to have begun at a
/// count the store has not reached; and when its fetch began before the listing under way
/// did, as a page of an older listing has.
#[test]
fn a_page_the_entry_cannot_write_rightly_is_refused_whole() {
    let page = [root(), file("A", "R", "a", "1")];
    let mut no_drive = TreeStore::in_memory().unwrap();
    assert!(is_refused(no_drive.item_store().write_listed_changes(&page, 0, &delta_link("link-1"))));
    assert_eq!((no_drive.delta_link().unwrap(), every_row(&mut no_drive).len()), (None, 1));

    let mut not_begun = TreeStore::in_memory().unwrap();
    not_begun.set_drive_id("d1").unwrap();
    assert!(is_refused(not_begun.item_store().write_listed_changes(&page, 0, &next_link("next-1"))));
    assert_eq!((under_way(&mut not_begun), every_row(&mut not_begun).len()), (None, 1));

    let mut store = listed_store();
    assert!(is_refused(store.item_store().write_listed_changes(&page, 1, &next_link("next-1"))), "the store counted no confirmation yet");
    write(&mut store, &[file("B", "R", "b", "1")]);
    let b = listed(&mut store, "B").local_item_id;
    let older = count(&mut store);
    confirm(&mut store, b, version(ROOT, "b", Some("22")), Some("e-B-22"));
    let began_at_count = store.item_store().begin_full_listing().unwrap();
    let begun = under_way(&mut store);
    assert!(older < began_at_count);
    assert!(is_refused(store.item_store().write_listed_changes(&page, older, &delta_link("link-1"))), "a page fetched before the listing under way began");
    assert_eq!((under_way(&mut store), store.delta_link().unwrap(), bound(&mut store, "A")), (begun, None, None));
}

/// A second full listing gives up the first: what only the first saw is not seen, its link
/// to a next page is forgotten, and the count the listing began at is the second's.
#[test]
fn a_second_full_listing_gives_up_the_first() {
    let mut store = listed_store();
    write_fetched_at(&mut store, &[file("A", "R", "a", "1"), file("B", "R", "b", "1")], 0, &delta_link("link-1"));
    let a = listed(&mut store, "A").local_item_id;
    store.item_store().begin_full_listing().unwrap();
    write_fetched_at(&mut store, &[root(), file("A", "R", "a", "1")], 0, &next_link("next-1"));
    confirm(&mut store, a, version(ROOT, "a", Some("1")), Some("e-A-1"));

    let began_at_count = store.item_store().begin_full_listing().unwrap();
    assert_eq!(under_way(&mut store), Some(ListingUnderWay { kind: ListingKind::Full, began_at_count, next_link: None }));
    assert_eq!(began_at_count, 1);
    write_fetched_at(&mut store, &[root(), file("B", "R", "b", "1")], began_at_count, &delta_link("link-2"));
    assert_eq!(item(&mut store, a).onedrive, None, "A was seen by the listing that was given up, and not by this one");
    assert_eq!(listed(&mut store, "B").onedrive, version(ROOT, "b", Some("1")));
}

/// While a full listing is under way every page is its page, also one that brings what was
/// read again, with a later count: what it says OneDrive has is seen by the listing and is
/// not taken at its end. The store has no delta link then, so that no listing of changes
/// can be asked beside it.
#[test]
fn what_is_read_again_while_a_full_listing_is_under_way_is_seen_by_it() {
    let mut store = listed_store();
    write_fetched_at(&mut store, &[file("A", "R", "a", "1"), file("B", "R", "b", "1")], 0, &delta_link("link-1"));
    let a = listed(&mut store, "A").local_item_id;
    let began_at_count = store.item_store().begin_full_listing().unwrap();
    confirm(&mut store, a, version(ROOT, "a", Some("22")), Some("e-A-22"));
    assert_eq!(write_fetched_at(&mut store, &[root(), file("A", "R", "a", "1"), file("B", "R", "b", "1")], began_at_count, &next_link("next-1")), ["A"]);
    assert_eq!(store.delta_link().unwrap(), None);

    let later = count(&mut store);
    assert_eq!(write_fetched_at(&mut store, &[file("A", "R", "a", "22")], later, &next_link("next-1")), Vec::<String>::new());
    assert_eq!(write_fetched_at(&mut store, &[], later, &delta_link("link-2")), Vec::<String>::new());
    assert_eq!(onedrive_side(&mut store).keys().map(String::as_str).collect::<Vec<_>>(), ["A", "B", "R"]);
    assert_eq!(listed(&mut store, "A").onedrive, version(ROOT, "a", Some("22")));
}

/// A directory is listed gone on one page, and a later page of the same listing says a
/// file that was in it is at the root: between the two the rows say OneDrive does not have
/// the file, and the store says a listing is under way; after the second they say where it
/// is. A file the folder shows is the same item throughout.
#[test]
fn a_child_moved_out_of_a_directory_listed_gone_has_its_version_again_on_a_later_page() {
    let mut store = listed_store();
    let kept = make_file(&mut store, ROOT, "kept");
    synced(&mut store, kept, "K");
    write_fetched_at(&mut store, &[directory("D", "R", "D"), file("X", "D", "x", "1"), file("K", "D", "kept", "1")], 0, &delta_link("link-1"));

    assert_eq!(write_fetched_at(&mut store, &[gone("D")], 0, &next_link("next-1")), Vec::<String>::new());
    assert_eq!((bound(&mut store, "X"), item(&mut store, kept).onedrive), (None, None));
    assert!(under_way(&mut store).is_some(), "what the rows say is gone may be on a page still to come");

    write_fetched_at(&mut store, &[file("X", "R", "x", "1"), file("K", "R", "kept", "1")], 0, &delta_link("link-2"));
    assert_eq!(under_way(&mut store), None);
    assert_eq!(onedrive_place(&mut store, "X"), Some(place(ROOT, "x")));
    assert_eq!(listed(&mut store, "K").local_item_id, kept);
    assert_eq!(item(&mut store, kept).onedrive, version(ROOT, "kept", Some("1")));
}

/// A full listing that begins forgets the delta link, in its own transaction: while one is
/// under way the store has no link to ask a listing of changes from, so none can be asked
/// whose end would take what the full listing has not seen yet.
#[test]
fn a_full_listing_that_begins_forgets_the_delta_link() {
    let mut store = listed_store();
    write_fetched_at(&mut store, &[file("A", "R", "a", "1")], 0, &delta_link("link-1"));
    assert_eq!(store.delta_link().unwrap().as_deref(), Some("link-1"));
    store.item_store().begin_full_listing().unwrap();
    assert_eq!(store.delta_link().unwrap(), None);
}

/// A full listing that is stopped goes on in a store opened again, from the link to its
/// next page: what it saw before the stop is seen.
#[test]
fn a_full_listing_goes_on_in_a_store_opened_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.sqlite");
    let mut store = TreeStore::open(&path).unwrap();
    store.set_drive_id("d1").unwrap();
    store.item_store().begin_full_listing().unwrap();
    write_fetched_at(&mut store, &[root(), file("A", "R", "a", "1"), file("B", "R", "b", "1")], 0, &delta_link("link-0"));
    let began_at_count = store.item_store().begin_full_listing().unwrap();
    write_fetched_at(&mut store, &[root(), file("A", "R", "a", "1")], 0, &next_link("next-1"));
    drop(store);

    let mut store = TreeStore::open(&path).unwrap();
    assert_eq!(under_way(&mut store), Some(ListingUnderWay { kind: ListingKind::Full, began_at_count, next_link: Some("next-1".to_owned()) }));
    write_fetched_at(&mut store, &[], 0, &delta_link("link-1"));
    assert_eq!(onedrive_side(&mut store).keys().map(String::as_str).collect::<Vec<_>>(), ["A", "R"]);
}

/// The link the next listing is asked from is written with the answer that gave it, in
/// one transaction; a link to a next page leaves it as it was.
#[test]
fn the_delta_link_is_written_with_the_answer_that_gave_it() {
    let mut store = listed_store();
    write(&mut store, &[file("A", "R", "a", "1")]);
    assert_eq!(store.delta_link().unwrap().as_deref(), Some("link-0"));
    write_fetched_at(&mut store, &[file("B", "R", "b", "1")], 0, &delta_link("link-1"));
    assert_eq!(store.delta_link().unwrap().as_deref(), Some("link-1"));
}

/// A directory `D` with a hundred directories in it and 9 900 files in those, as a listing
/// gives them.
fn ten_thousand_below() -> Vec<Change> {
    let mut tree = vec![directory("D", "R", "D")];
    tree.extend((0..100).map(|n| directory(&format!("S{n}"), "D", &format!("s{n}"))));
    tree.extend((0..9900).map(|n| file(&format!("F{n}"), &format!("S{}", n % 100), &format!("f{n}"), "1")));
    tree
}

/// A directory with ten thousand items below it listed gone takes them all, whatever their
/// number, but the one item among them that was confirmed after the fetch began: it keeps
/// its version and is answered to be read again. How long the page took is printed.
#[test]
fn a_directory_with_ten_thousand_items_below_it_listed_gone_takes_all_but_what_was_confirmed_since() {
    let mut store = listed_store();
    write(&mut store, &ten_thousand_below());
    let (guarded, its_directory) = (listed(&mut store, "F5050").local_item_id, listed(&mut store, "S50").local_item_id);
    let fetch_began_at_count = count(&mut store);
    confirm(&mut store, guarded, version(its_directory, "f5050", Some("22")), Some("e-F5050-22"));

    let began = std::time::Instant::now();
    let read_again = write_fetched_at(&mut store, &[gone("D")], fetch_began_at_count, &next_link("next"));
    println!("a directory with ten thousand items below it listed gone: the page took {:?}", began.elapsed());
    assert_eq!(read_again, ["F5050"]);
    assert_eq!(item(&mut store, guarded).onedrive, version(its_directory, "f5050", Some("22")));
    let left: Vec<String> = every_row(&mut store).into_iter().filter_map(|row| row.onedrive_item).map(|reference| reference.item_id.as_str().to_owned()).collect();
    assert_eq!(left, ["R", "S50", "F5050"], "a row stays while a row names it as its parent, and no other");
    assert_eq!(listed(&mut store, "S50").onedrive, None);
}

/// A full listing's end takes what it did not see, whatever its number, but what was
/// confirmed after the listing began. How long the page took is printed.
#[test]
fn a_full_listing_that_did_not_see_ten_thousand_items_takes_all_but_what_was_confirmed_since() {
    let mut store = listed_store();
    write_fetched_at(&mut store, &ten_thousand_below(), 0, &delta_link("link-0"));
    let (guarded, its_directory) = (listed(&mut store, "F5050").local_item_id, listed(&mut store, "S50").local_item_id);
    let fetch_began_at_count = store.item_store().begin_full_listing().unwrap();
    confirm(&mut store, guarded, version(its_directory, "f5050", Some("22")), Some("e-F5050-22"));

    let began = std::time::Instant::now();
    let read_again = write_fetched_at(&mut store, &[root()], fetch_began_at_count, &delta_link("link-1"));
    println!("a full listing that did not see ten thousand items: its last page took {:?}", began.elapsed());
    assert_eq!(read_again, ["F5050"]);
    let left: Vec<String> = every_row(&mut store).into_iter().filter_map(|row| row.onedrive_item).map(|reference| reference.item_id.as_str().to_owned()).collect();
    assert_eq!(left, ["R", "S50", "F5050"]);
    assert_eq!(item(&mut store, guarded).onedrive, version(its_directory, "f5050", Some("22")));
}

/// The two sides agree on what OneDrive has of the item of OneDrive `onedrive_id`.
fn agree(store: &mut TreeStore, onedrive_id: &str) -> LocalItemId {
    let mut row = listed(store, onedrive_id);
    row.synced_place = row.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone());
    row.synced_content = row.onedrive.as_ref().and_then(|onedrive| onedrive.content.clone());
    write_item(&store.conn, &row).unwrap();
    row.local_item_id
}

/// The items whose OneDrive version is not the synced one, a case each: new in OneDrive,
/// moved there, of another content there, and removed there are among them; one the two
/// sides agree on, the root, and a parent that was never listed are not. They are read a
/// bounded number at a time, by local item id.
#[test]
fn the_items_onedrive_changed_are_those_whose_onedrive_version_is_not_the_synced_one() {
    let mut store = listed_store();
    write(&mut store, &[directory("D", "R", "D"), file("S", "D", "same", "1"), file("M", "D", "moved", "1"), file("C", "D", "changed", "1"), file("G", "D", "gone", "1")]);
    for id in ["D", "S", "M", "C", "G"] {
        agree(&mut store, id);
    }
    let changed = |store: &mut TreeStore| -> Vec<String> {
        let items = store.item_store().list_changed_in_onedrive(None, 100).unwrap();
        items.into_iter().map(|item| item.onedrive_item.unwrap().item_id.as_str().to_owned()).collect()
    };
    assert_eq!(changed(&mut store), Vec::<String>::new());

    write(&mut store, &[file("M", "R", "moved", "1"), file("C", "D", "changed", "22"), gone("G"), file("N", "U", "new", "1")]);
    assert!(bound(&mut store, "U").is_some(), "a parent that was never listed has a row");
    assert_eq!(changed(&mut store), ["M", "C", "G", "N"]);

    let first = store.item_store().list_changed_in_onedrive(None, 3).unwrap();
    assert_eq!(first.len(), 3);
    let rest = store.item_store().list_changed_in_onedrive(Some(first[2].local_item_id), 3).unwrap();
    assert_eq!(rest.iter().map(|item| item.onedrive_item.clone().unwrap()).collect::<Vec<_>>(), [reference("N")]);

    for id in ["M", "C", "N"] {
        agree(&mut store, id);
    }
    assert_eq!(changed(&mut store), ["G"]);
}

/// The items OneDrive has in a directory under one name as OneDrive compares names: those
/// whose names fold to the same. The folded name is written with the name a listing gives,
/// follows a listed rename, and goes with the version of an item listed gone.
#[test]
fn the_items_of_one_folded_name_in_a_directory_of_onedrive_are_found() {
    let mut store = listed_store();
    write(&mut store, &[directory("D", "R", "Docs"), file("A", "D", "Report.TXT", "1"), file("B", "D", "report.txt", "1"), file("C", "R", "REPORT.txt", "1"), file("E", "D", "other", "1")]);
    let d = listed(&mut store, "D").local_item_id;
    let found = |store: &mut TreeStore, directory: LocalItemId, name: &str| -> Vec<String> {
        let items = store.item_store().list_children_in_onedrive_with_folded_name(directory, &crate::FoldedName::of(std::ffi::OsStr::new(name))).unwrap();
        items.into_iter().map(|item| item.onedrive_item.unwrap().item_id.as_str().to_owned()).collect()
    };
    assert_eq!(found(&mut store, d, "REPORT.TXT"), ["A", "B"]);
    assert_eq!(found(&mut store, ROOT, "report.txt"), ["C"]);
    assert_eq!(found(&mut store, ROOT, "DOCS"), ["D"]);

    write(&mut store, &[file("A", "D", "E\u{301}t\u{e9}", "1"), gone("B")]);
    assert_eq!((found(&mut store, d, "report.txt"), found(&mut store, d, "\u{c9}TE\u{301}")), (Vec::<String>::new(), vec!["A".to_owned()]));
}

/// What OneDrive has in a directory, by name, and what it has in it and below it: as the
/// rows say, whatever the folder shows there.
#[test]
fn what_onedrive_has_in_a_directory_and_below_it_is_read_from_the_rows() {
    let mut store = listed_store();
    write(&mut store, &[directory("D", "R", "D"), directory("E", "D", "E"), file("B", "D", "b", "1"), file("A", "D", "a", "1"), file("C", "E", "c", "1"), file("O", "R", "outside", "1")]);
    make_file(&mut store, ROOT, "made-here");
    let (d, e) = (listed(&mut store, "D").local_item_id, listed(&mut store, "E").local_item_id);
    let ids = |items: Vec<Item>| -> Vec<String> { items.into_iter().map(|item| item.onedrive_item.unwrap().item_id.as_str().to_owned()).collect() };

    assert_eq!(ids(store.item_store().list_children_in_onedrive(d).unwrap()), ["E", "A", "B"]);
    assert_eq!(ids(store.item_store().list_children_in_onedrive(ROOT).unwrap()), ["D", "O"]);
    assert_eq!(ids(store.item_store().list_children_in_onedrive(e).unwrap()), ["C"]);
    let mut below = ids(store.item_store().list_below_in_onedrive(d).unwrap());
    below.sort();
    assert_eq!(below, ["A", "B", "C", "E"]);
    assert_eq!(store.item_store().list_below_in_onedrive(e).unwrap().len(), 1);
    assert_eq!(store.item_store().list_below_in_onedrive(LocalItemId::new(999)).unwrap(), Vec::new());
}

/// Whether the folder can show an item as OneDrive has it, a case each. It can: its own
/// placement and that of every directory above it say so, up to the root; and the root. It
/// cannot: its own placement says no, or that of a directory above it, also one whose own
/// directory was never listed. Not known yet: a directory above it was never listed, or the
/// rows above it name each other in a circle. Not in OneDrive: an item made here, and a row
/// that is a binding alone. An id with no row is refused.
#[test]
fn whether_the_folder_can_show_an_item_is_one_of_four_cases() {
    use FolderVisibility::{CanBeShown, CannotBeShown, NotInOneDrive, NotKnownYet};
    let mut store = listed_store();
    let skipped = |change: Change, reason: SkipReason| {
        let Change::Upsert(row) = change else { unreachable!() };
        Change::Upsert(Row { placement: Placement::Skipped(reason), ..row })
    };
    write(
        &mut store,
        &[
            directory("D", "R", "D"),
            directory("E", "D", "E"),
            file("A", "E", "a", "1"),
            skipped(file("L", "D", "long", "1"), SkipReason::NameTooLong),
            skipped(directory("V", "R", "Vault"), SkipReason::PersonalVault),
            directory("W", "V", "W"),
            file("B", "W", "b", "1"),
            file("X", "U", "x", "1"),
            skipped(directory("S", "U", "Shared"), SkipReason::Shared),
            file("Z", "S", "z", "1"),
            directory("P", "Q", "P"),
            directory("Q", "P", "Q"),
            file("Y", "Q", "y", "1"),
        ],
    );
    let here = make_file(&mut store, ROOT, "made-here");
    for (id, visibility) in [
        ("D", CanBeShown),
        ("A", CanBeShown),
        ("L", CannotBeShown),
        ("V", CannotBeShown),
        ("W", CannotBeShown),
        ("B", CannotBeShown),
        ("S", CannotBeShown),
        ("Z", CannotBeShown),
        ("X", NotKnownYet),
        ("P", NotKnownYet),
        ("Y", NotKnownYet),
        ("U", NotInOneDrive),
    ] {
        let item = listed(&mut store, id).local_item_id;
        assert_eq!(store.item_store().find_folder_visibility(item).unwrap(), visibility, "{id}");
    }
    assert_eq!(store.item_store().find_folder_visibility(ROOT).unwrap(), CanBeShown);
    assert_eq!(store.item_store().find_folder_visibility(here).unwrap(), NotInOneDrive);
    assert!(is_refused(store.item_store().find_folder_visibility(LocalItemId::new(999))));
}

/// A full listing of `count` items in pages of 200 into `store`: a directory for every
/// hundred files, the files of a directory after it. How long each page took to write.
fn list_in_full(store: &mut TreeStore, count: usize) -> Vec<std::time::Duration> {
    let mut items = Vec::with_capacity(count + 1);
    items.push(root());
    for n in 0..count {
        let directory_id = format!("D{}", n / 100);
        if n % 100 == 0 {
            items.push(directory(&directory_id, "R", &directory_id));
        } else {
            items.push(file(&format!("F{n}"), &directory_id, &format!("f{n}"), "1"));
        }
    }
    store.item_store().begin_full_listing().unwrap();
    let pages: Vec<&[Change]> = items.chunks(200).collect();
    let mut took = Vec::new();
    for (at, page) in pages.iter().enumerate() {
        let link = if at + 1 == pages.len() { delta_link("link") } else { next_link("next") };
        let began = std::time::Instant::now();
        store.item_store().write_listed_changes(page, 0, &link).unwrap();
        took.push(began.elapsed());
    }
    took
}

/// A page of 200 changes over what [`list_in_full`] listed: a hundred files moved to
/// another directory with another content, fifty gone, fifty new in a new directory.
fn page_of_changes() -> Vec<Change> {
    let mut page = vec![directory("NEW", "R", "new")];
    page.extend((0..100).map(|n| file(&format!("F{}", n * 7 + 1), "D3", &format!("f{}", n * 7 + 1), "22")));
    page.extend((0..50).map(|n| gone(&format!("F{}", n * 7 + 2))));
    page.extend((0..49).map(|n| file(&format!("G{n}"), "NEW", &format!("g{n}"), "1")));
    page
}

/// The cost of a page, measured in the build the tests run in and printed: a full listing
/// of 100 000 items in pages of 200, and a page of 200 changes over 1 000 rows and over
/// 100 000, in memory and in a store on disk. Run by hand, once:
/// `cargo test -p konedrive-tree a_page_costs -- --ignored --nocapture`.
/// What it holds: a page over a hundred times the rows does not cost ten times as much.
#[test]
#[ignore = "a measurement: it lists 100 000 items three times"]
fn a_page_costs_the_same_whatever_the_store_holds() {
    let mean = |took: &[std::time::Duration]| took.iter().sum::<std::time::Duration>() / took.len() as u32;
    let page = page_of_changes();
    let measure = |what: &str, small: &mut TreeStore, large: &mut TreeStore| {
        small.set_drive_id("d1").unwrap();
        large.set_drive_id("d1").unwrap();
        list_in_full(small, 1_000);
        let listing = list_in_full(large, 100_000);
        println!("{what}: a full listing of 100 000 items in {} pages of 200 took {:?}; a page took {:?} on average over the first fifty and {:?} over the last fifty", listing.len(), listing.iter().sum::<std::time::Duration>(), mean(&listing[..50]), mean(&listing[listing.len() - 50..]));
        let mut took = Vec::new();
        for store in [small, large] {
            let fetch_began_at_count = count(store);
            let began = std::time::Instant::now();
            store.item_store().write_listed_changes(&page, fetch_began_at_count, &delta_link("link-2")).unwrap();
            took.push(began.elapsed());
        }
        println!("{what}: a page of {} changes took {:?} over 1 000 rows and {:?} over 100 000", page.len(), took[0], took[1]);
        assert!(took[1] < took[0] * 10, "{what}: the cost of a page grew with the rows of the store");
        assert!(mean(&listing[listing.len() - 50..]) < mean(&listing[..50]) * 10, "{what}: the pages of a full listing grew dearer as the store filled");
    };
    measure("in memory", &mut TreeStore::in_memory().unwrap(), &mut TreeStore::in_memory().unwrap());
    // On disk, below the build's own directory.
    let dir = tempfile::tempdir_in(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target")).unwrap();
    measure("on disk", &mut TreeStore::open(&dir.path().join("small.sqlite")).unwrap(), &mut TreeStore::open(&dir.path().join("large.sqlite")).unwrap());
}

/// A row a page leaves to be read again, against all else a page or its end can do.
mod outdated_row;
