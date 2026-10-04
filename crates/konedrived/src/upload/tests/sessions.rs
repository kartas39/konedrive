//! Upload sessions and the empty placeholder an open one holds its name with
//! in OneDrive (issue #47): every session persisted before its first byte, a
//! refused fragment sent again to the same session, a session given up
//! always cancelled — so our own placeholder never becomes a conflict.

use konedrive_tree::ActivityKind;
use super::*;

/// Larger than one request: four fragments of 320 KiB.
fn large() -> Vec<u8> {
    (0..(1024 * 1024 + 77)).map(|i| (i % 251) as u8).collect()
}

fn conflicts(w: &World) -> usize {
    w.h.host.kinds().iter().filter(|k| **k == ActivityKind::Conflict).count()
}

fn given_up(w: &World) -> Vec<String> {
    w.store.call_blocking(|s| s.upload_sessions_given_up(10)).unwrap().iter().map(|url| url.as_str().to_owned()).collect()
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
    row.session_url.expect("the session is kept for the next run").as_str().to_owned()
}

/// How many requests of `method` whose path holds `fragment` came after the
/// first `from`.
fn since(w: &World, from: usize, method: &str, fragment: &str) -> usize {
    w.cloud(|c| c.log[from..].iter().filter(|(m, p)| m == method && p.contains(fragment)).count())
}

/// Does `then` once, the first time the write gate is asked while `when`
/// holds. A row asks the gate before each fragment it sends, so `when` says
/// between which two fragments `then` happens.
fn between(w: &World, when: impl Fn(&fake::Cloud) -> bool + Send + 'static, then: impl FnOnce(&mut fake::Cloud) + Send + 'static) {
    let cloud = Arc::clone(&w.h.graph.cloud);
    let mut then = Some(then);
    w.h.host.asked.lock().unwrap().push(Box::new(move || {
        let mut cloud = cloud.lock().unwrap();
        if then.is_some() && when(&cloud) {
            then.take().expect("checked")(&mut cloud);
        }
    }));
}

/// `sent` fragments have reached OneDrive.
fn fragments(sent: usize) -> impl Fn(&fake::Cloud) -> bool + Send + 'static {
    move |c| c.count("PUT", "upload/") == sent
}

/// `a.bin` of four fragments queued: a new file, or new content for the
/// item `A`.
fn four_fragments(update: bool) -> World {
    let w = if update { World::new(&[file("A", "R", "a.bin", b"old")]) } else { World::new(&[]) };
    if update {
        w.hydrate("a.bin", b"old");
        w.edit("a.bin", &large());
    } else {
        w.write("a.bin", &large());
    }
    w.examine(&[("", "a.bin")]);
    w
}

/// A crash at each point of a session (§10), for a new file and for a
/// changed one, and what the next start does:
///
/// - the session opened, not persisted: a new session — a new file's replay
///   first meets the lost session's placeholder (`409`) and deletes it;
/// - after two fragments: the session is asked where it stands, and only the
///   other two are sent;
/// - the last fragment sent, its answer lost: the session answers `404`, the
///   item holds this content and is adopted — nothing is sent.
///
/// Each ends with the one item, committed: no copy, no placeholder, nothing
/// deleted.
#[test]
fn a_crash_at_each_point_of_a_session_is_replayed_to_the_same_item() {
    for update in [false, true] {
        for (fault, posts, puts) in [(Fault::SessionNotPersisted, if update { 1 } else { 2 }, 4), (Fault::MidSession(2), 0, 2), (Fault::AfterSend, 0, 0)] {
            let at = format!("update {update}, {fault:?}");
            let w = four_fragments(update);
            let engine = w.h.engine();
            engine.arm(fault);
            w.h.drain(&engine);
            let row = w.rows().remove(0);
            assert_eq!(row.state, OutboxState::Running, "{at}: stopped as by a crash");
            match fault {
                Fault::SessionNotPersisted => assert_eq!(row.session_url, None, "{at}"),
                Fault::MidSession(_) => assert_eq!(row.session_next, Some(2 * 320 * 1024), "{at}"),
                _ => assert!(row.session_url.is_some(), "{at}"),
            }

            let from = w.cloud(|c| c.log.len());
            w.run();
            assert!(w.rows().is_empty(), "{at}: {:?}", w.summary());
            assert_eq!((since(&w, from, "POST", "createUploadSession"), since(&w, from, "PUT", "upload/")), (posts, puts), "{at}");
            assert_eq!(w.cloud(|c| c.paths()), vec!["a.bin"], "{at}");
            assert_eq!(w.content("a.bin").unwrap(), large(), "{at}");
            assert_committed(&w, "a.bin", "a.bin");
            if update {
                assert_eq!(w.id_at("a.bin").as_deref(), Some("A"), "{at}: the item keeps its id");
            }
            assert_eq!(conflicts(&w), 0, "{at}");
            assert!(w.cloud(|c| c.placeholders().is_empty() && c.bin.is_empty()), "{at}: no placeholder left, nothing deleted");
        }
    }
}

