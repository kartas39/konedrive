//! A file or directory removed here before its upload finished: its
//! rows leave the outbox, with no retry, and nothing of it stays in OneDrive.

use super::*;

use OutboxKind::Delete;

/// Larger than one request: four fragments of 320 KiB.
fn large() -> Vec<u8> {
    (0..(1024 * 1024 + 77)).map(|i| (i % 251) as u8).collect()
}

/// `rel` queued and sent until the worker stops at `fault`, as a crash
/// leaves it: the row `running`.
fn stopped_at(w: &World, rel: &str, content: &[u8], fault: Fault) {
    w.make_file(rel, content);
    let engine = w.h.engine();
    engine.arm(fault);
    w.h.drain(&engine);
    assert_eq!(w.rows()[0].state, OutboxState::Running, "stopped as by a crash");
}

/// The requests the fake OneDrive got from `from` on.
fn requests_since(w: &World, from: usize) -> Vec<(String, String)> {
    w.cloud(|c| c.log[from..].to_vec())
}

fn not_uploaded(w: &World) -> Vec<String> {
    w.h.host.events.lock().unwrap().iter().filter(|e| e.kind == ActivityKind::NotUploaded).map(|e| e.detail.clone()).collect()
}

/// A file removed while its fragments go up, the removal recorded (a
/// `delete` behind the running `create`): on its next run the create ends —
/// the session cancelled, both rows gone, no other request. The same for a
/// row a store of the previous version holds stuck in `retry`/`not-found`.
#[test]
fn a_file_removed_mid_upload_leaves_the_outbox_with_the_delete_behind_it() {
    for stuck in [false, true] {
        let w = World::new(&[folder("D", "R", "d")]);
        stopped_at(&w, "d/big.bin", &large(), Fault::MidSession(1));
        let session = w.rows()[0].session_url.clone().expect("a session is open");
        w.remove_entry("d/big.bin");
        assert_eq!(w.summary(), vec![(Create, "d/big.bin".into(), OutboxState::Running), (Delete, "d/big.bin".into(), OutboxState::Ready)]);
        if stuck {
            let seq = w.rows()[0].seq;
            w.store.call_blocking(move |s| s.outbox_set_state(seq, OutboxState::Retry, Some(&Reason::NotFound), Some(0))).unwrap();
        }

        let from = w.cloud(|c| c.log.len());
        w.run();
        assert!(w.rows().is_empty(), "stuck {stuck}: {:?}", w.summary());
        let sid = session.as_str().rsplit('/').next().unwrap();
        assert_eq!(requests_since(&w, from), vec![("DELETE".to_owned(), format!("upload/{sid}"))], "stuck {stuck}");
        assert_eq!(w.cloud(|c| c.paths()), vec!["d"]);
        assert_eq!(not_uploaded(&w), vec!["removed here before its upload finished"]);
    }
}

/// A new directory gone from the disk, with a new file in it, before its `mkdir` ran, and
/// nothing recorded of it (the mount records a removal when it is made; this is the row that
/// meets its object gone all the same): both rows leave without a request.
#[test]
fn a_folder_removed_before_its_mkdir_ran_leaves_without_a_request() {
    let w = World::new(&[folder("D", "R", "d")]);
    w.make_directory("d/new");
    w.make_file("d/new/f.txt", b"f");
    assert_eq!(w.rows().len(), 2);
    std::fs::remove_dir_all(w.path("d/new")).unwrap();

    let from = w.cloud(|c| c.log.len());
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert!(requests_since(&w, from).is_empty(), "{:?}", requests_since(&w, from));
    assert_eq!(not_uploaded(&w).len(), 2);
}

/// A `delete` with no item id has nothing to delete: it leaves, with no
/// request (it used to be blocked as `no-item`).
#[test]
fn a_delete_with_no_item_id_leaves_without_a_request() {
    let w = World::new(&[]);
    let mut delete = w.detection(Delete, Path::new("gone.txt"), None, None);
    (delete.target_parent, delete.target_name) = (None, None);
    w.record(vec![delete], Vec::new());
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert!(w.cloud(|c| c.log.is_empty()), "{:?}", w.cloud(|c| c.log.clone()));
}

