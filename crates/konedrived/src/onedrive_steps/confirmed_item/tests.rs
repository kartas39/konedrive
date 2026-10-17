use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use konedrive_graph::drive::DriveItem;
use konedrive_tree::{ContentPresence, ContentVersion, DriveId, ItemPlace, ItemVersion, LocalItemId};
use serde_json::json;

use super::*;

fn drive() -> DriveId {
    DriveId::new("d1").expect("a drive id")
}

pub(in crate::onedrive_steps) fn drive_item(value: serde_json::Value) -> DriveItem {
    serde_json::from_value(value).expect("a driveItem")
}

fn answered_file() -> serde_json::Value {
    json!({
        "id": "F1", "name": "Report.TXT", "eTag": "e-1", "cTag": "c-1", "size": 12,
        "parentReference": { "id": "P1", "driveId": "D1" },
        "file": { "hashes": { "quickXorHash": "hash-1" } },
        "fileSystemInfo": { "lastModifiedDateTime": "2024-05-01T10:00:00Z" },
    })
}

/// Versions with only what a guard is decided from.
pub(in crate::onedrive_steps) fn versions(onedrive_etag: Option<&str>, synced_content: Option<ContentVersion>) -> ItemVersions {
    ItemVersions {
        local_item_id: LocalItemId::new(7),
        onedrive_item: None,
        local: Some(ItemVersion { place: Some(ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: "a".into() }), content: None }),
        onedrive: None,
        onedrive_etag: onedrive_etag.map(str::to_owned),
        synced_place: None,
        synced_content,
        content_presence: ContentPresence::NotOnDisk,
        unsent_since: None,
    }
}

#[test]
fn an_answer_is_confirmed_with_the_name_graph_stored() {
    let confirmed = to_confirmed_item(&drive_item(answered_file()), &drive()).expect("readable");
    assert_eq!(confirmed.name, OsStr::new("Report.TXT"));
    assert_eq!((confirmed.item.item_id.as_str(), confirmed.parent.item_id.as_str(), confirmed.etag.as_str()), ("F1", "P1", "e-1"));
}

#[test]
fn a_files_answer_carries_its_content_as_hashed_and_a_directorys_none() {
    let file = to_confirmed_item(&drive_item(answered_file()), &drive()).expect("readable");
    assert_eq!(file.content, Some(ContentVersion::Hashed { size: 12, mtime: 1_714_557_600, mtime_nsec: 0, hash: Some("hash-1".to_owned()), ctag: Some("c-1".to_owned()) }));

    let directory = drive_item(json!({ "id": "D2", "name": "docs", "eTag": "e-2", "parentReference": { "id": "P1" }, "folder": { "childCount": 0 } }));
    assert_eq!(to_confirmed_item(&directory, &drive()).expect("readable").content, None);
}

#[test]
fn an_answer_with_no_id_no_parent_no_name_or_no_etag_is_unreadable() {
    for missing in ["id", "name", "eTag", "parentReference"] {
        let mut answer = answered_file();
        match missing {
            "id" => answer["id"] = json!(""),
            missing => {
                answer.as_object_mut().expect("an object").remove(missing);
            }
        }
        let read = to_confirmed_item(&drive_item(answer), &drive());
        assert!(matches!(read, Err(StepFailure::AnswerUnreadable { .. })), "without {missing}: {read:?}");
    }
    let mut no_parent_id = answered_file();
    no_parent_id["parentReference"] = json!({ "driveId": "d1" });
    assert!(to_confirmed_item(&drive_item(no_parent_id), &drive()).is_err());
}

#[test]
fn an_answer_that_names_the_item_as_deleted_or_as_the_root_is_unreadable() {
    for facet in ["deleted", "root"] {
        let mut answer = answered_file();
        answer[facet] = json!({});
        assert!(matches!(to_confirmed_item(&drive_item(answer), &drive()), Err(StepFailure::AnswerUnreadable { .. })), "{facet}");
    }
}