/// The bug as it happened (issue #47): a fragment answered `429`. Refused
/// once, it goes again to the same session at once — a file's only fragment
/// and a middle one alike; refused as often as the client sends it, the row
/// fails for now, keeps its session, and its next run resumes it. Either way
/// the file lands under its own name: one session, no conflict, no
/// placeholder left.
#[test]
fn a_fragment_refused_for_now_goes_again_to_the_same_session() {
    for (content, before, refusals, puts) in [(b"hello".to_vec(), 0, 1, 2), (b"hello".to_vec(), 0, 2, 3), (large(), 1, 1, 5)] {
        let at = format!("{} bytes, {refusals} refusals", content.len());
        let w = World::new(&[]);
        w.write("a.bin", &content);
        w.examine(&[("", "a.bin")]);
        w.cloud(|c| c.throttle_429("PUT", "upload/", before, 0, refusals));
        w.run();
        if refusals == 2 {
            assert!(w.rows()[0].session_url.is_some(), "{at}: kept for the next run");
            assert_eq!(w.cloud(|c| c.placeholders()), vec!["a.bin"], "{at}");
            w.run();
        }
        assert!(w.rows().is_empty(), "{at}: {:?}", w.summary());
        assert_eq!(w.cloud(|c| c.paths()), vec!["a.bin"], "{at}");
        assert_eq!(w.content("a.bin").unwrap(), content, "{at}");
        assert_eq!(w.cloud(|c| (c.count("POST", "createUploadSession"), c.count("PUT", "upload/"))), (1, puts), "{at}: one session");
        assert_eq!(w.cloud(|c| (c.placeholders().len(), c.open_sessions())), (0, 0), "{at}");
        assert_eq!(conflicts(&w), 0, "{at}");
        assert!(given_up(&w).is_empty(), "{at}");
        assert_committed(&w, "a.bin", "a.bin");
    }
}

