//! What makes a row, or the whole worker, wait: throttling, the pause, the write gate, a
//! row that needs the user, and a failure in backoff (§6.2).

use super::*;

/// §6.2: a throttle stops the whole worker for as long as OneDrive asked —
/// `Retry-After` in seconds or as an HTTP date — and the row keeps its place.
#[test]
fn throttling_pauses_the_whole_worker() {
    for header in ["120", "date"] {
        let w = World::new(&[]);
        w.make_file("a.txt", b"a");
        let now = crate::clock::unix_now();
        let value = if header == "date" { http_date(now + 300) } else { header.to_owned() };
        w.cloud(|c| c.script("POST", "createUploadSession", ResponseTemplate::new(429).insert_header("Retry-After", value.as_str()), 1));
        let engine = w.run();
        let until = engine.status().throttled_until.expect("throttled");
        let wanted = if header == "date" { now + 300 } else { now + 120 };
        assert!((until - wanted).abs() <= 3, "{header}: {until} vs {wanted}");
        assert_eq!(w.summary(), vec![(Create, "a.txt".into(), OutboxState::Ready)]);
        assert_eq!(w.rows()[0].attempts, 0, "a throttle is no failure of the row");
        w.make_file("b.txt", b"b");
        let asked = w.cloud(|c| c.log.len());
        w.h.drain(&engine);
        assert_eq!(w.cloud(|c| c.log.len()), asked, "nothing is sent while throttled");
    }
}

fn http_date(at: i64) -> String {
    let text = konedrive_graph::drive::item::format_graph_time(at); // 2026-09-25T10:00:00Z
    let (date, time) = text.trim_end_matches('Z').split_once('T').unwrap();
    let mut parts = date.split('-');
    let (year, month, day) = (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap());
    let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("Thu, {day} {} {year} {time} GMT", months[month.parse::<usize>().unwrap() - 1])
}

/// A pause (the account's, whichever worker asks) stops the worker without touching the rows; a `403`,
/// a refused name and a writer each block or hold their own row, and a full
/// OneDrive holds the content; the file's `user.konedrive.sync`
/// says which.
#[test]
fn pause_and_blocked_rows() {
    let w = World::new(&[]);
    w.make_file("a.txt", b"a");
    let engine = w.h.engine();
    pause(&w);
    w.h.drain(&engine);
    assert_eq!(w.cloud(|c| c.log.len()), 0);
    assert_eq!(w.attr("a.txt", XATTR_SYNC).as_deref(), Some("pending"));
    let restarted = w.h.engine();
    w.h.drain(&restarted);
    assert_eq!(w.cloud(|c| c.log.len()), 0, "the account's pause holds another worker too");
    resume(&w);
    w.cloud(|c| c.script("POST", "createUploadSession", ResponseTemplate::new(403), 1));
    w.h.drain(&restarted);
    assert_eq!(w.summary(), vec![(Create, "a.txt".into(), OutboxState::Blocked)]);
    assert_eq!(w.attr("a.txt", XATTR_SYNC).as_deref(), Some("blocked"));
    // The worker a sign-in starts lets the row go.
    let restarted = w.h.engine();
    w.h.drain(&restarted);
    assert!(w.rows().is_empty());
    assert_eq!(w.attr("a.txt", XATTR_SYNC), None);

    // Refused by the service, OneDrive full, open for writing.
    // The refused one goes first and alone: once OneDrive is taken for full, no other row is
    // tried, so taken after `full.txt` it would wait for space instead of being refused.
    w.make_file("refused.txt", b"r");
    w.cloud(|c| {
        c.script("POST", "refused.txt", ResponseTemplate::new(400).set_body_json(serde_json::json!({"error": {"code": "invalidRequest", "message": "bad name"}})), 1);
    });
    w.h.drain(&restarted);
    w.make_file("full.txt", b"f");
    w.make_file("open.txt", b"o");
    let writer = File::options().append(true).open(w.path("open.txt")).unwrap();
    w.cloud(|c| c.script("POST", "full.txt", ResponseTemplate::new(507), 1));
    w.h.drain(&restarted);
    let state = |rel: &str| w.rows().into_iter().find(|r| r.rel == Path::new(rel)).map(|r| (r.state, r.reason_text().unwrap_or_default()));
    assert_eq!(state("refused.txt"), Some((OutboxState::Blocked, "refused: bad name".into())));
    // No quota to read: taken for full, and waiting for space in its place.
    assert_eq!(state("full.txt"), Some((OutboxState::Ready, Reason::WaitingForSpace.key().into())));
    assert_eq!(state("open.txt").map(|s| s.0), Some(OutboxState::Waiting));
    assert_eq!(w.attr("refused.txt", XATTR_SYNC).as_deref(), Some("blocked"));
    assert_eq!(w.attr("open.txt", XATTR_SYNC).as_deref(), Some("pending"));
    let counts = restarted.status().counts;
    assert_eq!((counts.pending, counts.blocked, counts.pending_bytes), (2, 1, 2));
    assert_eq!((counts.space_waiting, restarted.status().quota_full), (2, true));
    assert_eq!(w.h.host.kinds().iter().filter(|k| **k == ActivityKind::UploadFailed).count(), 2, "forbidden, refused: once each; full: none per file");
    drop(writer);
    w.cloud(|c| c.free = Some(10 << 20));
    refresh(&w, &restarted);
    w.h.block_on(restarted.retry_now()).unwrap();
    w.h.drain(&restarted);
    assert_eq!(w.summary(), vec![(Create, "refused.txt".into(), OutboxState::Blocked)]);
    assert_committed(&w, "full.txt", "full.txt");
    assert_committed(&w, "open.txt", "open.txt");
}

