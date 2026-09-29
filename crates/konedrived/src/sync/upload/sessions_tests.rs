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

/// `a.txt` queued, and the daemon stopped between opening its session and
/// persisting it (issue #84): the session's URL is lost, its place recorded,
/// its placeholder holds the name.
fn opened_and_lost(w: &World, content: &[u8]) {
    w.write("a.txt", content);
    w.examine(&[("", "a.txt")]);
    let engine = w.h.engine();
    engine.arm(Fault::SessionNotPersisted);
    w.h.drain(&engine);
    assert_eq!(w.rows()[0].session_url, None);
    assert_eq!(w.cloud(|c| c.placeholders()), vec!["a.txt"]);
}

fn opening_at(w: &World, name: &str) -> Option<i64> {
    let name = name.to_owned();
    w.store.call_blocking(move |s| s.upload_opening_at(fake::ROOT, &name)).unwrap()
}

/// A row waiting in backoff, due now.
fn due(w: &World) {
    let seq = w.rows()[0].seq;
    w.store.call_blocking(move |s| s.outbox_set_state(seq, OutboxState::Ready, None, None)).unwrap();
}

/// OneDrive refuses to delete the lost session's placeholder: the row waits
/// (`upload-session-open`), no copy is made; once the session expires and
/// frees the name, the file goes up under it.
#[test]
fn a_placeholder_onedrive_will_not_delete_makes_the_row_wait_not_a_copy() {
    let w = World::new(&[]);
    opened_and_lost(&w, b"hello");
    w.cloud(|c| c.refuse_placeholder_delete = true);
    w.run();
    assert_eq!(w.rows().len(), 1, "{:?}", w.summary());
    assert_eq!(w.rows()[0].reason.as_deref(), Some(reason::SESSION_OPEN));
    assert_eq!(w.cloud(|c| c.placeholders()), vec!["a.txt"]);
    assert_eq!(conflicts(&w), 0);
    assert!(!w.path("a-fedora.txt").exists());

    w.cloud(|c| c.expire_sessions());
    due(&w);
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["a.txt"]);
    assert_eq!(w.content("a.txt").unwrap(), b"hello");
    assert_eq!(conflicts(&w), 0);
    assert_committed(&w, "a.txt", "a.txt");
}

/// A `409` to a fresh opening from a file the delta feed listed (as
/// `x.txt`, renamed in OneDrive since), with content or empty, however old:
/// the opening's record goes with the `409`, and the file is someone else's —
/// a conflict copy, as for any `409`. Nothing of theirs is deleted.
#[test]
fn a_409_to_a_fresh_opening_from_a_listed_file_is_still_a_conflict() {
    for (theirs, age) in [(&b"theirs"[..], 0), (&b""[..], 3600)] {
        let w = World::new(&[file("X", "R", "x.txt", theirs)]);
        w.write("a.txt", b"mine");
        w.examine(&[("", "a.txt")]);
        w.cloud(|c| {
            c.edit("X", theirs);
            c.rename("X", fake::ROOT, "a.txt");
            c.created.insert("X".into(), crate::sync::activity::unix_now() - age);
        });
        w.run();
        assert!(w.rows().is_empty(), "{age}: {:?}", w.summary());
        assert_eq!(w.cloud(|c| c.paths()), vec!["a-fedora.txt", "a.txt"], "{age}");
        assert_eq!(w.content("a.txt").unwrap(), theirs, "{age}");
        assert_eq!(w.id_at("a.txt").as_deref(), Some("X"), "{age}: never deleted");
        assert_eq!(w.content("a-fedora.txt").unwrap(), b"mine");
        assert_eq!(conflicts(&w), 1, "{age}");
    }
}

/// The recorded place outlasts a restart, and goes when its row leaves the
/// outbox — committed, or never uploaded.
#[test]
fn a_recorded_opening_outlasts_a_restart_and_goes_with_its_row() {
    for removed in [false, true] {
        let w = World::new(&[]);
        opened_and_lost(&w, b"hello");
        let recorded = opening_at(&w, "A.TXT").expect("recorded, compared without case");
        // A new start on the same store.
        let _ = w.h.engine();
        assert_eq!(opening_at(&w, "a.txt"), Some(recorded), "removed {removed}");
        if removed {
            std::fs::remove_file(w.path("a.txt")).unwrap();
            w.examine(&[("", "a.txt")]);
        }
        w.run();
        assert!(w.rows().is_empty(), "removed {removed}: {:?}", w.summary());
        assert_eq!(opening_at(&w, "a.txt"), None, "removed {removed}");
        assert_eq!(conflicts(&w), 0);
    }
}