/// A file saved again while it goes up is never committed as what was being
/// sent — whether the save comes before its only fragment, while a middle
/// one goes, or before the last. The session is given up and cancelled (the
/// name is free again), the row waits (`changed`), and its next run sends
/// the new content from zero.
#[test]
fn a_file_changed_while_it_goes_up_gives_its_session_up_and_goes_again() {
    for (content, sent) in [(b"hello".to_vec(), 0), (large(), 1), (large(), 3)] {
        let at = format!("{} bytes, after {sent} fragments", content.len());
        let w = World::new(&[]);
        w.write("a.bin", &content);
        w.examine(&[("", "a.bin")]);
        let path = w.path("a.bin");
        between(&w, move |c| c.count("POST", "createUploadSession") == 1 && c.count("PUT", "upload/") == sent, move |_| std::fs::write(&path, b"saved again").unwrap());
        w.run();
        let row = w.rows().remove(0);
        assert_eq!((row.state, row.reason_text().as_deref()), (OutboxState::Waiting, Some(Reason::Changed.key())), "{at}");
        assert_eq!(row.session_url, None, "{at}");
        assert_eq!(w.cloud(|c| (c.count("PUT", "upload/"), c.count("DELETE", "upload/"))), (sent, 1), "{at}: nothing more sent, the session cancelled");
        assert_eq!(w.cloud(|c| (c.paths().len(), c.placeholders().len(), c.open_sessions())), (0, 0, 0), "{at}");
        assert!(given_up(&w).is_empty(), "{at}");

        due(&w);
        w.run();
        assert!(w.rows().is_empty(), "{at}: {:?}", w.summary());
        assert_eq!(w.content("a.bin").unwrap(), b"saved again", "{at}");
        assert_eq!(w.cloud(|c| c.count("POST", "createUploadSession")), 2, "{at}: a new session");
        assert_eq!(conflicts(&w), 0, "{at}");
        assert_committed(&w, "a.bin", "a.bin");
    }
}

/// A session that ends under its upload (`404` to a fragment) with nothing
/// of this content in OneDrive: the upload starts over with a new session,
/// once. When that one ends too, the row backs off (`the upload session
/// ended twice`) and no third is opened in that run; its next run goes
/// through. (A session that expired while its row waited:
/// `a_pause_stops_a_session_after_its_fragment_and_resume_goes_on`.)
#[test]
fn a_session_that_ends_under_its_upload_starts_over_once_in_a_run() {
    let w = four_fragments(false);
    between(&w, fragments(1), |c| c.expire_sessions());
    between(&w, fragments(3), |c| c.expire_sessions());
    w.run();
    let row = w.rows().remove(0);
    assert_eq!((row.state, row.reason_text().as_deref()), (OutboxState::Retry, Some(Reason::SessionEnded.key())), "{:?}", w.summary());
    assert_eq!(row.session_url, None);
    assert_eq!(w.cloud(|c| (c.count("POST", "createUploadSession"), c.count("PUT", "upload/"))), (2, 4), "one fragment taken and one refused, twice");
    assert!(given_up(&w).is_empty(), "an ended session is not one to cancel");

    due(&w);
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.content("a.bin").unwrap(), large());
    assert_eq!(conflicts(&w), 0);
    assert_committed(&w, "a.bin", "a.bin");
}

