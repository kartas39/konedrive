use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::*;
use crate::item_store::tests::shown_items::{show_listed_file, store_in_file_with_listed_root};
use crate::model::{ContentVersion, ItemKind, ItemPlace, LocalItemId, LocalVersionChange};
use crate::{ActivityKind, ActivityRow, ConflictKind, ConflictRow, TreeStore};

fn store_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("tree.sqlite")
}

/// The store at `path` says it is of `version` from now on.
fn set_stored_version(path: &Path, version: &str) {
    Connection::open(path).unwrap().execute("UPDATE meta SET value = ?1 WHERE key = 'schema_version'", [version]).unwrap();
}

fn stored_version(path: &Path) -> StoredVersion {
    read_stored_version(&Connection::open(path).unwrap()).unwrap()
}

#[test]
fn a_store_of_todays_version_is_used() {
    assert_eq!(decide_old_store(&StoredVersion::Version(SCHEMA_VERSION.to_owned())), OldStoreDecision::Use);

    let dir = tempfile::tempdir().unwrap();
    let path = store_path(&dir);
    let mut store = store_in_file_with_listed_root(&path);
    let file = show_listed_file(&mut store, "F", "R", "f", "h1");
    drop(store);
    assert_eq!(stored_version(&path), StoredVersion::Version(SCHEMA_VERSION.to_owned()));

    let (mut store, origin) = TreeStore::open_with_origin(&path).unwrap();
    assert_eq!(origin, StoreOrigin::Kept);
    assert!(store.item_store().find_item(file).unwrap().is_some(), "what it held is there");
    assert_eq!(store.delta_link().unwrap().as_deref(), Some("link"));
}

#[test]
fn a_store_of_an_older_version_is_rebuilt_and_never_migrated() {
    for version in ["8", "14", "15", "17", "19"] {
        assert_eq!(decide_old_store(&StoredVersion::Version(version.to_owned())), OldStoreDecision::Rebuild, "version {version}");

        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);
        let mut store = store_in_file_with_listed_root(&path);
        let file = show_listed_file(&mut store, "F", "R", "f", "h1");
        drop(store);
        set_stored_version(&path, version);
        assert_eq!(stored_version(&path), StoredVersion::Version(version.to_owned()));

        let (mut store, origin) = TreeStore::open_with_origin(&path).unwrap();
        assert_eq!(origin, StoreOrigin::MadeEmpty { loss: Some(RebuildLoss::default()) }, "version {version}");
        assert_eq!(store.item_store().find_item(file).unwrap(), None, "nothing of the old store is carried over");
        assert_eq!(store.delta_link().unwrap(), None, "the first listing is a full one");
        assert_eq!(store.meta("schema_version").unwrap().as_deref(), Some(SCHEMA_VERSION));
        drop(store);
        assert_eq!(TreeStore::open_with_origin(&path).unwrap().1, StoreOrigin::Kept, "rebuilt once, then kept");
    }
}

#[test]
fn a_store_of_an_unknown_version_is_rebuilt() {
    assert_eq!(decide_old_store(&StoredVersion::Version("99".to_owned())), OldStoreDecision::Rebuild, "a newer daemon's");
    assert_eq!(decide_old_store(&StoredVersion::Version(String::new())), OldStoreDecision::Rebuild);
    assert_eq!(decide_old_store(&StoredVersion::NoVersion), OldStoreDecision::Rebuild);

    // Tables and no `meta`: a build that died while it made them. And a `meta` with no version.
    let dir = tempfile::tempdir().unwrap();
    let path = store_path(&dir);
    Connection::open(&path).unwrap().execute_batch("CREATE TABLE items (id TEXT PRIMARY KEY);").unwrap();
    assert_eq!(stored_version(&path), StoredVersion::NoVersion);
    assert_eq!(TreeStore::open_with_origin(&path).unwrap().1, StoreOrigin::MadeEmpty { loss: None }, "its one table is of another shape: nothing is counted");
    Connection::open(&path).unwrap().execute("DELETE FROM meta WHERE key = 'schema_version'", []).unwrap();
    assert_eq!(stored_version(&path), StoredVersion::NoVersion);
    let (store, origin) = TreeStore::open_with_origin(&path).unwrap();
    assert!(matches!(origin, StoreOrigin::MadeEmpty { loss: Some(_) }));
    assert_eq!(store.meta("schema_version").unwrap().as_deref(), Some(SCHEMA_VERSION));
}

#[test]
fn a_file_that_is_no_database_is_rebuilt() {
    assert_eq!(decide_old_store(&StoredVersion::NotADatabase), OldStoreDecision::Rebuild);

    let dir = tempfile::tempdir().unwrap();
    let path = store_path(&dir);
    std::fs::write(&path, b"this is not sqlite at all, not even a little").unwrap();
    assert_eq!(stored_version(&path), StoredVersion::NotADatabase);
    let (store, origin) = TreeStore::open_with_origin(&path).unwrap();
    assert_eq!(origin, StoreOrigin::MadeEmpty { loss: None }, "nothing can be counted in it");
    assert_eq!(store.meta("schema_version").unwrap().as_deref(), Some(SCHEMA_VERSION));
}

