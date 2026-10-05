//! A folder replaced offline by a new one that keeps one of its files: the examination's
//! rows, and the worker's side of the name rule on them (F55).

use konedrive_tree::outbox::{frees, takes};

use super::*;
use crate::local::testing::names;
use crate::upload::SWAP_PREFIX;

use OutboxKind::Delete;

/// **The fixture the outbox worker proves its side of the name rule on** (F55 (7)): a
/// folder replaced offline by a new one holding one of its files — `mkdir
/// exports.new; mv exports/keep.txt exports.new/; cp … exports.new/`, and
/// later `rm -rf exports; mv exports.new exports`. The new folder's `mkdir`
/// waits for nothing: rule 4's wait on the old folder's `delete` closes a
/// circle (the keep's move waits for the `mkdir`, the `delete` for the
/// keep's move) and is dropped. So the `mkdir` runs while the old folder is
/// still in OneDrive and meets a 409. the outbox worker must then find the old folder's id
/// in a live row whose `frees` is that place, and never adopt it: it makes
/// the folder under `.konedrive-swap-<id>`, commits that place with a live
/// `move` row for the final name, and only then do the keep's move and the
/// old folder's delete run — the delete after the keep has left it.
///
/// **The outbox worker's side**, driven by the real worker against a fake OneDrive:
/// with the freer (the old folder's `delete`) in every state a live row can
/// be in, and with the new folder's name differing from the old one's only
/// in case, the kept file and the new folder's content are never deleted.
/// The handover after the temporary step: the final `move` has a base (the
/// temporary place), so it waits for the delete and never meets the name
/// still taken.
#[test]
fn w5_fixture_folder_replaced_offline_keeping_one_file() {
    let variants = [
        ("exports", None),
        ("Exports", None),
        ("exports", Some(OutboxState::Waiting)),
        ("exports", Some(OutboxState::Retry)),
        ("exports", Some(OutboxState::Blocked)),
        ("exports", Some(OutboxState::Held)),
        ("exports", Some(OutboxState::Running)),
    ];
    for (new_name, freer) in variants {
        let w = World::new(&[folder("E", "R", "exports"), file("K", "E", "keep.txt", b"k"), file("O", "E", "old.txt", b"o")]);
        let (fx, h) = (&w.folder, &w.h);
        std::fs::create_dir(fx.path("exports.new")).unwrap();
        fx.rename("exports/keep.txt", "exports.new/keep.txt");
        fx.write("exports.new/new.txt", b"new");
        fx.examine(&names(&[("", "exports.new"), ("exports", "keep.txt")]));
        std::fs::remove_dir_all(fx.path("exports")).unwrap();
        fx.rename("exports.new", new_name);
        fx.examine(&names(&[("", new_name), ("", "exports"), ("", "exports.new")]));

        let (keep_rel, new_rel) = (format!("{new_name}/keep.txt"), format!("{new_name}/new.txt"));
        let mut rows = fx.summary();
        rows.sort();
        assert_eq!(
            rows,
            vec![
                (Create, new_rel.clone(), None),
                (Mkdir, new_name.into(), None),
                (Move, keep_rel.clone(), Some("K".into())),
                (Delete, "exports".into(), Some("E".into())),
            ],
            "{new_name}"
        );
        let row = |kind: OutboxKind| fx.rows().into_iter().find(|r| r.kind == kind).unwrap();
        let (mkdir, keep, delete, create) = (row(Mkdir), row(Move), row(Delete), row(Create));
        let blockers = |seq: i64| fx.store.call_blocking(move |s| s.outbox_blockers(seq)).unwrap();
        assert!(blockers(mkdir.seq).is_empty(), "the name's wait closed a circle and was dropped");
        assert_eq!(blockers(keep.seq), vec![mkdir.seq]);
        assert_eq!(blockers(delete.seq), vec![keep.seq], "the old folder goes only after the keep has left it");
        assert_eq!(blockers(create.seq), vec![mkdir.seq]);
        let runnable: Vec<_> = fx.store.call_blocking(move |s| s.outbox_runnable(i64::MAX)).unwrap().into_iter().map(|r| (r.kind, r.rel.display().to_string())).collect();
        assert_eq!(runnable, vec![(Mkdir, new_name.to_owned())]);
        // What the outbox worker checks on the 409: the place the mkdir takes is the place a
        // live row frees, and that row names the item holding it.
        assert_eq!(takes(&mkdir), Some(("R", new_name)));
        assert_eq!(frees(&delete), Some(("R", "exports")));
        assert_eq!((delete.item_id.as_deref(), delete.state), (Some("E"), OutboxState::Ready));

        // The worker drives these rows against a fake OneDrive holding
        // what the base holds.
        if let Some(state) = freer {
            fx.store.call_blocking(move |s| s.outbox_set_state(delete.seq, state, Some(&"test".into()), Some(i64::MAX))).unwrap();
        }
        let never_deleted = |h: &Harness| {
            h.graph.with(|c| {
                assert!(!c.bin.contains_key("K"), "{new_name} {freer:?}: the kept file was deleted");
                assert!(c.bin.keys().all(|id| id == "E" || id == "O"), "{new_name} {freer:?}: {:?} deleted", c.bin.keys().collect::<Vec<_>>());
                assert_ne!(c.item("K").and_then(|k| k.parent.clone()).as_deref(), Some("E"), "the kept file left the old folder first");
                let content = c.paths().into_iter().filter(|p| p.ends_with("/keep.txt") || p.ends_with("/new.txt")).count();
                assert_eq!(content, 2, "{new_name} {freer:?}: {:?}", c.paths());
            })
        };
        h.run();
        never_deleted(h);
        let swap = format!("{SWAP_PREFIX}s{}", mkdir.seq);
        if let Some(state) = freer.filter(|&s| s != OutboxState::Running) {
            // The freer does not run: the new folder stays under its
            // temporary name with the kept file and the new one, and its
            // final move waits for the delete — the handover.
            let paths = h.graph.with(|c| c.paths());
            let expected = vec![swap.clone(), format!("{swap}/keep.txt"), format!("{swap}/new.txt"), "exports".to_owned(), "exports/old.txt".to_owned()];
            assert_eq!(paths, expected, "{state:?}");
            let rows = fx.rows();
            assert_eq!(rows.len(), 2, "{:?}", fx.summary());
            let last = rows.iter().find(|r| r.kind == Move).expect("the final move");
            assert_eq!(last.base.as_ref().and_then(|b| b.name.as_deref()), Some(swap.as_str()), "its base: the temporary place");
            assert_eq!(blockers(last.seq), vec![delete.seq], "the final move waits for the delete");
            fx.store.call_blocking(move |s| s.outbox_set_state(delete.seq, OutboxState::Ready, None, None)).unwrap();
            h.run();
            never_deleted(h);
        }
        assert!(fx.rows().is_empty(), "{new_name} {freer:?}: {:?}", fx.summary());
        assert_eq!(h.graph.with(|c| c.paths()), vec![new_name.to_owned(), keep_rel.clone(), new_rel.clone()], "{new_name} {freer:?}");
        let mut binned: Vec<String> = h.graph.with(|c| c.bin.keys().cloned().collect());
        binned.sort();
        assert_eq!(binned, vec!["E", "O"]);
        // Here: the folder is the new item, the kept file still K with its
        // content; nothing was taken for another.
        let new_id = h.graph.with(|c| c.at(new_name).unwrap().id.clone());
        assert_eq!(w.attr(new_name, XATTR_ITEM_ID), Some(new_id));
        assert_eq!(w.attr(&keep_rel, XATTR_ITEM_ID).as_deref(), Some("K"));
        assert_eq!(std::fs::read(fx.path(&new_rel)).unwrap(), b"new");
        assert!(h.graph.with(|c| c.log.iter().all(|(m, p)| !(m == "DELETE" && p.ends_with("items/K")))));
    }
}