/// What OneDrive refuses for good in the middle of a session — after its
/// opening was accepted — gives the session up, cancelled, and is decided as
/// the same refusal at the opening is (§6):
///
/// - a new file whose name another file took meanwhile (`409` to the last
///   fragment): a conflict copy, theirs untouched;
/// - a changed file whose item was changed in OneDrive meanwhile (read again
///   before the last fragment, §4.8 step 4): the last fragment is never
///   sent, and both versions are kept;
/// - a changed file whose item was deleted in OneDrive meanwhile: it goes up
///   again as new;
/// - a `412` to a fragment with the item as it was: again from zero, against
///   the item read again.
#[test]
fn a_refusal_in_the_middle_of_a_session_gives_it_up_and_is_decided_as_at_the_opening() {
    let ended = |w: &World, at: &str| {
        assert!(w.rows().is_empty(), "{at}: {:?}", w.summary());
        assert_eq!(w.cloud(|c| (c.placeholders().len(), c.open_sessions())), (0, 0), "{at}");
        assert!(given_up(w).is_empty(), "{at}");
    };

    let w = four_fragments(false);
    between(&w, fragments(1), |c| c.add_file("X", fake::ROOT, "a.bin", b"theirs"));
    w.run();
    ended(&w, "409");
    assert_eq!(w.cloud(|c| c.paths()), vec!["a-fedora.bin", "a.bin"]);
    assert_eq!((w.id_at("a.bin").as_deref(), w.content("a.bin").unwrap()), (Some("X"), b"theirs".to_vec()));
    assert_eq!(w.content("a-fedora.bin").unwrap(), large());
    assert_eq!(conflicts(&w), 1);

    let w = four_fragments(true);
    between(&w, fragments(1), |c| c.edit("A", b"theirs"));
    w.run();
    ended(&w, "changed there");
    assert_eq!(w.cloud(|c| c.paths()), vec!["a-fedora.bin", "a.bin"]);
    assert_eq!((w.id_at("a.bin").as_deref(), w.content("a.bin").unwrap()), (Some("A"), b"theirs".to_vec()), "their version is not overwritten");
    assert_eq!(w.content("a-fedora.bin").unwrap(), large());
    assert_eq!(w.cloud(|c| (c.count("PUT", "upload/"), c.count("DELETE", "upload/"))), (3 + 4, 1), "the last fragment never sent; then the copy");
    assert_eq!(conflicts(&w), 1);

    let w = four_fragments(true);
    between(&w, fragments(1), |c| c.trash("A"));
    w.run();
    ended(&w, "deleted there");
    assert_eq!(w.cloud(|c| c.paths()), vec!["a.bin"]);
    assert_ne!(w.id_at("a.bin").as_deref(), Some("A"), "a new item");
    assert_eq!(w.content("a.bin").unwrap(), large());
    assert!(w.h.host.kinds().contains(&ActivityKind::Restored));
    assert_eq!(conflicts(&w), 0);

    let w = four_fragments(true);
    w.cloud(|c| c.script("PUT", "upload/", ResponseTemplate::new(412), 1));
    w.run();
    ended(&w, "412");
    assert_eq!((w.id_at("a.bin").as_deref(), w.content("a.bin").unwrap()), (Some("A"), large()));
    assert_eq!(w.cloud(|c| (c.count("POST", "createUploadSession"), c.count("DELETE", "upload/"))), (2, 1));
    assert_eq!(conflicts(&w), 0);
}

/// A changed file's session that waited — its fragment refused for now — is
/// completed only against the version it was opened for: the item is read
/// again before the last fragment also when that is the session's only one
/// (§4.8 step 4), since the guard was checked when the session was opened,
/// not when it completes. Changed in OneDrive meanwhile, both versions are
/// kept, as for any `412`; theirs is never overwritten.
#[test]
fn a_session_resumed_after_the_item_changed_in_onedrive_never_overwrites_it() {
    let w = World::new(&[file("A", "R", "a.txt", b"old")]);
    w.hydrate("a.txt", b"old");
    w.edit("a.txt", b"mine");
    w.examine(&[("", "a.txt")]);
    w.cloud(|c| c.throttle_429("PUT", "upload/", 0, 0, 2));
    w.run();
    assert!(w.rows()[0].session_url.is_some(), "kept for the next run: {:?}", w.summary());

    w.cloud(|c| c.edit("A", b"theirs"));
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["a-fedora.txt", "a.txt"]);
    assert_eq!((w.id_at("a.txt").as_deref(), w.content("a.txt").unwrap()), (Some("A"), b"theirs".to_vec()));
    assert_eq!(w.content("a-fedora.txt").unwrap(), b"mine");
    assert_eq!(w.cloud(|c| (c.count("PUT", "upload/"), c.open_sessions())), (2 + 1, 0), "the fragment refused twice, then only the copy's");
    assert_eq!(conflicts(&w), 1);
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
    assert_eq!(w.rows()[0].reason_text().as_deref(), Some(Reason::SessionOpen.key()));
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
            c.created.insert("X".into(), crate::status::activity::unix_now() - age);
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

