use serde_json::json;

use super::*;

fn item(value: serde_json::Value) -> DriveItem {
    serde_json::from_value(value).unwrap()
}

#[test]
fn a_file_item_becomes_a_placed_file_row() {
    let change = classify(&item(json!({
        "id": "F", "name": "a.jpg", "size": 7, "eTag": "e", "cTag": "c",
        "parentReference": {"id": "R"},
        "file": {"mimeType": "image/jpeg", "hashes": {"quickXorHash": "q"}},
        "fileSystemInfo": {"lastModifiedDateTime": "2024-05-01T10:00:00Z"}
    })));
    let Change::Upsert(row) = change else { panic!("{change:?}") };
    assert_eq!((row.kind, row.size, row.mtime, row.placement), (Kind::File, 7, 1_714_557_600, Placement::Placed));
    assert_eq!((row.ctag.as_deref(), row.quickxor.as_deref(), row.mime.as_deref()), (Some("c"), Some("q"), Some("image/jpeg")));
    assert_eq!(row.parent_id.as_deref(), Some("R"));
}

#[test]
fn the_root_deletions_and_folders_are_told_apart() {
    assert!(matches!(classify(&item(json!({"id": "R", "root": {}, "folder": {}}))), Change::Root(_)));
    assert_eq!(classify(&item(json!({"id": "X", "deleted": {"state": "deleted"}}))), Change::Delete("X".into()));
    let Change::Upsert(row) = classify(&item(json!({"id": "D", "name": "d", "size": 999, "folder": {}, "parentReference": {"id": "R"}}))) else { panic!() };
    assert_eq!((row.kind, row.size), (Kind::Folder, 0), "a folder's size is its content's, not a file size");
}

#[test]
fn every_skip_reason_is_recognised() {
    let cases = [
        (json!({"id": "1", "name": "я".repeat(128), "file": {}, "parentReference": {"id": "R"}}), SkipReason::NameTooLong),
        (json!({"id": "2", "name": "Personal Vault", "folder": {}, "specialFolder": {"name": "vault"}, "parentReference": {"id": "R"}}), SkipReason::PersonalVault),
        (json!({"id": "3", "name": "shared", "folder": {}, "remoteItem": {"id": "x"}, "parentReference": {"id": "R"}}), SkipReason::Shared),
        (json!({"id": "4", "name": "Notes", "package": {"type": "oneNote"}, "parentReference": {"id": "R"}}), SkipReason::OneNote),
        (json!({"id": "5", "name": ".konedrive-holding", "folder": {}, "parentReference": {"id": "R"}}), SkipReason::ReservedName),
        (json!({"id": "6", "name": "odd", "parentReference": {"id": "R"}}), SkipReason::Unsupported),
        (json!({"id": "7", "name": "..", "file": {}, "parentReference": {"id": "R"}}), SkipReason::Unsupported),
    ];
    for (value, reason) in cases {
        let Change::Upsert(row) = classify(&item(value.clone())) else { panic!("{value}") };
        assert_eq!(row.placement, Placement::Skipped(reason), "{value}");
    }
    let Change::Upsert(fits) = classify(&item(json!({"id": "8", "name": "я".repeat(127), "file": {}, "parentReference": {"id": "R"}}))) else { panic!() };
    assert_eq!(fits.placement, Placement::Placed, "127 Cyrillic letters are 254 bytes and fit");
}

#[test]
fn the_255_byte_name_is_the_exact_boundary() {
    let Change::Upsert(exact) = classify(&item(json!({"id": "9", "name": "a".repeat(255), "file": {}, "parentReference": {"id": "R"}}))) else { panic!() };
    assert_eq!(exact.placement, Placement::Placed, "255 bytes is Linux's limit, inclusive");
    let Change::Upsert(over) = classify(&item(json!({"id": "10", "name": "a".repeat(256), "file": {}, "parentReference": {"id": "R"}}))) else { panic!() };
    assert_eq!(over.placement, Placement::Skipped(SkipReason::NameTooLong), "256 bytes is one over");
}

/// An item id becomes a name in the holding directory: an id that cannot
/// be one keeps the item out of the folder.
#[test]
fn an_id_that_cannot_be_a_file_name_is_not_placed() {
    for id in ["", ".", "..", "a/b", "a\0b"] {
        let Change::Upsert(row) = classify(&item(json!({"id": id, "name": "ok.txt", "file": {}, "parentReference": {"id": "R"}}))) else { panic!("{id:?}") };
        assert_eq!(row.placement, Placement::Skipped(SkipReason::Unsupported), "{id:?}");
    }
    let Change::Upsert(fine) = classify(&item(json!({"id": "8F6C!101", "name": "ok.txt", "file": {}, "parentReference": {"id": "R"}}))) else { panic!() };
    assert_eq!(fine.placement, Placement::Placed);
}
