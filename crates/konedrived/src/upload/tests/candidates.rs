//! Candidates for defects of the review of 2026-10-03 (`docs/quality/upload.md`), each shown
//! by a test that says what the worker should do.

use super::*;

/// UP2. OneDrive answers a new file's upload with an item that holds other
/// content, and the delete of that item fails: the row remembers the item,
/// to delete it before the file goes again. A second failure of that delete
/// must not make the row forget it: once OneDrive answers again, the bad
/// item goes and the file lands under its own name — never as a conflict
/// copy beside the worker's own bad upload.
#[test]
#[ignore = "shows UP2: the bad upload's id is lost, and the file becomes a conflict copy"]
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
    assert_eq!(reason_of(&w, "a.txt").as_deref(), Some("hash-mismatch:BAD"), "the bad item is remembered");

    // The delete fails once more.
    w.h.block_on(engine.retry_now()).unwrap();
    w.h.drain(&engine);
    assert_eq!(w.cloud(|c| c.count("DELETE", "items/BAD")), 2);
    assert_eq!(w.cloud(|c| c.paths()), vec!["a.txt"], "nothing else was sent meanwhile");

    // OneDrive answers again.
    w.h.block_on(engine.retry_now()).unwrap();
    w.h.drain(&engine);
    let conflicts = w.h.host.kinds().iter().filter(|k| *k == kind::CONFLICT).count();
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

/// UP1. A `403` blocks its row (`forbidden`) and stops the worker until the
/// account has signed in again. In the daemon nothing calls
/// `OutboxWorker::signed_in`: what a sign-in (after the sign-out it needs)
/// gives the folder is a worker built anew, and all it is told afterwards is
/// `Refresh()` and that a cycle went through. That must let the row go
/// again.
#[test]
#[ignore = "shows UP1: a row blocked by a 403 never goes again without OutboxWorker::signed_in"]
fn a_row_blocked_by_403_goes_again_with_the_worker_a_sign_in_builds() {
    let w = World::new(&[]);
    w.write("a.txt", b"a");
    w.examine(&[("", "a.txt")]);
    w.cloud(|c| c.script("POST", "createUploadSession", ResponseTemplate::new(403), 1));
    let engine = w.h.engine();
    w.h.drain(&engine);
    assert!(engine.status().needs_sign_in);
    assert_eq!(reason_of(&w, "a.txt").as_deref(), Some(reason::FORBIDDEN));

    // Signed out and in again: the folder left read-write and came back, with a new worker.
    let rebuilt = w.h.engine();
    w.h.block_on(rebuilt.cycle_done());
    w.h.block_on(rebuilt.retry_now()).unwrap();
    w.h.drain(&rebuilt);
    assert!(!rebuilt.status().needs_sign_in);
    assert!(w.rows().is_empty(), "the row a 403 blocked is sent again after the sign-in: {:?}, {:?}", w.summary(), reason_of(&w, "a.txt"));
    assert_committed(&w, "a.txt", "a.txt");
}