/// The recorded place outlasts a restart. It goes once the row resolves it
/// (its placeholder deleted, the file committed); a row that leaves before
/// — its file removed — leaves it behind, without a row, since our
/// placeholder may still hold the name (issue #89).
#[test]
fn a_recorded_opening_outlasts_a_restart_and_a_row_that_left() {
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
        assert_eq!(opening_at(&w, "a.txt").is_some(), removed, "removed {removed}");
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
            assert_eq!(w.rows()[0].reason_text().as_deref(), Some(Reason::NameHeld.key()), "{at}");
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
    assert_eq!(w.rows()[0].reason_text().as_deref(), Some(Reason::NameHeld.key()));
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
    assert_eq!(rows[0].reason_text().as_deref(), Some(Reason::NameHeld.key()));
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
        assert_eq!(mine.reason_text().as_deref(), Some(Reason::NameHeld.key()), "{what}");
        let other = rows.iter().find(|r| r.rel.to_str() == Some("a.txt")).unwrap();
        assert_eq!(other.session_url.as_ref().map(|url| url.as_str()), Some(session.as_str()), "{what}: its session untouched");
        assert_eq!(w.cloud(|c| (c.placeholders(), c.open_sessions())), (vec!["a.txt".to_owned()], 1), "{what}");
        assert_eq!(w.cloud(|c| (c.count("DELETE", "upload/"), c.count("DELETE", "items/"))), (0, 0), "{what}");
        assert_eq!(conflicts(&w), 0, "{what}");
    }
}

/// The record of an opening: made (`None`) or carried (`Some`) — the same
/// row at the same place, the name compared without case, keeping its first
/// time; at another place it starts again, and the old one is kept without
/// a row.
#[test]
fn an_opening_record_is_carried_at_the_same_place_without_case() {
    let w = World::new(&[]);
    let record = |name: &'static str, at: i64| w.store.call_blocking(move |s| s.outbox_record_opening(7, fake::ROOT, name, at)).unwrap().is_some();
    assert!(!record("a.txt", 100));
    assert!(record("A.txt", 200), "the same name to OneDrive");
    assert_eq!(opening_at(&w, "a.txt"), Some(100));
    assert!(!record("b.txt", 300));
    assert_eq!((opening_at(&w, "a.txt"), opening_at(&w, "b.txt")), (Some(100), Some(300)));
    // Kept without a row for a week, then gone (left at 300 here).
    let keep = konedrive_tree::outbox::OPENING_LEFT_KEEP;
    w.store.call_blocking(move |s| s.upload_openings_expire(300 + keep)).unwrap();
    assert_eq!(opening_at(&w, "a.txt"), Some(100));
    w.store.call_blocking(move |s| s.upload_openings_expire(301 + keep)).unwrap();
    assert_eq!(opening_at(&w, "a.txt"), None);
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
            assert!(carried.is_some(), "{status}: the retry carries it");
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
        c.created.insert("X".into(), crate::status::activity::unix_now());
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
    assert_eq!(w.rows()[0].reason_text().as_deref(), Some(Reason::NameHeld.key()));
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
    assert_eq!(w.rows()[0].reason_text().as_deref(), Some(Reason::NameHeld.key()));
    assert_eq!(w.cloud(|c| (c.placeholders(), c.open_sessions(), c.count("DELETE", "items/"))), (vec!["a.txt".to_owned()], 1, 1));
    assert_eq!(conflicts(&w), 0);
}

/// A timeout after OneDrive opened the session (issue #89): the record is
/// kept. The row then leaves (the file replaced by another inode), and its
/// record stays without a row: the new row at the name finds our placeholder
/// through it, deletes it, and goes up under its name — no copy, no endless
/// wait.
#[test]
fn a_record_whose_row_left_still_finds_our_placeholder() {
    let w = World::new(&[]);
    w.write("a.txt", b"first");
    w.examine(&[("", "a.txt")]);
    w.cloud(|c| c.lose_answers("POST", "createUploadSession", 1));
    w.run();
    assert_eq!(w.cloud(|c| c.placeholders()), vec!["a.txt"], "OneDrive opened it");
    let first = w.rows()[0].seq;
    assert!(opening_at(&w, "a.txt").is_some());

    std::fs::remove_file(w.path("a.txt")).unwrap();
    w.write("b.tmp", b"second");
    w.rename("b.tmp", "a.txt");
    w.examine(&[("", "a.txt"), ("", "b.tmp")]);
    assert!(w.rows().iter().all(|r| r.seq != first), "the row left: {:?}", w.summary());
    assert!(opening_at(&w, "a.txt").is_some(), "kept without its row");

    due_all(&w);
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["a.txt"]);
    assert_eq!(w.content("a.txt").unwrap(), b"second");
    assert_eq!(conflicts(&w), 0);
    assert_eq!(opening_at(&w, "a.txt"), None, "resolved");
}