/// Issue #89: a name held by the placeholder of an upload session nothing
/// here recorded — another device's, or one an older version abandoned.
/// For a new file, a rename onto the name, and a folder's `mkdir`: the row
/// waits (`name-held-by-an-upload`), no copy is made and the placeholder is
/// never deleted (a delete would end that session). Once the session
/// completes with other content, that is someone else's file: a copy, as for
/// any `409`. Once it is cancelled, the row goes through under its name.
#[test]
fn a_name_held_by_an_unknown_placeholder_waits_and_is_never_a_copy() {
    for what in ["create", "rename", "mkdir"] {
        for completed in [false, true] {
            let at = format!("{what} completed {completed}");
            let w = World::new(&[file("A", "R", "a.txt", b"old")]);
            let (name, copy_name) = if what == "mkdir" { ("dir", "dir-fedora") } else { ("b.txt", "b-fedora.txt") };
            let sid = w.cloud(|c| c.open_elsewhere(fake::ROOT, name, 3600));
            match what {
                "create" => {
                    w.write("b.txt", b"mine");
                    w.examine(&[("", "b.txt")]);
                }
                "rename" => {
                    w.rename("a.txt", "b.txt");
                    w.examine(&[("", "a.txt"), ("", "b.txt")]);
                }
                _ => {
                    std::fs::create_dir(w.path("dir")).unwrap();
                    w.examine(&[("", "dir")]);
                }
            }
            w.run();
            assert_eq!(w.rows().len(), 1, "{at}: {:?}", w.summary());
            assert_eq!(w.rows()[0].reason.as_deref(), Some(reason::NAME_HELD), "{at}");
            assert_eq!(w.cloud(|c| c.placeholders()), vec![name.to_owned()], "{at}: never deleted");
            assert_eq!(w.cloud(|c| c.open_sessions()), 1, "{at}: its session goes on");
            assert_eq!(conflicts(&w), 0, "{at}");
            assert!(!w.path(copy_name).exists(), "{at}");

            if completed {
                w.cloud(|c| c.complete_elsewhere(&sid, b"theirs"));
            } else {
                w.cloud(|c| c.cancel_elsewhere(&sid));
            }
            due(&w);
            w.run();
            assert!(w.rows().is_empty(), "{at}: {:?}", w.summary());
            if completed {
                assert_eq!(w.content(name).unwrap(), b"theirs", "{at}");
                assert!(w.id_at(copy_name).is_some(), "{at}: kept beside it");
                assert_eq!(conflicts(&w), 1, "{at}");
            } else {
                assert!(w.id_at(name).is_some(), "{at}");
                assert_eq!(conflicts(&w), 0, "{at}");
                if what != "rename" {
                    assert_committed(&w, name, name);
                }
            }
        }
    }
}

/// An empty file the delta feed listed is a real file, never waited for: a
/// new file at its name (it was renamed there in OneDrive, the feed not
/// brought yet) is a conflict copy, as today.
#[test]
fn an_empty_file_the_feed_listed_is_still_a_conflict() {
    let w = World::new(&[file("X", "R", "x.txt", b"")]);
    w.write("a.txt", b"mine");
    w.examine(&[("", "a.txt")]);
    w.cloud(|c| c.rename("X", fake::ROOT, "a.txt"));
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["a-fedora.txt", "a.txt"]);
    assert_eq!(w.id_at("a.txt").as_deref(), Some("X"));
    assert_eq!(w.content("a-fedora.txt").unwrap(), b"mine");
    assert_eq!(conflicts(&w), 1);
}

/// An empty local file over an unknown placeholder: the same content (both
/// empty), adopted as today — no wait, no copy, nothing deleted.
#[test]
fn an_empty_file_over_an_unknown_placeholder_is_adopted() {
    let w = World::new(&[]);
    w.cloud(|c| c.open_elsewhere(fake::ROOT, "a.txt", 3600));
    w.write("a.txt", b"");
    w.examine(&[("", "a.txt")]);
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(conflicts(&w), 0);
    assert_eq!(w.cloud(|c| c.count("DELETE", "items/")), 0);
    assert!(!w.path("a-fedora.txt").exists());
}

