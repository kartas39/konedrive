//! A worker stopped while a section runs that changes the folder and records the change: the
//! two are one section, so the stop finds both done, as when they ran in one go on the row's task.

use std::sync::mpsc;

use super::*;

/// Runs a worker on `w` until the section that changes the folder is about to record what it
/// did, stops the worker there, and lets the section go on only once its row's task is gone.
fn stopped_before_the_record(w: &World) {
    let worker = OutboxWorker::new(w.h.config());
    let (reached, has_reached) = mpsc::channel();
    *worker.engine.record_hook.lock().unwrap() = Some(Box::new(move |row_dropped: mpsc::Receiver<()>| {
        reached.send(()).unwrap();
        // Ends when the row's task is dropped: the stop has cut it.
        let _ = row_dropped.recv();
    }));
    w.h.runtime.block_on(async {
        worker.start();
        tokio::task::spawn_blocking(move || has_reached.recv().unwrap()).await.unwrap();
        worker.stop().await;
    });
}

/// Edit × edit, the worker stopped while the conflict copy is made: the copy is in the folder,
/// and the outbox knows it — the row is the copy's `create`, and the conflict is recorded.
#[test]
fn a_stop_during_the_conflict_copy_leaves_the_copy_recorded() {
    let w = World::new(&[file("A", "R", "a.txt", b"old")]);
    w.hydrate("a.txt", b"old");
    w.edit("a.txt", b"mine");
    w.examine(&[("", "a.txt")]);
    w.cloud(|c| c.edit("A", b"theirs"));
    stopped_before_the_record(&w);

    assert!(w.path("a-fedora.txt").exists() && !w.path("a.txt").exists(), "the copy was made");
    let rows = w.rows();
    assert_eq!(rows.len(), 1, "{:?}", w.summary());
    assert_eq!((rows[0].kind, rows[0].rel.to_str(), rows[0].item_id.as_deref()), (Create, Some("a-fedora.txt"), None), "the row is the copy's");
    let copy = w.path("a-fedora.txt").display().to_string();
    assert_eq!(w.store.call_blocking(move |s| s.conflict_kind(&copy)).unwrap().as_deref(), Some("copy"));

    // The next start sends the copy.
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["a-fedora.txt", "a.txt"]);
}

/// Edit × delete there, the worker stopped while the file loses its attributes: the row is
/// the file's `create` already, and the base has forgotten the item.
#[test]
fn a_stop_during_an_upload_as_new_leaves_the_row_a_create() {
    let w = World::new(&[file("A", "R", "a.txt", b"old")]);
    w.hydrate("a.txt", b"old");
    w.edit("a.txt", b"mine");
    w.examine(&[("", "a.txt")]);
    w.cloud(|c| {
        let gone = c.items.remove("A").unwrap();
        c.bin.insert("A".into(), gone);
    });
    stopped_before_the_record(&w);

    assert_eq!(w.id_at("a.txt"), None, "the attributes are off");
    let rows = w.rows();
    assert_eq!(rows.len(), 1, "{:?}", w.summary());
    assert_eq!((rows[0].kind, rows[0].item_id.as_deref()), (Create, None));
    assert!(w.base("A").is_none());

    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_committed(&w, "a.txt", "a.txt");
}
