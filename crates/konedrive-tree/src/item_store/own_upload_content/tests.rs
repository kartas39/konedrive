use super::*;
use crate::item_store::tests::listed_drive::{listed_file, store_with_listed_root, write_listing_of_changes};
use crate::item_store::tests::shown_items::{listed_content, row, show_listed_file};
use crate::item_store::tests::{is_refused, make_file};
use crate::model::{ContentPresence, LocalVersionChange};
use crate::sync_plan::ItemVersions;
use crate::TreeStore;

/// The content OneDrive has after an upload it rewrote: another than `sent`.
fn left_by_upload() -> ContentVersion {
    ContentVersion::Hashed { size: 11, mtime: 300, mtime_nsec: 0, hash: Some("rewritten".into()), ctag: Some("c-rewritten".into()) }
}

fn sent() -> ContentVersion {
    ContentVersion::ChangedHere { size: 9, mtime: 300, mtime_nsec: 0, change_count: 1 }
}

fn fact() -> OwnUploadContent {
    OwnUploadContent { onedrive_content: left_by_upload(), local_content: sent() }
}

fn find(store: &mut TreeStore, item: LocalItemId) -> Option<OwnUploadContent> {
    store.item_store().reader().find_own_upload_content(item).unwrap()
}

/// OneDrive has `content` of the file `F` in the root, by a listing.
fn list_content(store: &mut TreeStore, content: &ContentVersion) {
    let ContentVersion::Hashed { size, hash: Some(hash), .. } = content else { panic!("a content as OneDrive tells one") };
    write_listing_of_changes(store, &[crate::model::Change::Upsert(match listed_file("F", "R", "f", (*size, hash), "e-F-2") {
        crate::model::Change::Upsert(listed) => crate::model::Row { mtime: content.mtime().0, ..listed },
        _ => unreachable!(),
    })]);
}

/// The file `F`, shown as `f`, changed here, whose upload OneDrive answered with
/// [`left_by_upload`]: the row's OneDrive content is that one, and the fact is remembered.
fn store_with_own_upload() -> (TreeStore, LocalItemId) {
    let mut store = store_with_listed_root();
    let file = show_listed_file(&mut store, "F", "R", "f", "h1");
    store.item_store().set_content_presence(file, ContentPresence::OnDisk).unwrap();
    store.item_store().apply_all(&[LocalVersionChange::OpenedForWriting { item: file }, LocalVersionChange::ClosedChanged { item: file, size: 9, mtime: 300, mtime_nsec: 0 }]).unwrap();
    list_content(&mut store, &left_by_upload());
    store.item_store().remember_own_upload_content(file, &fact()).unwrap();
    (store, file)
}

#[test]
fn the_fact_is_answered_while_onedrives_content_is_the_one_the_upload_left() {
    let (mut store, file) = store_with_own_upload();
    assert_eq!(row(&mut store, file).onedrive.unwrap().content, Some(left_by_upload()));
    assert_eq!(find(&mut store, file), Some(fact()));

    // Remembered again, of a later upload: the last one counts.
    let later = OwnUploadContent { local_content: ContentVersion::ChangedHere { size: 9, mtime: 400, mtime_nsec: 0, change_count: 2 }, ..fact() };
    store.item_store().remember_own_upload_content(file, &later).unwrap();
    assert_eq!(find(&mut store, file), Some(later));

    let other = make_file(&mut store, LocalItemId::ROOT, "made");
    assert_eq!(find(&mut store, other), None);
    assert!(is_refused(store.item_store().remember_own_upload_content(other, &fact())), "an item OneDrive does not have was not uploaded into");
    assert!(is_refused(store.item_store().remember_own_upload_content(LocalItemId::new(999), &fact())));
}

#[test]
fn the_fact_is_not_answered_once_a_listing_wrote_another_content() {
    let (mut store, file) = store_with_own_upload();
    list_content(&mut store, &listed_content("h7"));
    assert_eq!(find(&mut store, file), None, "OneDrive's content is somebody's change by now");
    list_content(&mut store, &left_by_upload());
    assert_eq!(find(&mut store, file), Some(fact()), "and is the upload's again when the row says that content again");
}

#[test]
fn the_fact_goes_with_the_items_row_and_stays_with_the_item_when_its_binding_moves() {
    // The binding moves: a program saves another file over this one. The fact is of the
    // file that was uploaded, whose row goes, and not of the file that has the binding now.
    let (mut store, file) = store_with_own_upload();
    let replacing = make_file(&mut store, LocalItemId::ROOT, "f.tmp");
    store.item_store().apply(&LocalVersionChange::SavedOver { replacing, replaced: file }).unwrap();
    assert_eq!(store.item_store().find_item(file).unwrap(), None);
    assert_eq!(row(&mut store, replacing).onedrive.unwrap().content, Some(left_by_upload()));
    assert_eq!(find(&mut store, replacing), None);
    assert_eq!(store.conn.query_row("SELECT COUNT(*) FROM own_upload_contents", [], |row| row.get::<_, i64>(0)).unwrap(), 0, "it went with the row");

    // The item gives its binding up and keeps its row: the fact stays with it, and is not
    // answered while the row has no content of OneDrive's.
    let (mut store, file) = store_with_own_upload();
    let planned = ItemVersions::of(&row(&mut store, file));
    store.item_store().commit_give_up_binding(&planned, false).unwrap();
    assert!(row(&mut store, file).onedrive.is_none());
    assert_eq!(find(&mut store, file), None);
    assert_eq!(store.conn.query_row("SELECT COUNT(*) FROM own_upload_contents WHERE local_item_id = ?1", [file.as_i64()], |row| row.get::<_, i64>(0)).unwrap(), 1);
}
