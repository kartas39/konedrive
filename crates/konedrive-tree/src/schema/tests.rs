use rusqlite::Connection;

use crate::*;

/// A store of a version no step starts from — a newer daemon's — is
/// rebuilt once, from a full listing, and then kept.
#[test]
fn a_store_of_a_version_with_no_step_is_rebuilt_once() {
    for version in ["1", "99"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tree.sqlite");
        TreeStore::open(&path).unwrap().set_meta("delta_link", Some("link-1")).unwrap();
        Connection::open(&path).unwrap().execute("UPDATE meta SET value = ?1 WHERE key = 'schema_version'", [version]).unwrap();
        let store = TreeStore::open(&path).unwrap();
        assert_eq!(store.delta_link().unwrap(), None, "version {version}: the next cycle lists in full");
        assert_eq!(store.meta("schema_version").unwrap().as_deref(), Some(SCHEMA_VERSION));
        store.set_meta("delta_link", Some("link-2")).unwrap();
        drop(store);
        assert_eq!(TreeStore::open(&path).unwrap().delta_link().unwrap().as_deref(), Some("link-2"), "version {version}: rebuilt once, then kept");
    }
}

/// A store that cannot be read as one is rebuilt, not an error at every
/// open: a file that is not a database, and a schema a crash cut short
/// (some tables, no `meta`).
#[test]
fn a_store_that_is_not_one_is_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.sqlite");
    std::fs::write(&path, b"this is not sqlite at all, not even a little").unwrap();
    assert_eq!(TreeStore::open(&path).unwrap().delta_link().unwrap(), None);

    let path = dir.path().join("cut-short.sqlite");
    Connection::open(&path).unwrap().execute_batch("CREATE TABLE items (id TEXT PRIMARY KEY);").unwrap();
    assert_eq!(TreeStore::open(&path).unwrap().meta("schema_version").unwrap().as_deref(), Some(SCHEMA_VERSION));
}

/// A new store has every table from the start: `pins` too, which nothing makes at first
/// use any more, and the outbox's column that names a row's local item.
#[test]
fn a_new_store_has_the_pins_and_the_outboxs_local_item() {
    let store = TreeStore::in_memory().unwrap();
    let has = |sql: &str| store.conn.query_row(sql, [], |row| row.get::<_, i64>(0)).unwrap() == 1;
    assert!(has("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'pins'"));
    assert!(has("SELECT count(*) FROM pragma_table_info('outbox') WHERE name = 'local_item_id'"));
    assert_eq!(store.pins().unwrap(), Vec::<String>::new());
    store.set_pin("A", true).unwrap();
    assert_eq!(store.pins().unwrap(), ["A"]);
}
