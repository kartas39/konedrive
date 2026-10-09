//! What OneDrive changed under a row (`docs/design/writes.md` §7): both versions kept where
//! both changed, the first rename wins, and what was deleted there.

use super::*;
use konedrive_tree::outbox::{Base, Detection};

/// A row of new content made against another version than the base has — its cTag, and
/// no eTag — is guarded by that cTag: the guard fails, and both versions are kept. (The
/// mount records an edit against the base as it is; this is the row's own rule.)
#[test]
fn an_edit_of_an_outdated_download_is_guarded_by_its_ctag() {
    let w = World::new(&[file("A", "R", "a.txt", b"old")]);
    w.download("a.txt", b"old");
    // A newer version came in the delta; the file is not replaced yet.
    w.cloud(|c| c.edit("A", b"newer"));
    let newer = w.cloud(|c| c.item("A").cloned().unwrap());
    w.store
        .call_blocking(move |s| {
            let mut base = s.get(Table::Items, "A")?.unwrap();
            base.etag = Some(newer.etag.clone());
            base.ctag = Some(newer.ctag.clone());
            base.quickxor = newer.hash.clone();
            base.size = newer.size;
            s.begin_staging(konedrive_tree::NewTree::Delta)?;
            s.stage(&[Change::Upsert(base)])?;
            s.commit_staging("link-2")
        })
        .unwrap();
    w.overwrite("a.txt", b"mine, from the old one");
    let outdated = Base { etag: None, ctag: Some("c-A".into()), parent: Some("R".into()), name: Some("a.txt".into()) };
    let size = std::fs::metadata(w.path("a.txt")).unwrap().len();
    let update = Detection { item_id: Some("A".into()), base: Some(outdated), ..w.detection(Update, Path::new("a.txt"), Some(w.handle("a.txt")), Some(size)) };
    w.record(vec![update], Vec::new());
    let row = &w.rows()[0];
    assert_eq!((row.kind, row.base.as_ref().and_then(|b| b.etag.clone()), row.base.as_ref().and_then(|b| b.ctag.clone())), (Update, None, Some("c-A".into())));
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert!(w.cloud(|c| c.guards.iter().any(|(path, tag)| path.ends_with("items/A/createUploadSession") && tag == "c-A")), "{:?}", w.cloud(|c| c.guards.clone()));
    assert_eq!(w.cloud(|c| c.paths()), vec!["a-fedora.txt", "a.txt"]);
    assert_eq!(w.content("a.txt").unwrap(), b"newer");
    assert_committed(&w, "a-fedora.txt", "a-fedora.txt");
}

/// §7, the cells where both sides changed one item.
#[test]
fn conflicts_keep_both_and_the_first_rename_wins() {
    // edit × edit: the local version becomes a copy beside OneDrive's.
    let w = World::new(&[file("A", "R", "a.txt", b"old")]);
    w.download("a.txt", b"old");
    w.write_file("a.txt", b"mine");
    w.cloud(|c| c.edit("A", b"theirs"));
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["a-fedora.txt", "a.txt"]);
    assert_eq!(w.content("a.txt").unwrap(), b"theirs");
    assert_eq!(w.content("a-fedora.txt").unwrap(), b"mine");
    assert_committed(&w, "a-fedora.txt", "a-fedora.txt");
    assert!(!w.path("a.txt").exists(), "OneDrive's version is not downloaded: nothing is at the name on disk");
    assert!(kept_as_copy(&w, "a.txt", "a-fedora.txt"), "the conflict is on record");
    assert!(w.h.host.kinds().contains(&ActivityKind::Conflict));
    assert!(w.h.host.cycles.load(Ordering::SeqCst) > 0);
    // the outbox on the bus: the delta carries OneDrive's version; no Full
    // reconcile, which until the read-write reconcile puts waiting renames back.
    assert_eq!(w.h.host.fulls.load(Ordering::SeqCst), 0, "a copy asks for no Full reconcile");

    // edit × rename there: the content goes to the renamed item, and the
    // file takes OneDrive's name.
    let w = World::new(&[file("A", "R", "a.txt", b"old")]);
    w.download("a.txt", b"old");
    w.write_file("a.txt", b"mine");
    w.cloud(|c| c.rename("A", "R", "c.txt"));
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["c.txt"]);
    assert_committed(&w, "c.txt", "c.txt");

    // edit × delete there: uploaded again as new (local wins).
    let w = World::new(&[file("A", "R", "a.txt", b"old")]);
    w.download("a.txt", b"old");
    w.write_file("a.txt", b"mine");
    w.cloud(|c| {
        let gone = c.items.remove("A").unwrap();
        c.bin.insert("A".into(), gone);
    });
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_ne!(w.id_at("a.txt").as_deref(), Some("A"), "a new item");
    assert_committed(&w, "a.txt", "a.txt");
    assert!(w.base("A").is_none());
    assert!(w.h.host.kinds().contains(&ActivityKind::Restored));

    // rename × edit there: the rename goes again with the fresh eTag.
    let w = World::new(&[file("A", "R", "a.txt", b"old")]);
    w.rename_entry("a.txt", "b.txt");
    w.cloud(|c| c.edit("A", b"theirs"));
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["b.txt"]);
    assert_eq!(w.content("b.txt").unwrap(), b"theirs");

    // rename × rename: the first to reach OneDrive wins; the file follows.
    let w = World::new(&[file("A", "R", "a.txt", b"old")]);
    w.download("a.txt", b"old");
    w.rename_entry("a.txt", "b.txt");
    w.cloud(|c| c.rename("A", "R", "c.txt"));
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["c.txt"]);
    assert!(w.path("c.txt").exists() && !w.path("b.txt").exists());
    assert_eq!(w.base("A").unwrap().name, "c.txt");

    // rename × delete: a downloaded file goes up as new; a file that is not
    // downloaded, which holds nothing here, follows the delete.
    let w = World::new(&[file("A", "R", "a.txt", b"old"), file("P", "R", "p.bin", b"only there")]);
    w.download("a.txt", b"old");
    w.rename_entry("a.txt", "b.txt");
    w.rename_entry("p.bin", "q.bin");
    w.cloud(|c| {
        for id in ["A", "P"] {
            let gone = c.items.remove(id).unwrap();
            c.bin.insert(id.into(), gone);
        }
    });
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["b.txt"]);
    assert_committed(&w, "b.txt", "b.txt");
    assert!(!w.path("q.bin").exists(), "nothing was here to keep");
    assert!(w.base("P").is_none());

    // delete × edit: OneDrive's version comes back; nothing is deleted.
    let w = World::new(&[file("A", "R", "a.txt", b"old"), file("B", "R", "b.txt", b"b"), file("C", "R", "c.txt", b"c")]);
    for rel in ["a.txt", "b.txt", "c.txt"] {
        w.remove_entry(rel);
    }
    w.cloud(|c| {
        c.edit("A", b"theirs");
        c.rename("B", "R", "b2.txt");
        let gone = c.items.remove("C").unwrap();
        c.bin.insert("C".into(), gone);
    });
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    // edit: kept;
    // rename: the content the user deleted, deleted with the fresh eTag;
    // delete: done.
    assert_eq!(w.cloud(|c| c.paths()), vec!["a.txt"]);
    assert!(w.base("A").is_some() && w.base("B").is_none() && w.base("C").is_none());
    assert!(w.h.host.kinds().contains(&ActivityKind::Restored));
    assert!(w.h.host.fulls.load(Ordering::SeqCst) > 0, "placed again by a Full reconcile");

    // create × create: the same content is adopted, other content copied.
    let w = World::new(&[]);
    w.make_file("same.txt", b"same");
    w.make_file("x.txt", b"mine");
    w.cloud(|c| {
        c.add_file("S", "R", "same.txt", b"same");
        c.add_file("X", "R", "X.TXT", b"theirs");
    });
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["X.TXT", "same.txt", "x-fedora.txt"]);
    assert_eq!(w.attr("same.txt", XATTR_ITEM_ID).as_deref(), Some("S"), "adopted: nothing sent");
    assert_eq!(w.content("x-fedora.txt").unwrap(), b"mine");
    assert_eq!(w.content("X.TXT").unwrap(), b"theirs");
}

