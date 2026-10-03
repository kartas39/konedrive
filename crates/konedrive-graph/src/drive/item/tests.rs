use super::*;

#[test]
fn graph_times_are_unix_seconds() {
    assert_eq!(parse_graph_time("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_graph_time("2024-05-01T10:00:00Z"), Some(1_714_557_600));
    assert_eq!(parse_graph_time("2024-05-01T10:00:00.123Z"), Some(1_714_557_600), "fractions are dropped");
    assert_eq!(parse_graph_time("2000-02-29T23:59:59Z"), Some(951_868_799));
    assert_eq!(parse_graph_time("1969-12-31T23:59:59Z"), Some(-1));
}

#[test]
fn unix_seconds_format_back_to_graph_times() {
    for (seconds, text) in [
        (0, "1970-01-01T00:00:00Z"),
        (1_714_557_600, "2024-05-01T10:00:00Z"),
        (951_868_799, "2000-02-29T23:59:59Z"),
        (-1, "1969-12-31T23:59:59Z"),
    ] {
        assert_eq!(format_graph_time(seconds), text);
        assert_eq!(parse_graph_time(text), Some(seconds));
    }
}

#[test]
fn anything_else_is_not_a_graph_time() {
    for bad in ["", "2024-05-01", "2024-05-01T10:00:00", "2024-13-01T00:00:00Z", "2024-05-01T25:00:00Z", "x-05-01T10:00:00Z"] {
        assert_eq!(parse_graph_time(bad), None, "{bad:?}");
    }
}

#[test]
fn a_delta_item_deserializes_with_every_facet_it_may_carry() {
    let item: DriveItem = serde_json::from_value(serde_json::json!({
        "id": "I", "name": "n", "size": 3, "eTag": "e", "cTag": "c",
        "parentReference": {"id": "P", "driveId": "D"},
        "file": {"mimeType": "image/jpeg", "hashes": {"quickXorHash": "q"}},
        "fileSystemInfo": {"lastModifiedDateTime": "2024-05-01T10:00:00Z"},
        "specialFolder": {"name": "vault"}, "remoteItem": {}, "package": {"type": "oneNote"},
        "deleted": {"state": "deleted"}, "root": {}, "folder": {"childCount": 0}
    }))
    .unwrap();
    assert_eq!(item.parent_reference.as_ref().and_then(|p| p.id.as_deref()), Some("P"));
    assert_eq!(item.file.as_ref().and_then(|f| f.hashes.as_ref()).and_then(|h| h.quick_xor_hash.as_deref()), Some("q"));
    assert_eq!(item.special_folder.as_ref().and_then(|s| s.name.as_deref()), Some("vault"));
    assert!(item.remote_item.is_some() && item.package.is_some() && item.deleted.is_some() && item.root.is_some());
}
