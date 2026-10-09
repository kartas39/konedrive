//! A full OneDrive, and a file too big for the space left (`upload::space`).

use super::*;

/// No space left: the first refusal reads the quota, the account
/// turns full and sends no more content, while a rename and a delete still
/// go. A Refresh that finds space lets everything go.
#[test]
fn a_full_onedrive_sends_no_content_but_moves_and_deletes_go() {
    let w = World::new(&[file("A", "R", "old.txt", b"old"), file("B", "R", "gone.txt", b"gone")]);
    w.cloud(|c| c.free = Some(0));
    w.make_file("n1.txt", b"one");
    w.make_file("n2.txt", b"two");
    // One transfer at a time: the first refusal comes before the second starts.
    let engine = w.h.engine();
    engine.drive().pool().set_limits(1, 1);
    w.h.drain(&engine);
    assert!(engine.space_full());
    assert_eq!(w.cloud(|c| c.quota_reads()), 1, "one read for the refusal");
    assert_eq!(content_requests(&w, "n1.txt") + content_requests(&w, "n2.txt"), 1, "one refusal, then nothing more");
    assert_eq!(reason_of(&w, "n1.txt").as_deref(), Some(Reason::WaitingForSpace.key()));
    assert!(w.rows().iter().all(|r| r.state == OutboxState::Ready), "{:?}", w.summary());
    let counts = engine.status().counts;
    assert_eq!((counts.space_waiting, counts.space_waiting_bytes, counts.blocked), (2, 6, 0));

    w.rename_entry("old.txt", "moved.txt");
    w.remove_entry("gone.txt");
    w.h.drain(&engine);
    assert!(w.content("moved.txt").is_some() || w.id_at("moved.txt").is_some(), "{:?}", w.cloud(|c| c.paths()));
    assert!(w.id_at("gone.txt").is_none());
    assert_eq!(content_requests(&w, "n1.txt") + content_requests(&w, "n2.txt"), 1);
    assert_eq!(w.rows().len(), 2, "{:?}", w.summary());

    w.cloud(|c| c.free = Some(10 << 20));
    refresh(&w, &engine);
    assert!(!engine.space_full());
    w.h.drain(&engine);
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_committed(&w, "n1.txt", "n1.txt");
    assert_committed(&w, "n2.txt", "n2.txt");
}

/// Space left: the refused big file waits as too big while the
/// small ones go; it is not sent again until a quota read shows it fits —
/// the automatic read every 30 minutes, on a fake clock.
#[test]
fn a_file_too_big_for_the_space_left_waits_alone() {
    let w = World::new(&[]);
    let big = vec![7u8; 1536 * 1024];
    w.cloud(|c| c.free = Some(1280 * 1024));
    w.make_file("big.bin", &big);
    w.make_file("a.txt", b"a");
    w.make_file("b.txt", b"b");
    let engine = w.run();
    assert!(!engine.space_full());
    assert_committed(&w, "a.txt", "a.txt");
    assert_committed(&w, "b.txt", "b.txt");
    let reason = reason_of(&w, "big.bin").unwrap();
    assert_eq!(Reason::parse(&reason).sizes().map(|(needs, _)| needs), Some(big.len() as u64), "{reason}");
    assert_eq!(engine.status().counts.too_big, 1);
    let sent = content_requests(&w, "big.bin");

    w.h.drain(&engine);
    let now = engine::now();
    w.h.runtime.block_on(engine.space_check(now));
    assert_eq!((w.cloud(|c| c.quota_reads()), content_requests(&w, "big.bin")), (1, sent), "neither read again nor sent");

    // Thirty minutes on: read again, still too big.
    w.h.runtime.block_on(engine.space_check(now + 31 * 60));
    w.h.drain(&engine);
    assert_eq!((w.cloud(|c| c.quota_reads()), content_requests(&w, "big.bin")), (2, sent));

    // Space freed elsewhere: the next read lets it go.
    w.cloud(|c| c.free = Some(5 << 20));
    w.h.runtime.block_on(engine.space_check(now + 62 * 60));
    w.h.drain(&engine);
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_committed(&w, "big.bin", "big.bin");
}

