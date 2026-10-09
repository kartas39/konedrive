//! An upload session interrupted: by a pause, by OneDrive turning full, by the daemon's
//! stop, by the file changing — what is kept of it, and what the next run does.

use super::*;

/// OneDrive turning full while an upload in
/// fragments is under way stops it after the fragment in flight, as a pause
/// does, but the row waits for space — ready, its session kept. Space again
/// resumes it from its offset.
#[test]
fn a_full_onedrive_stops_a_session_after_its_fragment_and_space_resumes_it() {
    let w = World::new(&[]);
    let content: Vec<u8> = (0..(1024 * 1024 + 77)).map(|i| (i % 251) as u8).collect();
    w.make_file("big.bin", &content);
    let engine = w.h.engine();
    let exceeded = konedrive_graph::drive::DriveQuota { total: 0, used: 0, remaining: Some(0), state: "exceeded".into() };
    drain_stopped_mid_request(&w, &engine, "PUT", "upload/", || w.h.block_on(engine.apply_quota(&exceeded)));
    assert_eq!(w.cloud(|c| c.count("PUT", "upload/")), 1, "no fragment after the one in flight");
    let row = w.rows().remove(0);
    assert_eq!((row.state, row.reason_text().as_deref()), (OutboxState::Ready, Some(Reason::WaitingForSpace.key())));
    assert_eq!(row.session_next, Some(320 * 1024), "the session is kept");

    w.cloud(|c| c.free = Some(10 << 20));
    refresh(&w, &engine);
    w.h.drain(&engine);
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| (c.count("POST", "createUploadSession"), c.count("PUT", "upload/"))), (1, 4), "resumed, not started over");
    assert_eq!(w.content("big.bin").unwrap(), content);
}

/// A pause stops an upload in fragments after the fragment in flight.
/// The row waits as `paused` — no failure, nothing uploading — with its
/// session and offset kept, through a restart too. Resumed, a session still
/// open goes on from that offset; an expired one starts over.
#[test]
fn a_pause_stops_a_session_after_its_fragment_and_resume_goes_on() {
    for expire in [false, true] {
        let w = World::new(&[]);
        let content: Vec<u8> = (0..(1024 * 1024 + 77)).map(|i| (i % 251) as u8).collect();
        w.make_file("big.bin", &content);
        let engine = w.h.engine();
        drain_paused_mid_request(&w, &engine, "PUT", "upload/", || {});
        assert_eq!(w.cloud(|c| c.count("PUT", "upload/")), 1, "no fragment after the one in flight");
        let row = w.rows().remove(0);
        assert_eq!((row.state, row.reason_text().as_deref()), (OutboxState::Waiting, Some(Reason::Paused.key())));
        assert!(row.session_url.is_some(), "the session is kept");
        assert_eq!(row.session_next, Some(320 * 1024));
        let status = engine.status();
        assert!(status.uploads.is_empty(), "nothing shows as uploading: {status:?}");
        assert_eq!(w.attr("big.bin", XATTR_SYNC).as_deref(), Some("pending"));

        // A restart while paused: nothing is sent, the session stays.
        let asked = w.cloud(|c| c.log.len());
        let restarted = w.h.engine();
        w.h.drain(&restarted);
        assert_eq!(w.cloud(|c| c.log.len()), asked, "nothing is sent while paused");
        let kept = w.rows().remove(0);
        assert_eq!((kept.session_url, kept.session_next), (row.session_url, row.session_next));

        if expire {
            w.cloud(|c| c.expire_sessions());
        }
        resume(&w);
        w.h.drain(&restarted);
        assert!(w.rows().is_empty(), "{:?}", w.summary());
        let sent = w.cloud(|c| (c.count("POST", "createUploadSession"), c.count("PUT", "upload/")));
        // 1 MiB + 77 is four fragments of 320 KiB.
        let wanted = if expire { (2, 1 + 4) } else { (1, 4) };
        assert_eq!(sent, wanted, "expired: {expire}");
        assert_eq!(w.content("big.bin").unwrap(), content);
        assert_committed(&w, "big.bin", "big.bin");
        assert!(!w.h.host.kinds().contains(&ActivityKind::UploadFailed), "a pause is no failure: {:?}", w.h.host.kinds());
    }
}

/// A file's only fragment in flight when the pause comes finishes, and
/// the file is committed; a row that comes meanwhile does not start.
#[test]
fn a_fragment_in_flight_at_a_pause_finishes_and_nothing_new_starts() {
    let w = World::new(&[]);
    w.make_file("a.txt", b"a");
    let engine = w.h.engine();
    drain_paused_mid_request(&w, &engine, "PUT", "upload/", || {
        w.make_file("b.txt", b"b");
        engine.wake();
    });
    assert_committed(&w, "a.txt", "a.txt");
    assert_eq!(w.summary(), vec![(Create, "b.txt".into(), OutboxState::Ready)]);
    assert_eq!(w.cloud(|c| c.count("POST", "b.txt")), 0, "nothing new starts while paused");
    assert!(engine.status().uploads.is_empty());
}