/// Another device opened a session at the name a few seconds before this
/// folder's create (issue #89): the create's own opening got `409`, so it
/// made no placeholder, and its record goes — the holder is never taken for
/// ours, never deleted. The row waits; that device's session goes on.
#[test]
fn a_placeholder_opened_elsewhere_just_before_is_never_deleted() {
    let w = World::new(&[]);
    w.cloud(|c| c.open_elsewhere(fake::ROOT, "a.txt", 5));
    w.write("a.txt", b"mine");
    w.examine(&[("", "a.txt")]);
    w.run();
    assert_eq!(w.rows().len(), 1, "{:?}", w.summary());
    assert_eq!(w.rows()[0].reason.as_deref(), Some(reason::NAME_HELD));
    assert_eq!(opening_at(&w, "a.txt"), None, "a 409 to this opening clears its record");
    assert_eq!(w.cloud(|c| (c.placeholders(), c.open_sessions())), (vec!["a.txt".to_owned()], 1));
    assert_eq!(w.cloud(|c| c.count("DELETE", "items/")), 0);
    assert_eq!(conflicts(&w), 0);
}

/// A changed file also renamed onto a name an unknown placeholder holds
/// (the move before the content, `update`): the row waits, no copy, nothing
/// deleted; once the name is free, both go through.
#[test]
fn a_changed_and_renamed_file_waits_for_a_held_name() {
    let w = World::new(&[file("A", "R", "a.txt", b"old")]);
    w.hydrate("a.txt", b"old");
    w.edit("a.txt", b"mine");
    w.rename("a.txt", "b.txt");
    w.examine(&[("", "a.txt"), ("", "b.txt")]);
    let sid = w.cloud(|c| c.open_elsewhere(fake::ROOT, "b.txt", 3600));
    w.run();
    let rows = w.rows();
    assert_eq!(rows.len(), 1, "{:?}", w.summary());
    assert_eq!(rows[0].kind, Update, "one update carrying the move");
    assert_eq!(rows[0].reason.as_deref(), Some(reason::NAME_HELD));
    assert_eq!(w.cloud(|c| (c.placeholders(), c.open_sessions())), (vec!["b.txt".to_owned()], 1));
    assert_eq!(w.cloud(|c| c.count("DELETE", "items/")), 0);
    assert_eq!(conflicts(&w), 0);

    w.cloud(|c| c.cancel_elsewhere(&sid));
    due(&w);
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.id_at("b.txt").as_deref(), Some("A"));
    assert_eq!(w.content("b.txt").unwrap(), b"mine");
    assert_eq!(conflicts(&w), 0);
}

/// A move and a `mkdir` onto a name this folder's own listed session holds
/// (another row still sending it; the name differs only in case, one name
/// to OneDrive): the row waits, and that session is left to finish.
#[test]
fn a_move_or_mkdir_onto_our_own_sessions_name_waits() {
    for what in ["move", "mkdir"] {
        let w = World::new(&[file("X", "R", "x.txt", b"old")]);
        let session = refused_for_now(&w, b"hello");
        if what == "move" {
            w.rename("x.txt", "A.TXT");
            w.examine(&[("", "x.txt"), ("", "A.TXT")]);
        } else {
            std::fs::create_dir(w.path("A.TXT")).unwrap();
            w.examine(&[("", "A.TXT")]);
        }
        // The other row is refused again: its session stays open.
        w.cloud(|c| c.throttle_429("PUT", "upload/", 0, 0, 2));
        w.run();
        let rows = w.rows();
        assert_eq!(rows.len(), 2, "{what}: {:?}", w.summary());
        let mine = rows.iter().find(|r| r.rel.to_str() == Some("A.TXT")).unwrap();
        assert_eq!(mine.reason.as_deref(), Some(reason::NAME_HELD), "{what}");
        let other = rows.iter().find(|r| r.rel.to_str() == Some("a.txt")).unwrap();
        assert_eq!(other.session_url.as_deref(), Some(session.as_str()), "{what}: its session untouched");
        assert_eq!(w.cloud(|c| (c.placeholders(), c.open_sessions())), (vec!["a.txt".to_owned()], 1), "{what}");
        assert_eq!(w.cloud(|c| (c.count("DELETE", "upload/"), c.count("DELETE", "items/"))), (0, 0), "{what}");
        assert_eq!(conflicts(&w), 0, "{what}");
    }
}

/// The record of an opening: made (`false`) or carried (`true`) — the same
/// row at the same place, the name compared without case, keeping its first
/// time; at another place it starts again.
#[test]
fn an_opening_record_is_carried_at_the_same_place_without_case() {
    let w = World::new(&[]);
    let record = |name: &'static str, at: i64| w.store.call_blocking(move |s| s.outbox_record_opening(7, fake::ROOT, name, at)).unwrap();
    assert!(!record("a.txt", 100));
    assert!(record("A.txt", 200), "the same name to OneDrive");
    assert_eq!(opening_at(&w, "a.txt"), Some(100));
    assert!(!record("b.txt", 300));
    assert_eq!((opening_at(&w, "a.txt"), opening_at(&w, "b.txt")), (None, Some(300)));
}