/// Graph spells one drive's id in more than one case; an answer of another drive keeps
/// that drive, and one that names none is of the folder's.
#[test]
fn a_drive_id_spelt_in_another_case_is_the_folders_drive() {
    let same = to_confirmed_item(&drive_item(answered_file()), &drive()).expect("readable");
    assert_eq!((same.item.drive_id.as_str(), same.parent.drive_id.as_str()), ("d1", "d1"));

    let mut other = answered_file();
    other["parentReference"]["driveId"] = json!("another");
    let other = to_confirmed_item(&drive_item(other), &drive()).expect("readable");
    assert_eq!((other.item.drive_id.as_str(), other.parent.drive_id.as_str()), ("another", "another"));

    let mut unnamed = answered_file();
    unnamed["parentReference"] = json!({ "id": "P1" });
    assert_eq!(to_confirmed_item(&drive_item(unnamed), &drive()).expect("readable").item.drive_id.as_str(), "d1");
}

fn hashed(ctag: Option<&str>) -> Option<ContentVersion> {
    Some(ContentVersion::Hashed { size: 1, mtime: 1, mtime_nsec: 0, hash: None, ctag: ctag.map(str::to_owned) })
}

#[test]
fn the_guard_is_the_etag_planned_from_else_the_ctag_of_the_synced_content() {
    assert_eq!(decide_guard(&versions(Some("e-9"), hashed(Some("c-9")))), Some(RequestGuard::ETag("e-9".to_owned())));
    assert_eq!(decide_guard(&versions(None, hashed(Some("c-9")))), Some(RequestGuard::CTag("c-9".to_owned())));
}

/// A content is guarded by the cTag of what OneDrive has, which a rename does not raise.
#[test]
fn the_guard_of_a_content_is_the_ctag_of_onedrives_content_else_the_synced_ones_else_the_etag() {
    let with_onedrive = |onedrive_ctag: Option<&str>, synced_ctag: Option<&str>| ItemVersions {
        onedrive: Some(ItemVersion { place: None, content: hashed(onedrive_ctag) }),
        ..versions(Some("e-9"), hashed(synced_ctag))
    };
    assert_eq!(decide_content_guard(&with_onedrive(Some("c-onedrive"), Some("c-synced"))), Some(RequestGuard::CTag("c-onedrive".to_owned())));
    assert_eq!(decide_content_guard(&with_onedrive(None, Some("c-synced"))), Some(RequestGuard::CTag("c-synced".to_owned())));
    assert_eq!(decide_content_guard(&with_onedrive(None, None)), Some(RequestGuard::ETag("e-9".to_owned())));
    assert_eq!(decide_content_guard(&versions(None, None)), None);
}

#[test]
fn a_guard_matches_an_item_by_the_tag_it_names() {
    let found = drive_item(json!({ "id": "x", "eTag": "e-2", "cTag": "c-1" }));
    assert!(RequestGuard::CTag("c-1".to_owned()).matches(&found), "renamed: the content is the same");
    assert!(!RequestGuard::ETag("e-1".to_owned()).matches(&found));
    assert!(RequestGuard::ETag("e-2".to_owned()).matches(&found));
    assert!(!RequestGuard::CTag("e-2".to_owned()).matches(&found), "a cTag is not compared with the eTag");
}

#[test]
fn an_item_with_neither_has_no_guard() {
    let changed_here = Some(ContentVersion::ChangedHere { size: 1, mtime: 1, mtime_nsec: 0, change_count: 1 });
    let no_ctag = Some(ContentVersion::Hashed { size: 1, mtime: 1, mtime_nsec: 0, hash: Some("h".to_owned()), ctag: None });
    for synced_content in [None, changed_here, no_ctag] {
        assert_eq!(decide_guard(&versions(None, synced_content.clone())), None, "{synced_content:?}");
    }
}

/// The guard of a request that follows a read: the eTag just read, else the cTag, else
/// empty.
#[test]
fn the_guard_of_an_answer_is_its_etag_else_its_ctag() {
    let guard = |value: serde_json::Value| to_guard_of_answer(&drive_item(value)).as_str().to_owned();
    assert_eq!(guard(json!({ "id": "x", "eTag": "e", "cTag": "c" })), "e");
    assert_eq!(guard(json!({ "id": "x", "cTag": "c" })), "c");
    assert_eq!(guard(json!({ "id": "x" })), "");
}

#[test]
fn a_name_that_is_not_utf8_is_refused_for_good_and_any_other_is_given_as_it_is() {
    assert_eq!(to_graph_name(OsStr::new("Отчёт.txt")), Ok("Отчёт.txt"));
    assert!(matches!(to_graph_name(OsStr::from_bytes(&[b'a', 0xff])), Err(StepRefusal::RefusedForGood { .. })));
}
