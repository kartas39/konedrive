use std::path::{Path, PathBuf};

use super::*;
use crate::item_store::tests::listed_drive::{listed_file, store_with_listed_root, write_listing_of_changes};
use crate::item_store::tests::shown_items::{listed_content, row, show, show_listed_directory};
use crate::item_store::tests::{make_file, place};
use crate::model::{Change, LocalVersionChange, Row};
use crate::TreeStore;

/// A listing names the file `item_id` in `parent` under `name`, with the content of `hash`
/// and the MIME type `mime`.
fn list_file_of_type(store: &mut TreeStore, item_id: &str, parent: &str, name: &str, hash: &str, mime: Option<&str>) {
    let Change::Upsert(listed) = listed_file(item_id, parent, name, (3, hash), &format!("e-{item_id}-{hash}")) else { unreachable!() };
    write_listing_of_changes(store, &[Change::Upsert(Row { mime: mime.map(str::to_owned), ..listed })]);
}

fn show_file_of_type(store: &mut TreeStore, item_id: &str, parent: &str, name: &str, mime: Option<&str>) -> LocalItemId {
    list_file_of_type(store, item_id, parent, name, "h1", mime);
    show(store, item_id)
}

fn candidates(store: &mut TreeStore, after: Option<LocalItemId>, limit: usize) -> Vec<(LocalItemId, PathBuf)> {
    store.item_store().reader().list_thumbnail_candidates(after, limit).unwrap().into_iter().map(|candidate| (candidate.item.local_item_id, candidate.path)).collect()
}

fn marks(store: &mut TreeStore, after: Option<LocalItemId>, limit: usize) -> Vec<(LocalItemId, PathBuf)> {
    store.item_store().reader().list_thumbnail_marks(after, limit).unwrap().into_iter().map(|mark| (mark.item, mark.path)).collect()
}

/// A thumbnail of the content the listing gave (`h1`) was written for `item` at `path`.
fn mark_written(store: &mut TreeStore, item: LocalItemId, path: &str) {
    store.item_store().set_thumbnail_made(item, &listed_content("h1"), Path::new(path), true).unwrap();
}

#[test]
fn the_shown_images_and_videos_without_a_thumbnail_of_their_content_are_candidates() {
    let mut store = store_with_listed_root();
    show_listed_directory(&mut store, "D", "R", "d");
    let image = show_file_of_type(&mut store, "I", "D", "i.jpg", Some("image/jpeg"));
    show_file_of_type(&mut store, "T", "R", "t.txt", Some("text/plain"));
    show_file_of_type(&mut store, "U", "R", "unknown", None);
    let video = show_file_of_type(&mut store, "V", "R", "v.mp4", Some("video/mp4"));
    let second = show_file_of_type(&mut store, "J", "R", "j.png", Some("image/png"));
    let (image_path, video_path, second_path) = (PathBuf::from("d/i.jpg"), PathBuf::from("v.mp4"), PathBuf::from("j.png"));

    assert_eq!(candidates(&mut store, None, 10), [(image, image_path.clone()), (video, video_path.clone()), (second, second_path.clone())], "by local item id, each with its path");
    assert_eq!(candidates(&mut store, None, 2), [(image, image_path.clone()), (video, video_path.clone())]);
    assert_eq!(candidates(&mut store, Some(video), 2), [(second, second_path.clone())]);
    assert_eq!(store.item_store().reader().list_thumbnail_candidates(None, 1).unwrap()[0].item, row(&mut store, image));

    store.item_store().set_thumbnail_made(image, &listed_content("h1"), &image_path, true).unwrap();
    store.item_store().set_thumbnail_made(video, &listed_content("h1"), &video_path, false).unwrap();
    assert_eq!(candidates(&mut store, None, 10), [(second, second_path.clone())]);
    assert_eq!(candidates(&mut store, None, 1), [(second, second_path)], "the files that have theirs, or of which OneDrive has none, are passed over, however few are asked for");

    // A thumbnail of a content the row has no more is not marked.
    store.item_store().set_thumbnail_made(second, &listed_content("an-older-content"), Path::new("j.png"), true).unwrap();
    store.item_store().set_thumbnail_made(LocalItemId::new(999), &listed_content("h1"), Path::new("none"), true).unwrap();
    assert_eq!(candidates(&mut store, None, 10).len(), 1);
}