/// A timeout or a lost connection (a `5xx` here) leaves the opening's
/// outcome unknown: the record is kept, and the retry carries it. Any other
/// answer is certain, and the record goes.
#[test]
fn only_an_unknown_outcome_keeps_the_opening_record() {
    for (status, kept) in [(500, true), (400, false), (403, false)] {
        let w = World::new(&[]);
        w.write("a.txt", b"hello");
        w.examine(&[("", "a.txt")]);
        w.cloud(|c| c.script("POST", "createUploadSession", ResponseTemplate::new(status), 1));
        w.run();
        assert_eq!(w.rows().len(), 1, "{status}: {:?}", w.summary());
        let at = opening_at(&w, "a.txt");
        assert_eq!(at.is_some(), kept, "{status}");
        if kept {
            let seq = w.rows()[0].seq;
            let carried = w.store.call_blocking(move |s| s.outbox_record_opening(seq, fake::ROOT, "a.txt", 0)).unwrap();
            assert!(carried, "{status}: the retry carries it");
            assert_eq!(opening_at(&w, "a.txt"), at);
        }
    }
}

/// A carried record (a stop lost the session's URL) and a holder with
/// content: someone else's file — a copy, nothing deleted, and the record
/// resolved.
#[test]
fn a_carried_record_and_a_holder_with_content_is_a_copy() {
    let w = World::new(&[]);
    opened_and_lost(&w, b"mine");
    w.cloud(|c| {
        c.expire_sessions();
        c.add_file("X", fake::ROOT, "a.txt", b"theirs");
        c.created.insert("X".into(), crate::sync::activity::unix_now());
    });
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.id_at("a.txt").as_deref(), Some("X"));
    assert_eq!(w.content("a-fedora.txt").unwrap(), b"mine");
    assert_eq!(w.cloud(|c| c.count("DELETE", "items/")), 0);
    assert_eq!(conflicts(&w), 1);
    assert_eq!(opening_at(&w, "a.txt"), None);
}

/// A carried record and an empty holder made before it (less the clock
/// slack): not ours, never deleted — the row waits, and the record is
/// resolved.
#[test]
fn a_carried_record_and_an_older_empty_holder_waits() {
    let w = World::new(&[]);
    opened_and_lost(&w, b"mine");
    w.cloud(|c| {
        c.expire_sessions();
        c.open_elsewhere(fake::ROOT, "a.txt", 3600);
    });
    w.run();
    assert_eq!(w.rows().len(), 1, "{:?}", w.summary());
    assert_eq!(w.rows()[0].reason.as_deref(), Some(reason::NAME_HELD));
    assert_eq!(w.cloud(|c| (c.placeholders(), c.open_sessions())), (vec!["a.txt".to_owned()], 1));
    assert_eq!(w.cloud(|c| c.count("DELETE", "items/")), 0);
    assert_eq!(opening_at(&w, "a.txt"), None);
}

/// Once a carried record is resolved — our placeholder deleted — it goes: a
/// placeholder another device opens at the name later is never compared
/// with its old time, never deleted. The row waits for it.
#[test]
fn a_resolved_record_never_deletes_a_later_placeholder() {
    let w = World::new(&[]);
    opened_and_lost(&w, b"mine");
    // Our placeholder deleted, then the next opening answered `429`.
    w.cloud(|c| c.throttle_429("POST", "createUploadSession", 1, 0, 1));
    w.run();
    assert_eq!(w.cloud(|c| (c.placeholders().len(), c.count("DELETE", "items/"))), (0, 1), "ours deleted");
    assert_eq!(w.rows().len(), 1, "{:?}", w.summary());
    assert_eq!(opening_at(&w, "a.txt"), None, "resolved, and a certain answer to the next opening");

    w.cloud(|c| c.open_elsewhere(fake::ROOT, "a.txt", 0));
    due(&w);
    w.run();
    assert_eq!(w.rows().len(), 1, "{:?}", w.summary());
    assert_eq!(w.rows()[0].reason.as_deref(), Some(reason::NAME_HELD));
    assert_eq!(w.cloud(|c| (c.placeholders(), c.open_sessions(), c.count("DELETE", "items/"))), (vec!["a.txt".to_owned()], 1, 1));
    assert_eq!(conflicts(&w), 0);
}
