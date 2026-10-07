//! Defects the review of 2026-10-03 found (`docs/quality/upload.md`), each shown by a test
//! that says what the worker should do.

use konedrive_tree::ActivityKind;
use super::*;

/// A `403` blocks its own row (`forbidden`) and nothing else: the other
/// rows go on. In the daemon, what a sign-in (after the sign-out it needs)
/// gives the folder is a worker built anew, and all it is told afterwards is
/// `Refresh()` and that a cycle went through. That lets the row go again.
#[test]
fn a_row_blocked_by_403_goes_again_with_the_worker_a_sign_in_builds() {
    let w = World::new(&[]);
    w.write("a.txt", b"a");
    w.examine(&[("", "a.txt")]);
    w.cloud(|c| c.script("POST", "createUploadSession", ResponseTemplate::new(403), 1));
    let engine = w.h.engine();
    w.h.drain(&engine);
    assert_eq!(reason_of(&w, "a.txt").as_deref(), Some(Reason::Forbidden.key()));
    assert_eq!(w.h.host.kinds().iter().filter(|k| **k == ActivityKind::UploadFailed).count(), 1, "the refusal is said");

    // The same worker goes on with the other rows, and leaves the blocked one.
    w.write("b.txt", b"b");
    w.examine(&[("", "b.txt")]);
    w.h.drain(&engine);
    assert_eq!(w.summary(), vec![(Create, "a.txt".into(), OutboxState::Blocked)]);
    assert_committed(&w, "b.txt", "b.txt");

    // Signed out and in again: the folder left read-write and came back, with a new worker.
    let rebuilt = w.h.engine();
    w.h.block_on(rebuilt.cycle_done());
    w.h.block_on(rebuilt.retry_now()).unwrap();
    w.h.drain(&rebuilt);
    assert!(w.rows().is_empty(), "the row a 403 blocked is sent again after the sign-in: {:?}, {:?}", w.summary(), reason_of(&w, "a.txt"));
    assert_committed(&w, "a.txt", "a.txt");
}

/// A worker that may not send — here, the user's pause — leaves the row
/// a `403` blocked as it is, listed among what needs the user; it lets it go
/// once it may send.
#[test]
fn a_row_blocked_by_403_stays_blocked_while_the_new_worker_may_not_send() {
    let w = World::new(&[]);
    w.write("a.txt", b"a");
    w.examine(&[("", "a.txt")]);
    w.cloud(|c| c.script("POST", "createUploadSession", ResponseTemplate::new(403), 1));
    w.run();
    assert_eq!(w.summary(), vec![(Create, "a.txt".into(), OutboxState::Blocked)]);

    let rebuilt = w.h.engine();
    pause(&w);
    w.h.drain(&rebuilt);
    assert_eq!(w.summary(), vec![(Create, "a.txt".into(), OutboxState::Blocked)]);
    assert_eq!(rebuilt.status().counts.blocked, 1);

    resume(&w);
    w.h.drain(&rebuilt);
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_committed(&w, "a.txt", "a.txt");
}

/// OneDrive answers a new file's upload with an item that holds other
/// content, and the delete of that item fails: the row remembers the item,
/// to delete it before the file goes again. A second failure of that delete
/// must not make the row forget it: once OneDrive answers again, the bad
/// item goes and the file lands under its own name — never as a conflict
/// copy beside the worker's own bad upload.
#[test]
fn a_bad_upload_whose_delete_fails_twice_is_still_deleted_before_the_file_goes_again() {
    let w = World::new(&[]);
    w.write("a.txt", b"what the user wrote");
    w.examine(&[("", "a.txt")]);
    let bad = serde_json::json!({
        "id": "BAD",
        "name": "a.txt",
        "eTag": "e-BAD",
        "cTag": "c-BAD",
        "size": 5,
        "parentReference": { "id": "R", "driveId": "D" },
        "file": { "hashes": { "quickXorHash": qx(b"other") } },
    });
    w.cloud(|c| {
        c.script("PUT", "upload/", ResponseTemplate::new(201).set_body_json(bad), 1);
        // The delete in the run that found it, and the one in the run after.
        c.script("DELETE", "items/BAD", ResponseTemplate::new(502), 2);
    });
    let engine = w.h.engine();
    w.h.drain(&engine);
    // The drive as that answer says it is: the session over, the item there.
    w.cloud(|c| {
        c.expire_sessions();
        c.add_file("BAD", "R", "a.txt", b"other");
    });
    assert_eq!((reason_of(&w, "a.txt").as_deref(), bad_item_of(&w, "a.txt").as_deref()), (Some(Reason::Hash.key()), Some("BAD")), "the bad item is remembered");

    // The delete fails once more.
    w.h.block_on(engine.retry_now()).unwrap();
    w.h.drain(&engine);
    assert_eq!(w.cloud(|c| c.count("DELETE", "items/BAD")), 2);
    assert_eq!(w.cloud(|c| c.paths()), vec!["a.txt"], "nothing else was sent meanwhile");
    assert_eq!((reason_of(&w, "a.txt").as_deref(), bad_item_of(&w, "a.txt").as_deref()), (Some(Reason::Network.key()), Some("BAD")), "and still is, under another reason");

    // OneDrive answers again.
    w.h.block_on(engine.retry_now()).unwrap();
    w.h.drain(&engine);
    let conflicts = w.h.host.kinds().iter().filter(|k| **k == ActivityKind::Conflict).count();
    assert_eq!(
        (w.cloud(|c| c.paths()), conflicts, w.path("a.txt").exists()),
        (vec!["a.txt".to_owned()], 0, true),
        "the file lands under its own name, with no conflict copy: {:?}",
        w.summary()
    );
    assert_eq!(w.content("a.txt").unwrap(), b"what the user wrote");
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_committed(&w, "a.txt", "a.txt");
}