#[test]
fn a_file_whose_content_or_path_changed_since_its_thumbnail_is_a_candidate_again() {
    let mut store = store_with_listed_root();
    let directory = show_listed_directory(&mut store, "D", "R", "d");
    let image = show_file_of_type(&mut store, "I", "D", "i.jpg", Some("image/jpeg"));
    let other = show_file_of_type(&mut store, "J", "D", "j.jpg", Some("image/jpeg"));
    let mark = |store: &mut TreeStore, item, hash: &str, path: &str| store.item_store().set_thumbnail_made(item, &listed_content(hash), Path::new(path), true).unwrap();
    mark(&mut store, image, "h1", "d/i.jpg");
    mark(&mut store, other, "h1", "d/j.jpg");
    assert_eq!(candidates(&mut store, None, 10), []);

    // OneDrive has another content of the one.
    list_file_of_type(&mut store, "I", "D", "i.jpg", "h2", Some("image/jpeg"));
    assert_eq!(candidates(&mut store, None, 10), [(image, PathBuf::from("d/i.jpg"))]);
    mark(&mut store, image, "h2", "d/i.jpg");
    assert_eq!(candidates(&mut store, None, 10), []);

    // The directory above both is renamed: the cache names a thumbnail by the path.
    store.item_store().apply(&LocalVersionChange::Moved { item: directory, place: place(LocalItemId::ROOT, "renamed"), is_name_ignored: false }).unwrap();
    assert_eq!(candidates(&mut store, None, 10), [(image, PathBuf::from("renamed/i.jpg")), (other, PathBuf::from("renamed/j.jpg"))]);
    mark(&mut store, image, "h2", "renamed/i.jpg");
    assert_eq!(candidates(&mut store, None, 10), [(other, PathBuf::from("renamed/j.jpg"))]);
}

