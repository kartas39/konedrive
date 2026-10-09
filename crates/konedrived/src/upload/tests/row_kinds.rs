//! Each kind of row from its recording to its commit: `create`, `mkdir`, `update`, `move`
//! and `delete`; and each step replayed after a crash.

use konedrive_tree::outbox::{OutboxKind, OutboxState};
use konedrive_tree::ActivityKind;

use super::world::{file, folder, World};
use super::{assert_committed, assert_placed, InFlight};
use crate::upload::Fault;
use OutboxKind::{Create, Delete, Mkdir, Move, Update};

/// New folders and files go up, parents first; each is committed on the
/// file and in the store (§5.3, §6.1).
#[test]
fn new_folders_and_files_go_up_and_are_committed() {
    let w = World::new(&[]);
    w.make_directory("docs");
    w.make_directory("docs/deep");
    w.make_file("docs/deep/a.txt", b"alpha");
    w.make_file("docs/empty", b"");
    w.make_file("top.txt", b"top");
    let mut kinds: Vec<OutboxKind> = w.rows().iter().map(|r| r.kind).collect();
    kinds.sort();
    assert_eq!(kinds, vec![Create, Create, Create, Mkdir, Mkdir]);

    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["docs", "docs/deep", "docs/deep/a.txt", "docs/empty", "top.txt"]);
    for rel in ["docs", "docs/deep", "docs/deep/a.txt", "docs/empty", "top.txt"] {
        assert_committed(&w, rel, rel);
    }
    assert_eq!(w.h.host.kinds().iter().filter(|k| **k == ActivityKind::Uploaded).count(), 5);
    assert_eq!(w.store.call_blocking(move |s| s.outbox_seq()).unwrap(), 5);
}

/// An edit goes up guarded by the base's eTag, a rename is a PATCH, and a file moved and
/// changed at once is one row: the move first, then the content against the eTag the move
/// answered with (§5.2).
#[test]
fn edits_and_renames_go_up_guarded() {
    let w = World::new(&[folder("D", "R", "docs"), file("A", "R", "a.txt", b"old")]);
    w.download("a.txt", b"old");
    w.write_file("a.txt", b"new content");
    assert_eq!(w.summary(), vec![(Update, "a.txt".into(), OutboxState::Ready)]);
    w.run();
    assert!(w.rows().is_empty());
    assert_committed(&w, "a.txt", "a.txt");
    assert_eq!(w.id_at("a.txt").as_deref(), Some("A"), "the item keeps its id");
    assert!(w.cloud(|c| c.guards.iter().any(|(path, tag)| path.ends_with("items/A/createUploadSession") && tag == "e-A")), "{:?}", w.cloud(|c| c.guards.clone()));

    w.rename_entry("a.txt", "docs/b.txt");
    assert_eq!(w.summary(), vec![(Move, "docs/b.txt".into(), OutboxState::Ready)]);
    w.run();
    assert!(w.rows().is_empty());
    assert_eq!(w.cloud(|c| c.paths()), vec!["docs", "docs/b.txt"]);
    assert_eq!(w.base("A").map(|r| (r.parent_id, r.name)), Some((Some("D".into()), "b.txt".into())));
    assert!(w.h.host.kinds().contains(&ActivityKind::CloudMoved));

    w.rename_entry("docs/b.txt", "c.txt");
    w.write_file("c.txt", b"third");
    assert_eq!(w.summary(), vec![(Update, "c.txt".into(), OutboxState::Ready)]);
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["c.txt", "docs"]);
    assert_committed(&w, "c.txt", "c.txt");
}