/// Every row due now.
fn due_all(w: &World) {
    for row in w.rows() {
        let seq = row.seq;
        w.store.call_blocking(move |s| s.outbox_set_state(seq, OutboxState::Ready, None, None)).unwrap();
    }
}

/// A carried record takes for ours only an empty holder made up to the
/// latest attempt whose outcome was not known (plus the clock slack): one
/// made later is another device's — never deleted, the row waits.
#[test]
fn a_carried_record_never_takes_a_later_placeholder_for_ours() {
    let w = World::new(&[]);
    opened_and_lost(&w, b"mine");
    w.cloud(|c| {
        c.expire_sessions();
        c.open_elsewhere(fake::ROOT, "a.txt", -(crate::upload::content::CLOCK_SLACK + 60));
    });
    w.run();
    assert_eq!(w.rows().len(), 1, "{:?}", w.summary());
    assert_eq!(w.rows()[0].reason_text().as_deref(), Some(Reason::NameHeld.key()));
    assert_eq!(w.cloud(|c| (c.placeholders().len(), c.count("DELETE", "items/"))), (1, 0));
    assert_eq!(opening_at(&w, "a.txt"), None, "resolved");
}

/// A carried record and an empty holder the delta feed listed (renamed to
/// the name in OneDrive since): never ours, never deleted — a copy, as for
/// any listed file.
#[test]
fn a_carried_record_never_takes_a_listed_file_for_ours() {
    let w = World::new(&[file("X", "R", "x.txt", b"")]);
    opened_and_lost(&w, b"mine");
    w.cloud(|c| {
        c.expire_sessions();
        c.rename("X", fake::ROOT, "a.txt");
        c.created.insert("X".into(), crate::status::activity::unix_now());
    });
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.id_at("a.txt").as_deref(), Some("X"));
    assert_eq!(w.content("a-fedora.txt").unwrap(), b"mine");
    assert_eq!(w.cloud(|c| c.count("DELETE", "items/")), 0);
    assert_eq!(conflicts(&w), 1);
    assert_eq!(opening_at(&w, "a.txt"), None);
}

/// Two records at one name — one left by its row long ago, one carried now
/// — are two windows, never one: a placeholder another device made between
/// them is not ours, never deleted; the row waits (issue #89).
#[test]
fn a_placeholder_between_two_records_windows_is_not_ours() {
    let w = World::new(&[]);
    let long_ago = crate::status::activity::unix_now() - 7200;
    // Row 900's record at `a.txt`, left behind when it moves elsewhere.
    w.store
        .call_blocking(move |s| {
            s.outbox_record_opening(900, fake::ROOT, "a.txt", long_ago)?;
            s.outbox_record_opening(900, fake::ROOT, "elsewhere.txt", long_ago + 1)
        })
        .unwrap();
    opened_and_lost(&w, b"mine");
    w.cloud(|c| {
        c.expire_sessions();
        c.open_elsewhere(fake::ROOT, "a.txt", 3600);
    });
    w.run();
    assert_eq!(w.rows().len(), 1, "{:?}", w.summary());
    assert_eq!(w.rows()[0].reason_text().as_deref(), Some(Reason::NameHeld.key()));
    assert_eq!(w.cloud(|c| (c.placeholders().len(), c.count("DELETE", "items/"))), (1, 0));
    assert_eq!(conflicts(&w), 0);
}