#[test]
fn a_store_that_cannot_be_read_for_another_reason_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = store_path(&dir);
    drop(store_in_file_with_listed_root(&path));
    set_stored_version(&path, "19");
    let bytes = std::fs::read(&path).unwrap();
    // The directory too: SQLite opens a file it may not write for reading alone.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

    let opened = TreeStore::open_with_origin(&path);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(opened.is_err(), "no store is made over one that cannot be read");
    assert_eq!(std::fs::read(&path).unwrap(), bytes, "the file is as it was");
    assert_eq!(stored_version(&path), StoredVersion::Version("19".to_owned()));
}

#[test]
fn where_there_is_no_store_one_is_made() {
    assert_eq!(decide_old_store(&StoredVersion::NoStore), OldStoreDecision::Make);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("made/with/its/directory/tree.sqlite");
    let (store, origin) = TreeStore::open_with_origin(&path).unwrap();
    assert_eq!(origin, StoreOrigin::MadeEmpty { loss: None });
    assert_eq!(store.meta("schema_version").unwrap().as_deref(), Some(SCHEMA_VERSION));

    // A file with nothing in it is a database with no table.
    let path = store_path(&dir);
    std::fs::write(&path, b"").unwrap();
    assert_eq!(stored_version(&path), StoredVersion::NoStore);
    assert_eq!(TreeStore::open_with_origin(&path).unwrap().1, StoreOrigin::MadeEmpty { loss: None });
}

#[test]
fn a_rebuild_counts_what_it_loses_before_the_file_goes() {
    let dir = tempfile::tempdir().unwrap();
    let path = store_path(&dir);
    let mut store = store_in_file_with_listed_root(&path);
    let removed = show_listed_file(&mut store, "F", "R", "removed", "h1");
    let moved = show_listed_file(&mut store, "G", "R", "moved", "h2");
    let moved_on_disk = show_listed_file(&mut store, "H", "R", "moved-on-disk", "h3");
    show_listed_file(&mut store, "S", "R", "synced", "h4");
    let in_root = |name: &str| ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: name.into() };
    let content = Some(ContentVersion::ChangedHere { size: 1, mtime: 1, mtime_nsec: 0, change_count: 1 });
    store.item_store().apply(&LocalVersionChange::Made { kind: ItemKind::File, place: in_root("made-here"), content, is_name_ignored: false, replaced: None }).unwrap();
    store.item_store().set_content_presence(moved_on_disk, crate::ContentPresence::OnDisk).unwrap();
    store.item_store().apply(&LocalVersionChange::Removed { item: removed }).unwrap();
    for (item, name) in [(moved, "moved-away"), (moved_on_disk, "moved-on-disk-away")] {
        store.item_store().apply(&LocalVersionChange::Moved { item, place: in_root(name), is_name_ignored: false }).unwrap();
    }
    for pinned in [removed, moved] {
        store.item_store().set_item_pin(pinned, true).unwrap();
    }
    let event = |path: &str| ActivityRow { at: 1, kind: ActivityKind::Uploaded, path: path.into(), detail: String::new() };
    store.add_activity(&[event("a"), event("b"), event("c")]).unwrap();
    store.add_conflicts(&[ConflictRow { at: 1, original: "/f".into(), rescued: "/f (copy)".into(), kind: ConflictKind::Copy }]).unwrap();
    // What the tables of an older version hold of what is unsent and of sessions: a store of today's version has neither table.
    store
        .bench_sql(
            "CREATE TABLE outbox (kind TEXT, rel TEXT, state TEXT, handle BLOB);
             CREATE TABLE upload_sessions (url TEXT, parent TEXT, name TEXT, opened INTEGER);
             INSERT INTO outbox (kind, rel, state) VALUES ('delete', 'gone', 'pending'), ('move', 'no-object', 'pending'), ('create', 'new', 'pending');
             INSERT INTO outbox (kind, rel, state, handle) VALUES ('move', 'has-object', 'pending', x'01');
             INSERT INTO upload_sessions (url, parent, name, opened) VALUES ('https://session/1', 'R', 'new', 1);",
        )
        .unwrap();
    let expected = RebuildLoss { pins: 2, activity_events: 3, conflicts: 1, unsent_changes_not_on_disk: 4, open_upload_sessions: 1 };
    assert_eq!(count_rebuild_loss(&store.conn), Some(expected), "a removal and a move of a file not on disk, in the rows and in the outbox");
    drop(store);

    // A store of today's version loses nothing: it is kept.
    assert_eq!(TreeStore::open_with_origin(&path).unwrap().1, StoreOrigin::Kept);
    set_stored_version(&path, "19");
    let (store, origin) = TreeStore::open_with_origin(&path).unwrap();
    assert_eq!(origin, StoreOrigin::MadeEmpty { loss: Some(expected) });
    assert_eq!(count_rebuild_loss(&store.conn), Some(RebuildLoss::default()), "and the store that was made holds none of it");

    // Tables of another shape: nothing is answered rather than a wrong count.
    let other = Connection::open_in_memory().unwrap();
    other.execute_batch("CREATE TABLE outbox (seq INTEGER PRIMARY KEY); CREATE TABLE pins (id TEXT);").unwrap();
    assert_eq!(count_rebuild_loss(&other), None);
    assert_eq!(count_rebuild_loss(&Connection::open_in_memory().unwrap()), Some(RebuildLoss::default()), "no table holds nothing");
}