/// A file of OneDrive that is not downloaded is renamed and removed with nothing on disk
/// for it: the row has no object, and is sent and committed all the same.
#[test]
fn a_file_that_is_not_downloaded_is_moved_and_deleted_in_onedrive() {
    let w = World::new(&[folder("D", "R", "docs"), file("A", "R", "a.txt", b"a"), file("B", "R", "b.txt", b"b")]);
    w.rename_entry("a.txt", "docs/renamed.txt");
    w.remove_entry("b.txt");
    assert_eq!(w.summary(), vec![(Move, "docs/renamed.txt".into(), OutboxState::Ready), (Delete, "b.txt".into(), OutboxState::Ready)]);
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["docs", "docs/renamed.txt"]);
    assert!(w.cloud(|c| c.bin.contains_key("B")));
    assert_eq!(w.base("A").map(|r| (r.parent_id, r.name)), Some((Some("D".into()), "renamed.txt".into())));
    assert!(w.base("B").is_none());
    assert_eq!(w.names_in(""), vec!["docs"], "nothing was made on disk for them");
    assert!(w.h.host.kinds().contains(&ActivityKind::CloudDeleted));
}

/// A directory of OneDrive removed with all it holds, as `rm -r` through the mount removes
/// it — what is inside first, each recorded — is gone from OneDrive and from the base with
/// all it held, and nothing else is.
#[test]
fn a_folder_removed_with_all_it_holds_is_deleted_in_onedrive() {
    let w = World::new(&[folder("F", "R", "f"), file("A", "F", "a.txt", b"a"), folder("S", "F", "sub"), file("B", "S", "b.txt", b"b"), file("K", "R", "kept.txt", b"k")]);
    w.download("f/a.txt", b"a");
    w.remove_entry("f");
    assert_eq!(w.rows().iter().map(|r| r.kind).collect::<Vec<_>>(), vec![Delete; 4], "{:?}", w.summary());
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["kept.txt"]);
    assert!(["F", "A", "S", "B"].iter().all(|id| w.base(id).is_none()), "the base forgets the directory and all in it");
    assert!(w.base("K").is_some());
}

/// The `delete` of a directory is one unguarded `DELETE` of the whole directory (§6.1): what
/// is inside it in OneDrive goes with it, and the base forgets it all.
#[test]
fn a_folder_delete_sends_one_delete_and_none_for_what_is_inside() {
    let w = World::new(&[folder("F", "R", "f"), file("A", "F", "a.txt", b"a"), folder("S", "F", "sub"), file("B", "S", "b.txt", b"b")]);
    w.remove_directory_alone("f");
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert!(w.cloud(|c| c.paths()).is_empty());
    assert_eq!(w.cloud(|c| c.log.iter().filter(|(method, _)| method == "DELETE").cloned().collect::<Vec<_>>()), vec![("DELETE".to_owned(), "me/drive/items/F".to_owned())]);
    assert!(["F", "A", "S", "B"].iter().all(|id| w.base(id).is_none()), "the base forgets the directory and all in it");
}

