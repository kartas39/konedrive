use crate::item_store::tests::is_refused_by_schema;
use crate::TreeStore;

/// What one row says of the OneDrive side cannot contradict itself, whoever writes it: a
/// place is a parent and a name; a file OneDrive has, has a content there, and nothing else
/// has one; a content OneDrive gave is told by no change count; no row is its own parent;
/// the root has no place; and there is no OneDrive version and no eTag without a binding.
#[test]
fn the_schema_refuses_an_onedrive_version_that_contradicts_its_row() {
    let store = TreeStore::in_memory().unwrap();
    store
        .bench_sql(
            "INSERT INTO items (local_item_id, kind, onedrive_drive_id, onedrive_item_id, onedrive_parent_id, onedrive_name) VALUES (5, 'directory', 'd1', 'D', 1, 'dir');
             INSERT INTO items (local_item_id, kind, onedrive_drive_id, onedrive_item_id, onedrive_parent_id, onedrive_name, onedrive_size) VALUES (6, 'file', 'd1', 'F', 5, 'file', 3);
             INSERT INTO items (local_item_id, kind, local_parent_id, local_name, local_size) VALUES (7, 'file', 1, 'made-here', 0);
             INSERT INTO items (local_item_id, kind, onedrive_drive_id, onedrive_item_id) VALUES (8, 'file', 'd1', 'G');",
        )
        .unwrap();
    for (what, sql) in [
        ("a parent and no name", "UPDATE items SET onedrive_name = NULL WHERE local_item_id = 5"),
        ("a name and no parent", "UPDATE items SET onedrive_name = 'gone' WHERE local_item_id = 8"),
        ("its own parent", "UPDATE items SET onedrive_parent_id = 5 WHERE local_item_id = 5"),
        ("a directory with a content", "UPDATE items SET onedrive_size = 1 WHERE local_item_id = 5"),
        ("a file OneDrive has with no content", "UPDATE items SET onedrive_size = NULL WHERE local_item_id = 6"),
        ("a file OneDrive does not have with a content", "UPDATE items SET onedrive_size = 1 WHERE local_item_id = 8"),
        ("a content of OneDrive told by a change count", "UPDATE items SET onedrive_change_count = 1 WHERE local_item_id = 6"),
        ("a place with no binding", "UPDATE items SET onedrive_parent_id = 1, onedrive_name = 'made-here' WHERE local_item_id = 7"),
        ("an eTag with no binding", "UPDATE items SET onedrive_etag = 'e' WHERE local_item_id = 7"),
        ("a binding gone from a row with an OneDrive version", "UPDATE items SET onedrive_drive_id = NULL, onedrive_item_id = NULL WHERE local_item_id = 5"),
        ("the root with a place", "UPDATE items SET onedrive_parent_id = 5, onedrive_name = 'root' WHERE local_item_id = 1"),
        ("no placement", "UPDATE items SET placement = NULL WHERE local_item_id = 5"),
        ("no count of the confirmation", "UPDATE items SET confirmed_count = NULL WHERE local_item_id = 5"),
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
             INSERT INTO items (local_item_id, kind, onedrive_drive_id, onedrive_item_id, onedrive_parent_id, onedrive_name, onedrive_size) VALUES (6, 'file', 'd1', 'F', 5, 'file', 3);",
        )
        .unwrap();
    for (what, sql) in [
        ("made in a directory that has no row", "INSERT INTO items (kind, onedrive_drive_id, onedrive_item_id, onedrive_parent_id, onedrive_name) VALUES ('directory', 'd1', 'B', 99, 'x')"),
        ("made in a file", "INSERT INTO items (kind, onedrive_drive_id, onedrive_item_id, onedrive_parent_id, onedrive_name) VALUES ('directory', 'd1', 'B', 6, 'x')"),
        ("moved into a directory that has no row", "UPDATE items SET onedrive_parent_id = 99 WHERE local_item_id = 6"),
        ("moved into a file", "UPDATE items SET onedrive_parent_id = 6 WHERE local_item_id = 5"),
        ("a parent deleted from over its child", "DELETE FROM items WHERE local_item_id = 5"),
    ] {
        assert!(is_refused_by_schema(store.bench_sql(sql)), "{what}");
    }
    store.bench_sql("DELETE FROM items WHERE local_item_id = 6; DELETE FROM items WHERE local_item_id = 5;").unwrap();
}
