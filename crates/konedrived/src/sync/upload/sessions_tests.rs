//! Upload sessions and the empty placeholder an open one holds its name with
//! in OneDrive (issue #47): every session persisted before its first byte, a
//! refused fragment sent again to the same session, a session given up
//! always cancelled — so our own placeholder never becomes a conflict.

use super::*;

/// Larger than one request: four fragments of 320 KiB.
fn large() -> Vec<u8> {
    (0..(1024 * 1024 + 77)).map(|i| (i % 251) as u8).collect()
}

fn conflicts(w: &World) -> usize {
    w.h.host.kinds().iter().filter(|k| *k == kind::CONFLICT).count()
}

fn given_up(w: &World) -> Vec<String> {
    w.store.call_blocking(|s| s.upload_sessions_given_up(10)).unwrap()
}

/// `a.txt` queued, and its one fragment refused `429` as often as the
/// client sends it: the row fails for now, keeping its session, whose
/// placeholder holds the name.
fn refused_for_now(w: &World, content: &[u8]) -> String {
    w.write("a.txt", content);
    w.examine(&[("", "a.txt")]);
    w.cloud(|c| c.throttle_429("PUT", "upload/", 0, 0, 2));
    w.run();
    let row = w.rows().remove(0);
    assert_eq!(row.state, OutboxState::Ready, "throttled: {:?}", row.reason);
    assert_eq!(w.cloud(|c| c.placeholders()), vec!["a.txt"]);
    row.session_url.expect("the session is kept for the next run")
}

/// The bug as it happened: a small file's one `PUT` answered `429`. Refused
/// once, it goes again to the same session at once; refused as often as the
/// client allows, the row fails for now and its next run resumes the same
/// session. Either way the file lands under its own name: one session, no
/// conflict, no placeholder left.
#[test]
fn a_small_file_refused_429_goes_up_under_its_own_name() {
    for refusals in [1, 2] {
        let w = World::new(&[]);
        w.write("a.txt", b"hello");
        w.examine(&[("", "a.txt")]);
        w.cloud(|c| c.throttle_429("PUT", "upload/", 0, 0, refusals));
        w.run();
        if refusals == 2 {
            assert!(w.rows()[0].session_url.is_some(), "kept for the next run");
            w.run();
        }
        assert!(w.rows().is_empty(), "{refusals}: {:?}", w.summary());
        assert_eq!(w.cloud(|c| c.paths()), vec!["a.txt"], "{refusals}");
        assert_eq!(w.content("a.txt").unwrap(), b"hello");
        assert_eq!(w.cloud(|c| c.count("POST", "createUploadSession")), 1, "{refusals}: one session");
        assert_eq!(w.cloud(|c| (c.placeholders().len(), c.open_sessions())), (0, 0), "{refusals}");
        assert_eq!(conflicts(&w), 0, "{refusals}");
        assert!(given_up(&w).is_empty());
        assert_committed(&w, "a.txt", "a.txt");
    }
}

/// The same for a fragment in the middle of a large file: sent again to the
/// same session, and the upload goes on.
#[test]
fn a_large_files_fragment_refused_429_goes_again_to_the_same_session() {
    let w = World::new(&[]);
    let content = large();
    w.write("big.bin", &content);
    w.examine(&[("", "big.bin")]);
    w.cloud(|c| c.throttle_429("PUT", "upload/", 1, 0, 1));
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["big.bin"]);
    assert_eq!(w.content("big.bin").unwrap(), content);
    assert_eq!(w.cloud(|c| (c.count("POST", "createUploadSession"), c.count("PUT", "upload/"))), (1, 5), "four fragments, the second twice");
    assert_eq!(w.cloud(|c| (c.placeholders().len(), c.open_sessions())), (0, 0));
    assert_eq!(conflicts(&w), 0);
}