/// §10, one crash row at a time: each step replayed on a new worker reaches
/// the same end — one item in OneDrive, adopted by hash, by place or by
/// kind, never a copy, and the outbox empty. The points of an upload
/// session are in `sessions.rs`
/// (`a_crash_at_each_point_of_a_session_is_replayed_to_the_same_item`).
#[test]
fn every_crash_point_is_replayed_to_the_same_end() {
    let cases: Vec<(&str, Fault)> = vec![
        ("create", Fault::CommitStep1Partial),
        ("create", Fault::AfterCommitStep1),
        ("update", Fault::AfterCommitStep1),
        ("mkdir", Fault::AfterSend),
        ("mkdir", Fault::AfterCommitStep1),
        ("move", Fault::AfterSend),
        ("delete", Fault::AfterSend),
    ];
    for (what, fault) in cases {
        let w = World::new(&[file("A", "R", "a.txt", b"old"), file("B", "R", "b.txt", b"b")]);
        w.download("a.txt", b"old");
        w.download("b.txt", b"b");
        let (rel, expect): (&str, Vec<&str>) = match what {
            "create" => {
                w.make_file("n.txt", b"new file");
                ("n.txt", vec!["a.txt", "b.txt", "n.txt"])
            }
            "update" => {
                w.write_file("a.txt", b"edited");
                ("a.txt", vec!["a.txt", "b.txt"])
            }
            "mkdir" => {
                w.make_directory("dir");
                ("dir", vec!["a.txt", "b.txt", "dir"])
            }
            "move" => {
                w.rename_entry("b.txt", "c.txt");
                ("c.txt", vec!["a.txt", "c.txt"])
            }
            _ => {
                w.remove_entry("b.txt");
                ("", vec!["a.txt"])
            }
        };
        assert_eq!(w.rows().len(), 1, "{what}: {:?}", w.summary());
        let engine = w.h.engine();
        engine.arm(fault);
        w.h.drain(&engine);
        assert_eq!(w.rows()[0].state, OutboxState::Running, "{what} {fault:?}: stopped at the step");

        w.run();
        assert!(w.rows().is_empty(), "{what} {fault:?}: {:?}", w.summary());
        assert!(!w.h.host.kinds().contains(&ActivityKind::Conflict), "{what} {fault:?}: no conflict copy");
        assert!(w.cloud(|c| c.placeholders()).is_empty(), "{what} {fault:?}: no placeholder left");
        assert_eq!(w.cloud(|c| c.paths()), expect, "{what} {fault:?}");
        match what {
            "delete" => {}
            "move" => assert_placed(&w, rel, rel),
            _ => assert_committed(&w, rel, rel),
        }
        assert!(w.cloud(|c| c.bin.keys().all(|id| id == "B")), "{what} {fault:?}: nothing else deleted");
    }
}

/// A new directory removed while its `mkdir` is in flight: the directory made in
/// OneDrive is committed all the same, and deleted there by the removal recorded behind
/// the running row.
#[test]
fn a_folder_removed_while_its_mkdir_goes_up_is_deleted_in_onedrive() {
    let w = World::new(&[folder("D", "R", "d")]);
    w.make_directory("d/new");
    let mkdir = InFlight::start(&w, "POST", "children");
    w.remove_entry("d/new");
    assert_eq!(w.rows().iter().map(|r| (r.kind, r.state)).collect::<Vec<_>>(), vec![(Mkdir, OutboxState::Running), (Delete, OutboxState::Ready)]);
    mkdir.finish(&w);
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["d"]);
    assert_eq!(w.cloud(|c| c.count("DELETE", "items/")), 1, "made, then deleted");
}

/// Sequences of renames, moves and removals, with the worker before the removals or after
/// them: a file renamed then deleted; a new directory that got a file moved in and
/// renamed, and a new file, all removed with it; a directory made, filled and removed
/// before anything of it was sent. OneDrive ends as the folder is.
#[test]
fn renames_moves_and_removals_reach_onedrive_as_the_disk_is() {
    for sent_between in [false, true] {
        let w = World::new(&[
            folder("A", "R", "a"),
            file("F", "A", "f.txt", b"f"),
            file("K", "A", "k.txt", b"k"),
            folder("B", "R", "b"),
            file("H", "B", "h.txt", b"h"),
        ]);
        w.rename_entry("a/f.txt", "a/renamed-f.txt");
        w.make_directory("n");
        w.rename_entry("a/k.txt", "n/k.txt");
        w.rename_entry("n/k.txt", "n/renamed-k.txt");
        w.make_file("n/new.bin", b"new");
        if sent_between {
            w.run();
            assert_eq!(w.cloud(|c| c.paths()), vec!["a", "a/renamed-f.txt", "b", "b/h.txt", "n", "n/new.bin", "n/renamed-k.txt"]);
        }

        w.remove_entry("a/renamed-f.txt");
        w.remove_entry("n");
        w.make_directory("q");
        w.make_file("q/y.txt", b"y");
        w.remove_entry("q");
        w.run();
        assert!(w.rows().is_empty(), "sent between {sent_between}: {:?}", w.summary());
        assert_eq!(w.cloud(|c| c.paths()), vec!["a", "b", "b/h.txt"], "sent between {sent_between}");
    }
}