/// A certain answer to a carried record's retry puts its latest unknown
/// outcome back: a placeholder made after that (plus the slack), though
/// before the retry, is not ours — the row waits, nothing deleted.
#[test]
fn a_certain_answer_keeps_a_carried_records_last_unknown_time() {
    let w = World::new(&[]);
    w.write("a.txt", b"mine");
    w.examine(&[("", "a.txt")]);
    let seq = w.rows()[0].seq;
    let then = crate::status::activity::unix_now() - 3600;
    w.store.call_blocking(move |s| s.outbox_record_opening(seq, fake::ROOT, "a.txt", then)).unwrap();
    w.cloud(|c| c.open_elsewhere(fake::ROOT, "a.txt", 1800));
    w.run();
    assert_eq!(w.rows().len(), 1, "{:?}", w.summary());
    assert_eq!(w.rows()[0].reason_text().as_deref(), Some(Reason::NameHeld.key()));
    assert_eq!(w.cloud(|c| (c.placeholders().len(), c.count("DELETE", "items/"))), (1, 0));
    assert_eq!(conflicts(&w), 0);
}

/// A host whose write gate, once a fragment of an upload has reached OneDrive, does not
/// answer until it is let go: the row that asks between two fragments waits there.
struct HeldGate {
    cloud: Arc<Mutex<fake::Cloud>>,
    asked: std::sync::atomic::AtomicBool,
    let_go: Mutex<bool>,
    told: std::sync::Condvar,
    answered: std::sync::atomic::AtomicBool,
}

impl HeldGate {
    fn let_go(&self) {
        *self.let_go.lock().unwrap() = true;
        self.told.notify_all();
    }
}

impl crate::upload::OutboxHost for HeldGate {
    fn may_write(&self) -> Result<(), String> {
        use std::sync::atomic::Ordering;
        if self.cloud.lock().unwrap().count("PUT", "upload/") == 0 {
            return Ok(());
        }
        self.asked.store(true, Ordering::SeqCst);
        let mut let_go = self.let_go.lock().unwrap();
        while !*let_go {
            let_go = self.told.wait(let_go).unwrap();
        }
        self.answered.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Lets the gate go when the test ends, however it ends: the runtime waits for the gate.
struct LetGo(Arc<HeldGate>);

impl Drop for LetGo {
    fn drop(&mut self) {
        self.0.let_go();
    }
}

/// A stop of the worker returns only when the write gate a row was asking has answered:
/// what the gate writes (the folder's note, the account's mode) is never written after the
/// stop. The row is cut off between two fragments of its upload, while it waits for the gate.
#[test]
fn a_stop_waits_for_the_gate_a_row_was_asking_between_fragments() {
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    let w = World::new(&[]);
    w.write("big.bin", &large());
    w.examine(&[("", "big.bin")]);
    let host = Arc::new(HeldGate {
        cloud: Arc::clone(&w.h.graph.cloud),
        asked: false.into(),
        let_go: Mutex::new(false),
        told: std::sync::Condvar::new(),
        answered: false.into(),
    });
    let _let_go = LetGo(Arc::clone(&host));
    let mut config = w.h.config();
    config.host = host.clone();
    let worker = Arc::new(crate::upload::OutboxWorker::new(config));
    w.h.block_on(async {
        worker.start();
        let asked = tokio::time::timeout(Duration::from_secs(30), async {
            while !host.asked.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        asked.await.expect("the gate is asked after the first fragment");
        let mut stop = tokio::spawn({
            let worker = Arc::clone(&worker);
            async move { worker.stop().await }
        });
        let early = tokio::time::timeout(Duration::from_millis(300), &mut stop).await;
        assert!(early.is_err(), "the stop returned while the gate was still being asked");
        host.let_go();
        tokio::time::timeout(Duration::from_secs(30), stop).await.expect("the stop ends once the gate has answered").unwrap();
        assert!(host.answered.load(Ordering::SeqCst), "the gate answered before the stop returned");
    });
}