/// An answer no step settles is stored on its row as a stable key, never as
/// the error's own text, and the row goes again after its backoff.
#[test]
fn an_answer_nothing_settles_waits_as_upload_error() {
    let w = World::new(&[]);
    w.make_file("a.txt", b"a");
    w.cloud(|c| c.script("POST", "createUploadSession", ResponseTemplate::new(418), 1));
    let engine = w.h.engine();
    w.h.drain(&engine);
    assert_eq!(w.summary(), vec![(Create, "a.txt".into(), OutboxState::Retry)]);
    assert_eq!(reason_of(&w, "a.txt").as_deref(), Some(Reason::Failed.key()));
    w.h.block_on(engine.retry_now()).unwrap();
    w.h.drain(&engine);
    assert_committed(&w, "a.txt", "a.txt");
}

/// An answer that says the request went through and names no item that can be kept (here: the
/// new folder, as deleted) is OneDrive's failure, not the disk's: the row waits as
/// `upload-error`, and goes again.
#[test]
fn an_answer_that_names_no_usable_item_waits_as_upload_error() {
    let w = World::new(&[]);
    w.make_directory("d");
    let answer = serde_json::json!({ "id": "X", "name": "d", "folder": {}, "deleted": {} });
    w.cloud(|c| c.script("POST", "children", ResponseTemplate::new(201).set_body_json(answer), 1));
    let engine = w.h.engine();
    w.h.drain(&engine);
    assert_eq!(w.summary(), vec![(Mkdir, "d".into(), OutboxState::Retry)]);
    assert_eq!(reason_of(&w, "d").as_deref(), Some(Reason::Failed.key()));
    w.h.block_on(engine.retry_now()).unwrap();
    w.h.drain(&engine);
    assert_committed(&w, "d", "d");
}

/// An upload whose step meets a network failure waits with
/// `network`, and goes up once OneDrive answers again.
#[test]
fn a_network_failure_waits_as_network() {
    let w = World::new(&[]);
    w.make_file("a.txt", b"a");
    w.cloud(|c| c.script("POST", "createUploadSession", ResponseTemplate::new(502), 1));
    let engine = w.h.engine();
    w.h.drain(&engine);
    assert_eq!(w.summary(), vec![(Create, "a.txt".into(), OutboxState::Retry)]);
    assert_eq!(reason_of(&w, "a.txt").as_deref(), Some(Reason::Network.key()));
    w.h.block_on(engine.retry_now()).unwrap();
    w.h.drain(&engine);
    assert_committed(&w, "a.txt", "a.txt");
}