/// The quota is the account's one: a read the account's info made a moment
/// ago is what a refusal uses instead of asking again, a read the worker makes shows in it,
/// and what goes up comes off what is left and onto what is used.
#[test]
fn the_worker_reads_and_adjusts_the_accounts_one_quota() {
    let w = World::new(&[]);
    let quota = |s: &crate::account::state::AccountSnapshot| (s.quota.used, s.quota.total, s.quota.remaining, s.quota.state.clone());
    w.h.quota.read(&konedrive_graph::drive::DriveQuota { total: 10 << 20, used: 1 << 20, remaining: Some(1280 * 1024), state: "normal".into() });
    let big = vec![7u8; 1536 * 1024];
    w.cloud(|c| c.free = Some(1280 * 1024));
    w.make_file("big.bin", &big);
    w.make_file("a.txt", b"abc");
    let engine = w.run();
    assert_eq!(w.cloud(|c| c.quota_reads()), 0, "the account's read of a moment ago decided the refusal");
    let reason = reason_of(&w, "big.bin").unwrap();
    assert_eq!(Reason::parse(&reason).sizes().map(|(needs, _)| needs), Some(big.len() as u64), "{reason}");
    assert_eq!(quota(&w.h.quota.state().get()), ((1 << 20) + 3, 10 << 20, 1280 * 1024 - 3, "normal".into()), "a.txt went up");

    w.cloud(|c| c.free = Some(5 << 20));
    w.h.runtime.block_on(engine.space_check(engine::now() + 31 * 60));
    assert_eq!(w.cloud(|c| c.quota_reads()), 1);
    assert_eq!(quota(&w.h.quota.state().get()), ((1 << 20) + 3, 10 << 20, 5 << 20, "normal".into()), "the worker's read");
    w.h.drain(&engine);
    assert_committed(&w, "big.bin", "big.bin");
    let used = (1 << 20) + 3 + big.len() as u64;
    assert_eq!(quota(&w.h.quota.state().get()), (used, 10 << 20, (5 << 20) - big.len() as u64, "normal".into()));
}

/// Outside full, a file never refused is sent even when the free
/// space known says it does not fit — OneDrive has the last word.
#[test]
fn a_file_not_refused_goes_whatever_the_known_free_space_says() {
    let w = World::new(&[]);
    let engine = w.h.engine();
    w.h.block_on(engine.apply_quota(&konedrive_graph::drive::DriveQuota { total: 0, used: 0, remaining: Some(space::NO_SPACE + 1), state: "critical".into() }));
    w.make_file("big.bin", &vec![1u8; 1536 * 1024]);
    w.h.drain(&engine);
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_committed(&w, "big.bin", "big.bin");
}

/// Rows an earlier version blocked on a full OneDrive wait for
/// space from the start, and one quota read decides them.
#[test]
fn rows_blocked_on_a_full_onedrive_before_wait_for_space_after_a_start() {
    let w = World::new(&[]);
    w.make_file("full.txt", b"f");
    let seq = w.rows()[0].seq;
    w.store.call_blocking(move |s| s.outbox_set_state(seq, OutboxState::Blocked, Some(&Reason::Quota), Some(engine::now() + 1800))).unwrap();
    w.cloud(|c| c.free = Some(10 << 20));
    let engine = w.h.engine();
    w.h.block_on(engine.space_start());
    assert!(engine.space_full(), "waiting rows keep the worker full until the quota is read");
    assert_eq!(w.rows()[0].state, OutboxState::Ready);
    w.h.drain(&engine);
    assert_eq!(w.cloud(|c| c.quota_reads()), 1);
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_committed(&w, "full.txt", "full.txt");
}

/// A file removed while its upload was under way — the
/// daemon stopped mid-session, the removal recorded (a `delete` behind the
/// `running` create) — leaves the outbox at once although OneDrive is full
/// and content rows are not taken: the session cancelled, nothing sent, one
/// `not-uploaded` event, and the account still full.
#[test]
fn a_file_removed_while_it_waits_for_space_leaves_the_outbox() {
    let w = World::new(&[]);
    w.make_file("big.bin", &vec![3u8; 1024 * 1024 + 77]);
    let crashed = w.h.engine();
    crashed.arm(Fault::MidSession(1));
    w.h.drain(&crashed);
    let session = w.rows()[0].session_url.clone().expect("a session is open");
    w.remove_entry("big.bin");
    assert_eq!(w.summary(), vec![(Create, "big.bin".into(), OutboxState::Running), (OutboxKind::Delete, "big.bin".into(), OutboxState::Ready)]);

    let engine = w.h.engine();
    w.h.block_on(engine.apply_quota(&konedrive_graph::drive::DriveQuota { total: 0, used: 0, remaining: Some(0), state: "exceeded".into() }));
    let from = w.cloud(|c| c.log.len());
    w.h.drain(&engine);
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert!(engine.space_full(), "still full: only a quota read ends it");
    let sid = session.as_str().rsplit('/').next().unwrap();
    assert_eq!(w.cloud(|c| c.log[from..].to_vec()), vec![("DELETE".to_owned(), format!("upload/{sid}"))], "the session cancelled, nothing sent");
    assert!(w.h.host.kinds().contains(&ActivityKind::NotUploaded), "{:?}", w.h.host.kinds());
}