/// The last fragment (or the one request of a small file) landed and its
/// answer was lost; then the file was removed. The item it made in OneDrive
/// is this row's — its name, size and time — and goes to the recycle bin
/// with the rows.
#[test]
fn a_file_whose_upload_landed_unanswered_then_removed_is_deleted_in_onedrive() {
    for content in [b"small".to_vec(), large()] {
        let w = World::new(&[folder("D", "R", "d")]);
        stopped_at(&w, "d/f.bin", &content, Fault::AfterSend);
        let id = w.id_at("d/f.bin").expect("it landed");
        w.remove_entry("d/f.bin");
        w.run();
        assert!(w.rows().is_empty(), "{} bytes: {:?}", content.len(), w.summary());
        assert_eq!(w.cloud(|c| c.paths()), vec!["d"], "{} bytes", content.len());
        assert!(w.cloud(|c| c.bin.contains_key(&id)), "{} bytes: to the recycle bin", content.len());
        assert_eq!(not_uploaded(&w), vec!["removed here before its upload finished; what reached OneDrive went to its recycle bin"]);
    }
}

/// A file replaced by a new one under its name while its old content went
/// up (a save by replacing): the old row leaves, the new file goes up.
#[test]
fn a_file_replaced_mid_upload_leaves_and_the_new_one_goes_up() {
    let w = World::new(&[folder("D", "R", "d")]);
    stopped_at(&w, "d/big.bin", &large(), Fault::MidSession(1));
    w.remove_entry("d/big.bin");
    w.make_file("d/big.bin", b"the new version");
    let engine = w.run();
    // The new row may meet the old one's session still open (they run together): one backoff.
    w.h.block_on(engine.retry_now()).unwrap();
    w.h.drain(&engine);
    assert!(w.rows().is_empty(), "{:?}", w.rows());
    assert_eq!(w.cloud(|c| c.paths()), vec!["d", "d/big.bin"]);
    assert_eq!(w.content("d/big.bin").unwrap(), b"the new version");
    assert_committed(&w, "d/big.bin", "d/big.bin");
    assert_eq!(not_uploaded(&w).len(), 1);
}

// The file removed while its fragments go up stops the upload
// after the fragment in flight.

/// A file of `D` in OneDrive, downloaded here and edited to [`large`]
/// content: an `update` in fragments, queued.
fn edited_large(w: &World) {
    w.download("d/big.bin", b"old");
    w.write_file("d/big.bin", &large());
    assert_eq!(w.summary(), vec![(Update, "d/big.bin".into(), OutboxState::Ready)]);
}

fn with_old_file() -> World {
    World::new(&[folder("D", "R", "d"), file("A", "D", "big.bin", b"old")])
}

/// A new file removed while its first fragment goes up, the removal recorded (what a
/// move out of the folder is to the mount as well): no fragment after the one in flight,
/// the session cancelled, the rows gone, nothing of it in OneDrive.
#[test]
fn a_new_file_removed_mid_upload_stops_after_the_fragment_in_flight() {
    let w = World::new(&[folder("D", "R", "d")]);
    w.make_file("d/big.bin", &large());
    let engine = w.h.engine();
    drain_stopped_mid_request(&w, &engine, "PUT", "upload/", || w.remove_entry("d/big.bin"));
    w.h.drain(&engine);
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| (c.count("PUT", "upload/"), c.count("DELETE", "upload/"))), (1, 1), "no fragment after the one in flight; the session cancelled");
    assert_eq!(w.cloud(|c| c.paths()), vec!["d"]);
    assert!(w.cloud(|c| c.bin.is_empty()), "nothing reached OneDrive");
    assert_eq!(not_uploaded(&w), vec!["removed here before its upload finished"]);
}

/// A new version removed while its first fragment goes up, the removal
/// recorded (a `delete` behind the running `update`): the session is
/// cancelled, the update leaves with no event, and the delete behind it
/// sends the old version to the recycle bin.
#[test]
fn an_update_removed_mid_upload_ends_and_the_delete_behind_it_runs() {
    let w = with_old_file();
    edited_large(&w);
    let engine = w.h.engine();
    drain_stopped_mid_request(&w, &engine, "PUT", "upload/", || w.remove_entry("d/big.bin"));
    w.h.drain(&engine);
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| (c.count("PUT", "upload/"), c.count("DELETE", "upload/"))), (1, 1), "no fragment after the one in flight; the session cancelled");
    assert_eq!(w.cloud(|c| c.paths()), vec!["d"]);
    assert_eq!(w.cloud(|c| c.bin.get("A").and_then(|i| i.hash.clone())), Some(qx(b"old")), "the old version, to the recycle bin");
    assert_eq!(w.h.host.kinds(), vec![ActivityKind::CloudDeleted]);
}