#[test]
fn a_file_the_folder_does_not_show_is_no_candidate() {
    let mut store = store_with_listed_root();
    list_file_of_type(&mut store, "N", "R", "never-shown.jpg", "h1", Some("image/jpeg"));
    let removed = show_file_of_type(&mut store, "G", "R", "removed.jpg", Some("image/jpeg"));
    store.item_store().set_thumbnail_made(removed, &listed_content("h1"), Path::new("removed.jpg"), true).unwrap();
    store.item_store().apply(&LocalVersionChange::Removed { item: removed }).unwrap();
    // A file made here, which OneDrive does not have: nothing tells its type.
    make_file(&mut store, LocalItemId::ROOT, "made.jpg");
    assert_eq!(candidates(&mut store, None, 10), []);

    // The mark goes with the item's row.
    let planned = crate::sync_plan::ItemVersions::of(&row(&mut store, removed));
    store.item_store().commit_send_file_removal(&planned).unwrap();
    write_listing_of_changes(&mut store, &[]);
    assert_eq!(store.item_store().find_item(removed).unwrap(), None);
    assert_eq!(store.conn.query_row("SELECT COUNT(*) FROM thumbnail_marks", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
}

#[test]
fn the_documents_a_thumbnail_is_made_for_are_candidates_and_plain_text_is_not() {
    let mut store = store_with_listed_root();
    let documents = [
        ("report.pdf", "application/pdf"),
        ("letter.docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
        ("sheet.xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
        ("text.odt", "application/vnd.oasis.opendocument.text"),
        ("print.ps", "application/postscript"),
        ("old.doc", "application/msword"),
        ("old.xls", "application/vnd.ms-excel"),
        ("old.ppt", "application/vnd.ms-powerpoint"),
        ("book.epub", "application/epub+zip"),
    ];
    let shown: Vec<(LocalItemId, PathBuf)> =
        documents.iter().enumerate().map(|(i, (name, mime))| (show_file_of_type(&mut store, &format!("D{i}"), "R", name, Some(mime)), PathBuf::from(name))).collect();
    for (i, (name, mime)) in [("notes.txt", "text/plain"), ("main.rs", "text/x-rust"), ("archive.zip", "application/zip"), ("blob", "application/octet-stream")].iter().enumerate() {
        show_file_of_type(&mut store, &format!("N{i}"), "R", name, Some(mime));
    }
    assert_eq!(candidates(&mut store, None, 20), shown, "each of the documents, and none of the rest");
}

#[test]
fn the_marks_that_stand_are_listed_in_batches_after_an_id() {
    let mut store = store_with_listed_root();
    let directory = show_listed_directory(&mut store, "D", "R", "d");
    let first = show_file_of_type(&mut store, "A", "R", "a.jpg", Some("image/jpeg"));
    let none_given = show_file_of_type(&mut store, "B", "R", "b.pdf", Some("application/pdf"));
    let second = show_file_of_type(&mut store, "C", "R", "c.pdf", Some("application/pdf"));
    let changed = show_file_of_type(&mut store, "E", "R", "e.jpg", Some("image/jpeg"));
    show_file_of_type(&mut store, "F", "R", "f.jpg", Some("image/jpeg"));
    let moved = show_file_of_type(&mut store, "G", "D", "g.jpg", Some("image/jpeg"));
    let third = show_file_of_type(&mut store, "H", "R", "h.mp4", Some("video/mp4"));
    for (item, path) in [(first, "a.jpg"), (second, "c.pdf"), (changed, "e.jpg"), (moved, "d/g.jpg"), (third, "h.mp4")] {
        mark_written(&mut store, item, path);
    }
    // OneDrive has no thumbnail of the one: nothing of it is in the cache to look for.
    store.item_store().set_thumbnail_made(none_given, &listed_content("h1"), Path::new("b.pdf"), false).unwrap();
    // Since their marks, OneDrive has another content of the one, and the other is at another path.
    list_file_of_type(&mut store, "E", "R", "e.jpg", "h2", Some("image/jpeg"));
    store.item_store().apply(&LocalVersionChange::Moved { item: directory, place: place(LocalItemId::ROOT, "renamed"), is_name_ignored: false }).unwrap();

    let (first_path, second_path, third_path) = (PathBuf::from("a.jpg"), PathBuf::from("c.pdf"), PathBuf::from("h.mp4"));
    assert_eq!(marks(&mut store, None, 10), [(first, first_path.clone()), (second, second_path.clone()), (third, third_path.clone())], "by local item id, each with its path");
    assert_eq!(marks(&mut store, None, 2), [(first, first_path), (second, second_path.clone())]);
    assert_eq!(marks(&mut store, Some(second), 2), [(third, third_path.clone())], "after the last of a batch");
    assert_eq!(marks(&mut store, Some(first), 1), [(second, second_path)], "the marks that do not stand are passed over, however few are asked for");
    assert_eq!(marks(&mut store, Some(third), 10), []);
}

#[test]
fn a_forgotten_mark_makes_its_file_a_candidate_again() {
    let mut store = store_with_listed_root();
    let image = show_file_of_type(&mut store, "I", "R", "i.jpg", Some("image/jpeg"));
    let document = show_file_of_type(&mut store, "P", "R", "p.pdf", Some("application/pdf"));
    mark_written(&mut store, image, "i.jpg");
    mark_written(&mut store, document, "p.pdf");
    assert_eq!(candidates(&mut store, None, 10), []);

    store.item_store().forget_thumbnail_marks(&[document, LocalItemId::new(999)]).unwrap();
    assert_eq!(candidates(&mut store, None, 10), [(document, PathBuf::from("p.pdf"))], "an item with no mark is passed over");
    assert_eq!(marks(&mut store, None, 10), [(image, PathBuf::from("i.jpg"))]);
    store.item_store().forget_thumbnail_marks(&[]).unwrap();
    assert_eq!(marks(&mut store, None, 10).len(), 1, "nothing is forgotten for no item");
}