/// A restart with a small file's session persisted: resumed while the file
/// is the content it was opened for, cancelled when the file changed — and
/// no placeholder left either way.
#[test]
fn a_restart_resumes_or_cancels_a_small_files_session() {
    for changed in [false, true] {
        let w = World::new(&[]);
        let session = refused_for_now(&w, b"hello");
        // As a crash leaves it.
        let seq = w.rows()[0].seq;
        w.store.call_blocking(move |s| s.outbox_set_state(seq, OutboxState::Running, None, None)).unwrap();
        if changed {
            w.write("a.txt", b"hello again");
        }
        w.run();
        assert!(w.rows().is_empty(), "changed {changed}: {:?}", w.summary());
        let sid = session.rsplit('/').next().unwrap().to_owned();
        let (opened, cancelled) = w.cloud(|c| (c.count("POST", "createUploadSession"), c.count("DELETE", &format!("upload/{sid}"))));
        assert_eq!((opened, cancelled), if changed { (2, 1) } else { (1, 0) }, "changed {changed}");
        assert_eq!(w.content("a.txt").unwrap(), if changed { &b"hello again"[..] } else { &b"hello"[..] });
        assert_eq!(w.cloud(|c| (c.paths(), c.placeholders().len(), c.open_sessions())), (vec!["a.txt".to_owned()], 0, 0), "changed {changed}");
        assert_eq!(conflicts(&w), 0, "changed {changed}");
        assert!(given_up(&w).is_empty());
    }
}

/// A session given up because its file was removed is cancelled: the name is
/// free again in OneDrive.
#[test]
fn a_removed_files_session_is_cancelled() {
    let w = World::new(&[]);
    refused_for_now(&w, b"hello");
    std::fs::remove_file(w.path("a.txt")).unwrap();
    w.examine(&[("", "a.txt")]);
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| (c.paths(), c.placeholders(), c.open_sessions())), (Vec::<String>::new(), Vec::new(), 0));
    assert!(given_up(&w).is_empty());
}

/// A `409` from the placeholder of the row's own earlier session — given up
/// when the file changed, its cancel failed — cancels that session and
/// creates the file again: no conflict copy.
#[test]
fn a_409_from_our_own_sessions_placeholder_cancels_it_and_no_copy_is_made() {
    let w = World::new(&[]);
    let session = refused_for_now(&w, b"hello");
    w.write("a.txt", b"hello again");
    w.cloud(|c| c.script("DELETE", "upload/", ResponseTemplate::new(500), 1));
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["a.txt"]);
    assert_eq!(w.content("a.txt").unwrap(), b"hello again");
    let sid = session.rsplit('/').next().unwrap().to_owned();
    assert_eq!(w.cloud(|c| c.count("DELETE", &format!("upload/{sid}"))), 2, "the cancel that failed, then the one after the 409");
    assert_eq!(w.cloud(|c| (c.placeholders().len(), c.open_sessions())), (0, 0));
    assert_eq!(conflicts(&w), 0);
    assert!(!w.path("a-fedora.txt").exists());
    assert!(given_up(&w).is_empty());
}

/// A cancel that fails keeps the session on the list after its row has
/// left the outbox; a later run cancels it.
#[test]
fn a_failed_cancel_is_tried_again_after_the_row_left() {
    let w = World::new(&[]);
    let session = refused_for_now(&w, b"hello");
    std::fs::remove_file(w.path("a.txt")).unwrap();
    w.examine(&[("", "a.txt")]);
    w.cloud(|c| c.script("DELETE", "upload/", ResponseTemplate::new(500), 1));
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(given_up(&w), vec![session.clone()], "still to be cancelled");
    assert_eq!(w.cloud(|c| c.placeholders()), vec!["a.txt"]);

    w.run();
    assert!(given_up(&w).is_empty());
    assert_eq!(w.cloud(|c| (c.placeholders().len(), c.open_sessions())), (0, 0));
    let sid = session.rsplit('/').next().unwrap().to_owned();
    assert_eq!(w.cloud(|c| c.count("DELETE", &format!("upload/{sid}"))), 2);
}

/// A `409` from another file — no session of ours holds the name — is still
/// a conflict: the local file is kept as a copy beside it.
#[test]
fn a_409_from_another_file_still_makes_a_copy() {
    let w = World::new(&[]);
    w.write("a.txt", b"mine");
    w.examine(&[("", "a.txt")]);
    w.cloud(|c| c.add_file("X", "R", "a.txt", b"theirs"));
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["a-fedora.txt", "a.txt"]);
    assert_eq!(w.content("a.txt").unwrap(), b"theirs");
    assert_eq!(w.content("a-fedora.txt").unwrap(), b"mine");
    assert_eq!(conflicts(&w), 1);
    assert_eq!(w.cloud(|c| (c.placeholders().len(), c.open_sessions())), (0, 0));
}