/// The bad item is deleted only while its content is what the upload
/// left: the cTag its answer gave says so. Edited in OneDrive since (another
/// device, the web), it is someone's now: left there and forgotten, and the
/// file goes up beside it as it does beside any other holder of its name.
#[test]
fn a_bad_upload_changed_in_onedrive_since_is_left_there() {
    let w = World::new(&[]);
    w.write("a.txt", b"what the user wrote");
    w.examine(&[("", "a.txt")]);
    let bad = serde_json::json!({
        "id": "BAD",
        "name": "a.txt",
        "eTag": "e-BAD",
        "cTag": "c-BAD",
        "size": 5,
        "parentReference": { "id": "R", "driveId": "D" },
        "file": { "hashes": { "quickXorHash": qx(b"other") } },
    });
    w.cloud(|c| {
        c.script("PUT", "upload/", ResponseTemplate::new(201).set_body_json(bad), 1);
        c.script("DELETE", "items/BAD", ResponseTemplate::new(502), 1);
    });
    let engine = w.h.engine();
    w.h.drain(&engine);
    w.cloud(|c| {
        c.expire_sessions();
        c.add_file("BAD", "R", "a.txt", b"other");
        c.edit("BAD", b"edited elsewhere");
    });
    assert_eq!(bad_item_of(&w, "a.txt").as_deref(), Some("BAD"));

    w.h.block_on(engine.retry_now()).unwrap();
    w.h.drain(&engine);
    assert_eq!(w.cloud(|c| c.count("DELETE", "items/BAD")), 1, "no second delete: its cTag is another");
    assert_eq!(w.cloud(|c| c.item("BAD").map(|i| i.content.clone())), Some(b"edited elsewhere".to_vec()), "what was edited elsewhere stays");
    assert_eq!(w.cloud(|c| c.at("a.txt").map(|i| i.id.clone())).as_deref(), Some("BAD"));
    assert!(w.rows().iter().all(|r| bad_item_of(&w, &r.rel.display().to_string()).is_none()), "forgotten: {:?}", w.summary());
    assert_eq!(w.h.host.kinds().iter().filter(|k| **k == ActivityKind::Conflict).count(), 1, "the file goes up as a copy beside it: {:?}", w.summary());
}

/// OneDrive moves an item's eTag by itself, with its content as it
/// was: the bad item is still the worker's own upload, and is deleted — the
/// file lands under its own name, with no conflict copy.
#[test]
fn a_bad_upload_whose_etag_moved_by_itself_is_still_deleted() {
    let w = World::new(&[]);
    w.write("a.txt", b"what the user wrote");
    w.examine(&[("", "a.txt")]);
    let bad = serde_json::json!({
        "id": "BAD",
        "name": "a.txt",
        "eTag": "e-BAD",
        "cTag": "c-BAD",
        "size": 5,
        "parentReference": { "id": "R", "driveId": "D" },
        "file": { "hashes": { "quickXorHash": qx(b"other") } },
    });
    w.cloud(|c| {
        c.script("PUT", "upload/", ResponseTemplate::new(201).set_body_json(bad), 1);
        c.script("DELETE", "items/BAD", ResponseTemplate::new(502), 1);
    });
    let engine = w.h.engine();
    w.h.drain(&engine);
    w.cloud(|c| {
        c.expire_sessions();
        c.add_file("BAD", "R", "a.txt", b"other");
        // The same place and content: only the eTag moves.
        c.rename("BAD", "R", "a.txt");
    });
    assert_eq!(w.cloud(|c| c.item("BAD").map(|i| (i.etag != "e-BAD", i.ctag == "c-BAD"))), Some((true, true)));

    w.h.block_on(engine.retry_now()).unwrap();
    w.h.drain(&engine);
    let conflicts = w.h.host.kinds().iter().filter(|k| **k == ActivityKind::Conflict).count();
    assert_eq!((w.cloud(|c| c.paths()), conflicts), (vec!["a.txt".to_owned()], 0), "{:?}", w.rows());
    assert!(w.cloud(|c| c.item("BAD").is_none()), "the bad upload is gone");
    assert!(w.rows().is_empty(), "{:?}", w.rows());
    assert_committed(&w, "a.txt", "a.txt");
}

/// The item a row's bad upload left in OneDrive, as the store remembers it.
fn bad_item_of(w: &World, rel: &str) -> Option<String> {
    let seq = w.rows().into_iter().find(|r| r.rel == Path::new(rel))?.seq;
    konedrive_tree::off_runtime(|| w.store.call_blocking(move |s| s.outbox_bad_item(seq))).unwrap().map(|bad| bad.id)
}
