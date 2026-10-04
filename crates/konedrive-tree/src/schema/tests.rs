use std::collections::BTreeSet;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use konedrive_fs::handle::FileHandle;
use rusqlite::Connection;

use crate::outbox::{BadItem, OutboxKind, OutboxState, Reason, SessionUrl, Snapshot};
use crate::reconcile::Committed;
use crate::*;

/// A store of version 4 as the build of `dev` at `8ea40f6` wrote it: made
/// by that build through the store's own functions and dumped as SQL. The
/// shape every installed version leaves.
const VERSION_4: &str = include_str!("tests/v4.sql");

/// The oldest store a step starts from: version 3 as it was first created,
/// before anything was added to it without a number, with the openings as
/// the build before issue #89 kept them. Written by hand from that
/// history, not made by an old build.
const VERSION_3: &str = include_str!("tests/v3.sql");

fn store_of(sql: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.sqlite");
    Connection::open(&path).unwrap().execute_batch(sql).unwrap();
    (dir, path)
}

/// Every table's columns and every index and trigger of a store, whatever
/// order they were added in.
fn shape(conn: &Connection) -> BTreeSet<String> {
    let mut shape = BTreeSet::new();
    let named = |kind: &str| -> Vec<(String, Option<String>)> {
        conn.prepare("SELECT name, sql FROM sqlite_master WHERE type = ?1 AND name NOT LIKE 'sqlite_%'")
            .unwrap()
            .query_map([kind], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    for (table, _) in named("table") {
        let columns: Vec<String> = conn
            .prepare(&format!("SELECT name || ' ' || type || ' ' || \"notnull\" || ' ' || COALESCE(dflt_value, '-') || ' ' || pk FROM pragma_table_info('{table}')"))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        shape.extend(columns.into_iter().map(|column| format!("{table}: {column}")));
    }
    for kind in ["index", "trigger", "view"] {
        shape.extend(named(kind).into_iter().map(|(name, sql)| format!("{kind} {name}: {}", sql.unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" "))));
    }
    shape
}

fn new_shape() -> BTreeSet<String> {
    shape(&TreeStore::in_memory().unwrap().conn)
}

fn handle(n: u8) -> FileHandle {
    FileHandle { kind: 1, bytes: vec![n; 4] }
}

/// The store an installed version left opens with everything in it: the
/// tree and its local objects, what waits in the outbox with its content's
/// snapshot, its session and its bad item, the openings with a row and
/// without, what a cycle keeps between cycles, the log and the conflicts.
/// It ends with the schema a new store has.
#[test]
fn an_installed_versions_store_opens_with_what_it_held() {
    let (_dir, path) = store_of(VERSION_4);
    let mut s = TreeStore::open(&path).unwrap();
    assert_eq!(s.meta("schema_version").unwrap().as_deref(), Some(SCHEMA_VERSION));
    assert_eq!(shape(&s.conn), new_shape());

    assert_eq!(s.delta_link().unwrap().as_deref(), Some("link-1"), "not rebuilt");
    assert_eq!(s.get(Table::Items, "A").unwrap().map(|row| (row.name, row.kind, row.placement)), Some(("a.txt".into(), Kind::File, Placement::Placed)));
    // What was leaving waits: placed where its object stayed, its row as
    // OneDrive has it deferred.
    assert_eq!(s.locate(Table::Items, "I").unwrap().map(|at| (at.rel, at.placed)), Some((PathBuf::from("long/inside.txt"), true)));
    assert!(matches!(s.deferred("L").unwrap(), Some(Change::Upsert(row)) if row.placement == Placement::Skipped(SkipReason::NameTooLong)));
    assert_eq!(s.local_handle("B").unwrap(), Some(handle(3)));
    assert_eq!(s.outbox_seq().unwrap(), 12);
    assert_eq!(s.committed_since(10).unwrap().get("GONE"), Some(&Committed { etag: None, gone: true }));
    assert_eq!(s.deferred_ids().unwrap(), ["B", "DG", "L"]);
    assert_eq!(s.recent_activity(5).unwrap().len(), 1);
    assert_eq!(s.conflicts().unwrap()[0].kind, ConflictKind::Copy);
    assert_eq!(s.local_skipped().unwrap()[0].rel, Path::new("link"));

    let rows = s.outbox_rows().unwrap();
    assert_eq!(rows.iter().map(|row| (row.seq, row.kind, row.state)).collect::<Vec<_>>(), [
        (1, OutboxKind::Create, OutboxState::Ready),
        (2, OutboxKind::Create, OutboxState::Ready),
        (3, OutboxKind::Update, OutboxState::Retry),
        (4, OutboxKind::MoveOut, OutboxState::Ready),
        (5, OutboxKind::Delete, OutboxState::Held),
        (7, OutboxKind::Create, OutboxState::Blocked),
    ]);
    let sending = &rows[0];
    assert_eq!(sending.snapshot(), Some(Snapshot::content(1000, 1_700_000_000, 123_456_789)));
    assert_eq!((sending.session_url.as_ref().map(SessionUrl::as_str), sending.session_expires, sending.session_next), (Some("https://up.example/session-1"), Some(2000), Some(640)));
    assert_eq!((sending.rel.as_path(), sending.target_parent.as_deref(), sending.size), (Path::new("d/new.bin"), Some("D"), Some(900)));
    assert_eq!(s.upload_sessions_at("D", "new.bin").unwrap(), [(SessionUrl::new("https://up.example/session-1"), Some(1))]);
    let retried = &rows[2];
    assert_eq!((retried.item_id.as_deref(), retried.reason.clone(), retried.attempts, retried.next_try), (Some("B"), Some(Reason::Hash), 1, Some(1500)));
    assert_eq!(retried.base.as_ref().and_then(|base| base.etag.as_deref()), Some("e-B"));
    assert_eq!(s.outbox_bad_item(3).unwrap(), Some(BadItem { id: "BAD".into(), ctag: Some("c-BAD".into()), etag: None }));
    assert_eq!((rows[3].snapshot(), rows[3].last_place()), (Some(Snapshot::ContentLocal), Some(Path::new("/elsewhere/m.txt"))));
    assert_eq!(rows[4].reason, Some(Reason::MassDelete));
    assert_eq!(rows[5].rel.as_os_str().as_bytes(), b"caf\xe9.txt");
    assert_eq!(rows[5].inode.as_ref().and_then(|inode| inode.handle.clone()), Some(handle(13)));
    assert_eq!(s.outbox_groups().unwrap().iter().map(|group| group.bytes).sum::<u64>(), 1000 + 5 + 42 + 3, "a snapshot's size, or the size detected");

    // The openings, and a row that leaves after the upgrade: its opening is
    // kept without it, as the trigger kept it.
    assert_eq!(s.upload_opening_windows("R", "left.txt").unwrap(), [(1200, 1200)]);
    assert_eq!(s.upload_opening_windows("R", "opening.txt").unwrap(), [(1100, 1100)]);
    s.outbox_drop(2, None, None).unwrap();
    assert_eq!(s.upload_opening_windows("R", "OPENING.txt").unwrap(), [(1100, 1100)]);
    s.upload_openings_expire(i64::MAX).unwrap();
    assert!(s.upload_opening_windows("R", "opening.txt").unwrap().is_empty(), "and it left now: it goes when its time is over");
}

/// A store of version 7, written by hand in the shape a build of `dev` at
/// `98b14f5` leaves: a folder that is leaving with rows waiting in it, a
/// file that is leaving, one placed again elsewhere, one whose folder the
/// base does not have, and one moved in OneDrive into the Personal Vault.
const VERSION_7: &str = include_str!("tests/v7.sql");

/// What was leaving the folder in a store of version 7 waits in version 8:
/// the item is placed again where its object stayed, with that object, and
/// the row OneDrive has of it is its deferred change, which no commit on
/// record supersedes. Nothing queued is lost, and nothing is left that the
/// daemon would rename back, delete, or upload beside its item:
///
/// - a content row of the leaving item itself is one against the place the
///   disk has, so it sends no name;
/// - a row blocked by a `404` is ready again;
/// - what was inside a leaving folder is placed with it, with no object on
///   record: an examination records what it finds in place, and proves
///   nothing gone; a content row there whose file has another name than
///   OneDrive has for the item goes, since it would send the old name back;
/// - one that cannot be carried (placed again elsewhere, or its folder not
///   in the base) is only dropped: its content row goes, since its object
///   is a copy now and goes up as new, and a new file's row there asks the
///   directory for its folder;
/// - a row the base does not place keeps no local object.
#[test]
fn what_was_leaving_waits_after_the_upgrade_and_nothing_queued_is_lost() {
    let (_dir, path) = store_of(VERSION_7);
    let mut s = TreeStore::open(&path).unwrap();
    assert_eq!(s.meta("schema_version").unwrap().as_deref(), Some(SCHEMA_VERSION));
    assert_eq!(shape(&s.conn), new_shape());
    assert_eq!(s.delta_link().unwrap().as_deref(), Some("link-7"), "not rebuilt");
    let place = |s: &TreeStore, id: &str| s.locate(Table::Items, id).unwrap().map(|at| (at.rel.display().to_string(), at.placed));
    let (long_folder, long_file) = ("l".repeat(300), format!("{}.txt", "f".repeat(300)));

    // The folder, the file, and the file moved into the Personal Vault.
    assert_eq!(place(&s, "L"), Some(("old".into(), true)));
    assert_eq!((place(&s, "I"), place(&s, "J")), (Some(("old/inside.txt".into(), true)), Some(("old/sub/deep.txt".into(), true))));
    assert_eq!((s.local_handle("L").unwrap(), s.local_handle("I").unwrap()), (Some(handle(5)), None));
    assert_eq!(s.local_handle("J").unwrap(), None, "an object a build before left below it would prove a delete");
    // A folder that was leaving inside it is carried into it, whatever the
    // order of their ids, with what waits in it.
    assert_eq!((place(&s, "B"), s.local_handle("B").unwrap()), (Some(("old/in".into(), true)), Some(handle(12))));
    assert_eq!(place(&s, "C"), Some(("old/in/c.txt".into(), true)));
    assert_eq!((place(&s, "F"), s.local_handle("F").unwrap()), (Some(("d/f.txt".into(), true)), Some(handle(6))));
    assert_eq!((place(&s, "W"), s.local_handle("W").unwrap()), (Some(("d/w.txt".into(), true)), Some(handle(9))));
    // A removal in OneDrive that already waited for the item is newer than
    // the base's row: it is what waits, and it is not superseded.
    assert_eq!((place(&s, "Y"), s.deferred("Y").unwrap()), (Some(("d/y.txt".into(), true)), Some(Change::Delete("Y".into()))));
    assert_eq!(s.local_handle("Y").unwrap(), Some(handle(14)));
    let waits = |s: &mut TreeStore| s.live_deferred().unwrap().into_iter().filter_map(|change| match change {
        Change::Upsert(row) => Some((row.id, row.parent_id.unwrap(), row.name, row.placement)),
        Change::Delete(id) => {
            assert_eq!(id, "Y");
            None
        }
        other => panic!("{other:?}"),
    }).collect::<Vec<_>>();
    assert_eq!(waits(&mut s), [
        ("A".into(), "D".into(), "a.txt".into(), Placement::Placed),
        ("B".into(), "L".into(), "b".repeat(300), Placement::Skipped(SkipReason::NameTooLong)),
        ("F".into(), "D".into(), long_file.clone(), Placement::Skipped(SkipReason::NameTooLong)),
        ("L".into(), "R".into(), long_folder.clone(), Placement::Skipped(SkipReason::NameTooLong)),
        ("W".into(), "V".into(), "w.txt".into(), Placement::Placed),
    ], "each as OneDrive has it, and none superseded by a commit on record");
    assert_eq!(
        s.skipped().unwrap().into_iter().map(|line| (line.rel.display().to_string(), line.reason, line.waits)).collect::<Vec<_>>(),
        [
            ("Personal Vault".to_owned(), SkipReason::PersonalVault, None),
            ("Personal Vault/w.txt".to_owned(), SkipReason::PersonalVault, Some(WaitsFor::Cycle)),
            (format!("d/{long_file}"), SkipReason::NameTooLong, Some(WaitsFor::Cycle)),
            ("k".repeat(300), SkipReason::NameTooLong, None),
            (long_folder, SkipReason::NameTooLong, Some(WaitsFor::Cycle)),
            ("n".repeat(300), SkipReason::NameTooLong, None),
        ]
    );
    // The next cycle stages what waits over the base, which it differs from.
    let staged = s.stage_rw(&[], 20, false).unwrap().unwrap();
    for id in ["B", "F", "L", "W"] {
        assert!(staged.ids.contains(&id.to_owned()), "{id}");
        assert!(!s.locate(Table::Staging, id).unwrap().is_some_and(|at| at.placed), "{id} is to leave");
    }

    // Not carried: placed again elsewhere, and a folder the base has not.
    assert_eq!((place(&s, "P"), s.local_handle("P").unwrap()), (Some(("d/p.txt".into(), true)), Some(handle(7))));
    assert_eq!(s.get(Table::Items, "N").unwrap().unwrap().placement, Placement::Skipped(SkipReason::NameTooLong));
    assert_eq!(s.local_handle("X").unwrap(), None, "below a folder that is not placed");
    assert_eq!(s.local_handle("A").unwrap(), Some(handle(2)));

    let rows = s.outbox_rows().unwrap();
    assert_eq!(rows.iter().map(|row| (row.seq, row.kind, row.state, row.reason.clone())).collect::<Vec<_>>(), [
        (1, OutboxKind::Update, OutboxState::Ready, None),
        (2, OutboxKind::Create, OutboxState::Ready, None),
        (3, OutboxKind::Update, OutboxState::Ready, None),
        (4, OutboxKind::Update, OutboxState::Ready, None),
        (6, OutboxKind::Create, OutboxState::Ready, None),
        (7, OutboxKind::Delete, OutboxState::Ready, None),
        (9, OutboxKind::Update, OutboxState::Ready, None),
        (10, OutboxKind::Create, OutboxState::Ready, None),
    ]);
    assert_eq!((rows[6].item_id.as_deref(), rows[7].target_parent.as_deref()), (Some("C"), Some("B")), "what waited in the inner folder still goes into it");
    let base = |row: &crate::outbox::OutboxRow| row.base.as_ref().map(|base| (base.parent.clone().unwrap(), base.name.clone().unwrap(), base.etag.clone().unwrap()));
    assert_eq!(base(&rows[3]), Some(("D".into(), "f.txt".into(), "e-F".into())), "against the place the disk has: no name is sent");
    assert_eq!((rows[3].target_parent.as_deref(), rows[3].target_name.as_deref()), (Some("D"), Some("f.txt")));
    assert_eq!(base(&rows[0]), Some(("L".into(), "inside.txt".into(), "e-I".into())));
    assert_eq!((rows[1].target_parent.as_deref(), rows[4].target_parent.as_deref()), (Some("L"), None), "a new file where the folder is not the base's asks again");
    assert!(s.get(Table::Items, "Q").unwrap().is_some_and(|row| row.name == "renamed-there.txt"), "and no row is left that names it as it was");

    assert_eq!(
        s.local_skipped().unwrap().into_iter().map(|skip| (skip.rel.display().to_string(), skip.reason)).collect::<Vec<_>>(),
        [("link".to_owned(), crate::outbox::LocalSkip::Symlink), ("old/mnt".to_owned(), crate::outbox::LocalSkip::OtherDevice)]
    );
}

/// The oldest store that is upgraded and not rebuilt goes through every
/// step, and nothing waiting in its outbox is lost: what is below a folder
/// not placed forgets its local object (issue #104); a row's snapshot and
/// marker are read from the text that held them; a session a row had is
/// listed; a bad item's id leaves the reason; an opening is kept, and is
/// left behind when its row goes. A `seq` is not handed out again.
#[test]
fn the_oldest_store_that_is_upgraded_keeps_what_waits_in_it() {
    let (_dir, path) = store_of(VERSION_3);
    let mut s = TreeStore::open(&path).unwrap();
    assert_eq!(s.meta("schema_version").unwrap().as_deref(), Some(SCHEMA_VERSION));
    assert_eq!(shape(&s.conn), new_shape());
    assert_eq!(s.delta_link().unwrap().as_deref(), Some("link-1"), "not rebuilt");

    assert_eq!((s.local_handle("F").unwrap(), s.local_handle("G").unwrap()), (None, None), "every level below");
    assert_eq!((s.local_handle("L").unwrap(), s.local_handle("T").unwrap()), (None, Some(handle(4))), "nor a row that is not placed itself");

    let rows = s.outbox_rows().unwrap();
    assert_eq!(rows.iter().map(|row| row.seq).collect::<Vec<_>>(), [1, 2, 3, 4, 5]);
    assert_eq!(rows[0].snapshot(), Some(Snapshot::content(5, 1_700_000_000, 1)));
    assert_eq!(s.upload_sessions_given_up(10).unwrap(), [], "listed, and its row still points at it");
    assert_eq!((rows[1].reason.clone(), rows[1].state, rows[1].attempts), (Some(Reason::Hash), OutboxState::Retry, 2));
    assert_eq!(s.outbox_bad_item(2).unwrap(), Some(BadItem { id: "OLD!1".into(), ctag: None, etag: None }), "an older row has no tag");
    assert_eq!(rows[2].snapshot(), Some(Snapshot::Trashed));
    assert_eq!((rows[3].snapshot(), rows[3].state), (None, OutboxState::Ready), "a text no build wrote is no snapshot, and the row stays");
    assert_eq!(s.outbox_take_snapshot(4, Snapshot::content(12, 7, 0)).unwrap(), Some(SessionUrl::new("https://up.example/odd")), "its session is given up");
    assert_eq!(s.upload_sessions_given_up(10).unwrap(), [SessionUrl::new("https://up.example/odd")]);

    assert_eq!(s.upload_opening_windows("R", "a.txt").unwrap(), [(100, 100)], "a record without `last` reads as its first time");
    s.outbox_drop(5, None, None).unwrap();
    assert_eq!(s.upload_opening_windows("R", "A.TXT").unwrap(), [(100, 100)], "kept without its row");
    let d = crate::outbox::Detection {
        kind: OutboxKind::Create,
        item_id: None,
        inode: None,
        rel: "next".into(),
        base: None,
        target_parent: Some("R".into()),
        target_name: Some("next".into()),
        same_content: false,
        state: OutboxState::Ready,
        reason: None,
        next_try: None,
        size: None,
    };
    assert_eq!(s.outbox_record(&d).unwrap(), crate::outbox::Recorded::Inserted(10));
}

/// A store of a version no step starts from — older than the outbox, or a
/// newer daemon's — is rebuilt once, from a full listing, and then kept.
#[test]
fn a_store_of_a_version_with_no_step_is_rebuilt_once() {
    for version in ["1", "2", "99"] {
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

/// A step that fails costs nothing: the open is an error, not a store to
/// rebuild, the step and its version are rolled back together, and what
/// waits in the outbox is as it was. Here the step to 6 meets a column it
/// is about to add; the step to 5 before it is kept.
#[test]
fn a_step_that_fails_leaves_the_store_and_its_outbox_as_they_were() {
    let (_dir, path) = store_of(&format!("{VERSION_4}\nALTER TABLE outbox ADD COLUMN snapshot_size INTEGER;"));
    assert!(matches!(TreeStore::open(&path), Err(TreeError::Sql(_))));
    let conn = Connection::open(&path).unwrap();
    let version: String = conn.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0)).unwrap();
    assert_eq!(version, "5", "not rebuilt, and not marked as upgraded");
    let rows: Vec<(i64, Option<String>)> =
        conn.prepare("SELECT seq, snapshot FROM outbox ORDER BY seq").unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
    assert_eq!(rows.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(), [1, 2, 3, 4, 5, 7]);
    assert_eq!((rows[0].1.as_deref(), rows[3].1.as_deref()), (Some("1000 1700000000123456789"), Some("moved-out:local")));
    let delta: String = conn.query_row("SELECT value FROM meta WHERE key = 'delta_link'", [], |r| r.get(0)).unwrap();
    assert_eq!(delta, "link-1");
}