/// An `update` whose file is gone when its run starts — its first run
/// stopped mid-session, or a store of the previous version holding it in
/// `retry`/`not-found` — ends, with no retry, its session cancelled; the
/// `delete` behind it runs.
#[test]
fn an_update_whose_file_is_gone_at_its_start_ends_and_the_delete_behind_it_runs() {
    for stuck in [false, true] {
        let w = with_old_file();
        edited_large(&w);
        let engine = w.h.engine();
        engine.arm(Fault::MidSession(1));
        w.h.drain(&engine);
        let session = w.rows()[0].session_url.clone().expect("a session is open");
        w.remove_entry("d/big.bin");
        assert_eq!(w.summary(), vec![(Update, "d/big.bin".into(), OutboxState::Running), (Delete, "d/big.bin".into(), OutboxState::Ready)]);
        if stuck {
            let seq = w.rows()[0].seq;
            w.store.call_blocking(move |s| s.outbox_set_state(seq, OutboxState::Retry, Some(&Reason::NotFound), Some(0))).unwrap();
        }

        let from = w.cloud(|c| c.log.len());
        w.run();
        assert!(w.rows().is_empty(), "stuck {stuck}: {:?}", w.summary());
        let sid = session.as_str().rsplit('/').next().unwrap();
        assert_eq!(
            requests_since(&w, from),
            vec![("DELETE".to_owned(), format!("upload/{sid}")), ("DELETE".to_owned(), "me/drive/items/A".to_owned())],
            "stuck {stuck}"
        );
        assert!(w.cloud(|c| c.bin.contains_key("A")), "stuck {stuck}");
        assert_eq!(w.h.host.kinds(), vec![ActivityKind::CloudDeleted], "stuck {stuck}");
    }
}

/// A new file renamed inside the folder while its first fragment goes up,
/// the rename recorded: it is found under its new name, and its upload goes
/// on to the end in the same session.
#[test]
fn a_file_renamed_mid_upload_with_the_rename_recorded_goes_on() {
    let w = World::new(&[folder("D", "R", "d")]);
    w.make_file("d/big.bin", &large());
    let engine = w.h.engine();
    drain_stopped_mid_request(&w, &engine, "PUT", "upload/", || w.rename_entry("d/big.bin", "d/moved.bin"));
    w.h.drain(&engine);
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| (c.count("POST", "createUploadSession"), c.count("PUT", "upload/"))), (1, 4), "one session, to the end");
    assert_eq!(w.cloud(|c| c.paths()), vec!["d", "d/moved.bin"]);
    assert_eq!(w.content("d/moved.bin").unwrap(), large());
    assert_committed(&w, "d/moved.bin", "d/moved.bin");
    assert!(not_uploaded(&w).is_empty());
}

/// A new file replaced by another under its name while its only fragment goes up (a save
/// by replacing: the old one removed, a new one made, both recorded): what was sent is
/// committed as the file that was sent and deleted in OneDrive by the removal behind it,
/// and the new file goes up under the name. OneDrive ends with the new content alone.
#[test]
fn a_file_replaced_while_its_create_goes_up_ends_as_the_new_file_in_onedrive() {
    let w = World::new(&[folder("D", "R", "d")]);
    w.make_file("d/n.bin", b"first content");
    let upload = InFlight::start(&w, "PUT", "upload/");
    w.remove_entry("d/n.bin");
    w.make_file("d/n.bin", b"the second");
    upload.finish(&w);
    let engine = w.run();
    w.h.block_on(engine.retry_now()).unwrap();
    w.h.drain(&engine);
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["d", "d/n.bin"]);
    assert_eq!(w.content("d/n.bin").unwrap(), b"the second");
    assert_committed(&w, "d/n.bin", "d/n.bin");
    assert_eq!(conflict_count(&w), 0, "never a copy of its own upload");
}

fn conflict_count(w: &World) -> usize {
    w.h.host.kinds().iter().filter(|k| **k == ActivityKind::Conflict).count()
}
