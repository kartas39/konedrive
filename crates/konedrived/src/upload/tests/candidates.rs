//! Defects the review of 2026-10-03 found (`docs/quality/upload.md`), each shown by a test
//! that says what the worker should do.

use super::*;

/// UP1. A `403` blocks its own row (`forbidden`) and nothing else: the other
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
    assert_eq!(reason_of(&w, "a.txt").as_deref(), Some(reason::FORBIDDEN));
    assert_eq!(w.h.host.kinds().iter().filter(|k| *k == kind::UPLOAD_FAILED).count(), 1, "the refusal is said");

    // The same worker goes on with the other rows, and leaves the blocked one.
    w.write("b.txt", b"b");
    w.examine(&[("", "b.txt")]);
    w.h.drain(&engine);
    assert!(!engine.status().needs_sign_in);
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

/// UP1. A worker that may not send — here, the user's pause — leaves the row
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
    pause(&rebuilt);
    w.h.drain(&rebuilt);
    assert_eq!(w.summary(), vec![(Create, "a.txt".into(), OutboxState::Blocked)]);
    assert_eq!(rebuilt.status().counts.blocked, 1);

    resume(&rebuilt);
    w.h.drain(&rebuilt);
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_committed(&w, "a.txt", "a.txt");
}