/// The daemon's stop while a new file's session is being opened:
/// the worker takes nothing more, waits for the answer and persists the
/// session before its task ends; the upload in fragments stops before its
/// first fragment, and the next start resumes the same session.
#[test]
fn a_stop_while_a_session_opens_waits_for_it_and_persists_it() {
    let w = World::new(&[]);
    let content: Vec<u8> = (0..(1024 * 1024 + 77)).map(|i| (i % 251) as u8).collect();
    w.make_file("big.bin", &content);
    w.cloud(|c| c.delay("POST", "createUploadSession", Duration::from_millis(400), 1));
    let worker = OutboxWorker::new(w.h.config());
    w.h.runtime.block_on(async {
        worker.start();
        let mut waited = 0;
        while w.cloud(|c| c.count("POST", "createUploadSession")) == 0 && waited < 500 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            waited += 1;
        }
        assert_eq!(w.cloud(|c| c.count("POST", "createUploadSession")), 1, "the opening is in flight");
        worker.close().await;
    });
    let row = w.rows().remove(0);
    assert_eq!(row.state, OutboxState::Ready, "{:?}", row.reason);
    assert!(row.session_url.is_some(), "the session is persisted");
    assert_eq!(w.cloud(|c| c.count("PUT", "upload/")), 0, "nothing sent after the stop");

    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| (c.count("POST", "createUploadSession"), c.count("PUT", "upload/"))), (1, 4), "resumed, not opened again");
    assert_eq!(w.content("big.bin").unwrap(), content);
    assert!(!w.h.host.kinds().contains(&ActivityKind::Conflict));
}

/// A large upload stopped mid-session, then the file saved again with
/// the same size, and the worker stopped while it cancels the old session.
/// That session is never resumed with the new bytes: what OneDrive ends
/// with is exactly the new content.
#[test]
fn a_session_is_never_resumed_with_other_content() {
    let w = World::new(&[]);
    let first: Vec<u8> = (0..(700 * 1024)).map(|i| (i % 251) as u8).collect();
    let second: Vec<u8> = (0..(700 * 1024)).map(|i| (i % 241) as u8).collect();
    w.make_file("big.bin", &first);
    let engine = w.h.engine();
    engine.arm(Fault::MidSession(1));
    w.h.drain(&engine);
    assert!(w.rows()[0].session_url.is_some());

    let file = File::options().write(true).open(w.path("big.bin")).unwrap();
    (&file).write_all(&second).unwrap();
    placeholder::set_mtime(&file, std::time::SystemTime::now() + Duration::from_secs(5)).unwrap();
    drop(file);
    // The cancel of the old session hangs, and the worker is stopped then.
    w.cloud(|c| c.script("DELETE", "upload/", ResponseTemplate::new(204).set_delay(Duration::from_secs(5)), 1));
    let worker = OutboxWorker::new(w.h.config());
    w.h.runtime.block_on(async {
        worker.start();
        let mut waited = 0;
        while w.cloud(|c| c.count("DELETE", "upload/")) == 0 && waited < 250 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            waited += 1;
        }
        worker.stop().await;
    });
    let row = &w.rows()[0];
    assert_eq!(row.session_url, None, "the session went with the content it was opened for");
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.content("big.bin").unwrap(), second);
    assert_committed(&w, "big.bin", "big.bin");
}

/// The folder turns read-only while an upload in fragments is under way: the write gate,
/// asked between two fragments, is closed. No fragment follows the one that was sent, and
/// the row waits with the gate's reason, its session and offset kept. Once the gate is open
/// the same session goes on from that offset.
#[test]
fn a_write_gate_that_closes_between_fragments_stops_the_session_until_it_opens() {
    let w = World::new(&[]);
    let content: Vec<u8> = (0..(1024 * 1024 + 77)).map(|i| (i % 251) as u8).collect();
    w.make_file("big.bin", &content);
    let (host, cloud) = (Arc::downgrade(&w.h.host), Arc::clone(&w.h.graph.cloud));
    w.h.host.asked.lock().unwrap().push(Box::new(move || {
        if cloud.lock().unwrap().count("PUT", "upload/") == 1 {
            if let Some(host) = host.upgrade() {
                *host.gate.lock().unwrap() = Some("the folder is read-only".into());
            }
        }
    }));
    let engine = w.run();
    assert_eq!(w.cloud(|c| c.count("PUT", "upload/")), 1, "no fragment after the gate closed");
    let row = w.rows().remove(0);
    assert_eq!((row.state, row.reason), (OutboxState::Waiting, Some(Reason::NotAllowed(Some("the folder is read-only".into())))));
    assert_eq!(row.session_next, Some(320 * 1024), "the session is kept");
    assert!(engine.status().uploads.is_empty());

    w.h.host.asked.lock().unwrap().clear();
    *w.h.host.gate.lock().unwrap() = None;
    w.h.drain(&engine);
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| (c.count("POST", "createUploadSession"), c.count("PUT", "upload/"))), (1, 4), "resumed, not started over");
    assert_eq!(w.content("big.bin").unwrap(), content);
    assert_committed(&w, "big.bin", "big.bin");
}