/// Rename × rename of a file that is not downloaded: the first to reach OneDrive wins
/// (§7), as for a downloaded one, though nothing is on disk to rename.
#[test]
#[ignore = "a move of a file with no downloaded copy that meets an earlier rename in OneDrive is sent again and undoes that rename: `steps::meta::moved` follows OneDrive's place only with an entry on disk"]
fn a_renamed_file_that_is_not_downloaded_follows_an_earlier_rename_in_onedrive() {
    let w = World::new(&[file("A", "R", "a.txt", b"old")]);
    w.rename_entry("a.txt", "b.txt");
    w.cloud(|c| c.rename("A", "R", "c.txt"));
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["c.txt"]);
    assert_eq!(w.base("A").unwrap().name, "c.txt");
}

/// A file that is not downloaded, renamed onto a name another file took in OneDrive
/// meanwhile: nothing is on disk to keep as a copy, and the row must still come to an end.
#[test]
#[ignore = "a move of a file with no downloaded copy onto a name taken in OneDrive waits as `not-found` and is tried every 30 s for ever: `steps::shared::name_taken` makes a copy only of an entry on disk"]
fn a_renamed_file_that_is_not_downloaded_does_not_wait_for_ever_for_a_taken_name() {
    let w = World::new(&[file("A", "R", "a.txt", b"old")]);
    w.rename_entry("a.txt", "b.txt");
    w.cloud(|c| c.add_file("X", "R", "b.txt", b"theirs"));
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.content("b.txt").unwrap(), b"theirs");
}

/// A delete × edit dropped while a cycle is between staging and swap.
/// The drop waits for the cycle's swap (the tree lock), so the swap cannot
/// give the item back the local object it forgot — which would make the
/// next examination delete OneDrive's newer version.
#[test]
fn delete_commits_wait_for_the_cycles_swap() {
    let w = World::new(&[file("A", "R", "a.txt", b"a"), file("B", "R", "b.txt", b"b")]);
    w.remove_entry("a.txt");
    w.cloud(|c| c.edit("A", b"theirs"));
    let lock = w.h.runtime.block_on(Arc::clone(&w.h.tree_lock).lock_owned());
    w.store.call_blocking(move |s| s.begin_staging(konedrive_tree::NewTree::Delta)).unwrap();
    let engine = w.h.engine();
    let task = w.h.runtime.spawn({
        let engine = Arc::clone(&engine);
        async move { engine.drain(&CancellationToken::new()).await }
    });
    let mut waited = 0;
    while w.cloud(|c| c.count("GET", "items/A")) == 0 && waited < 500 {
        std::thread::sleep(Duration::from_millis(10));
        waited += 1;
    }
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(w.rows().len(), 1, "the drop waits for the cycle");
    w.store.call_blocking(move |s| s.commit_staging("link-2")).unwrap();
    drop(lock);
    w.h.runtime.block_on(task).unwrap();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert!(w.cloud(|c| c.item("A").is_some() && c.bin.is_empty()));
}
