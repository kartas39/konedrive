use super::*;

fn fd() -> OwnedFd {
    tempfile::tempfile().unwrap().into()
}

fn owner(uid: u32, conn: u64) -> Owner {
    Owner { uid, conn }
}

#[test]
fn the_first_opener_claims_the_job_and_the_rest_wait() {
    let mut jobs = Jobs::default();
    let a = owner(1000, 1);
    assert_eq!(jobs.enroll((42, 7), a, fd(), 0).outcome, Enrolled::New { req_id: 1 });
    assert_eq!(jobs.enroll((42, 7), a, fd(), 0).outcome, Enrolled::Existing { req_id: 1 });
    assert_eq!(jobs.enroll((42, 8), a, fd(), 0).outcome, Enrolled::New { req_id: 2 });
}

/// The race this ordering exists to close: the descriptor is in the job
/// the instant the request id exists, so there is no window in which a
/// `finish` can drain a job whose waiter has not been recorded.
#[test]
fn enrolling_registers_the_waiter_in_the_same_step_as_the_claim() {
    let mut jobs = Jobs::default();
    let a = owner(1000, 1);
    let Enrolled::New { req_id } = jobs.enroll((42, 7), a, fd(), 0).outcome else {
        panic!("expected a new job")
    };
    let _ = jobs.enroll((42, 7), a, fd(), 0);
    let _ = jobs.enroll((42, 7), a, fd(), 0);
    assert_eq!(jobs.finish(req_id, a).unwrap().waiters.len(), 3, "three openers were waiting");
    assert_eq!(
        jobs.enroll((42, 7), a, fd(), 0).outcome,
        Enrolled::New { req_id: 2 },
        "the inode is free again, and only claims consume a request id"
    );
}

/// second guard needs to know when a job began: the count
/// of unregistrations when the open that *created* it was read. An opener
/// that joins later brings a later count, which must not replace it.
#[test]
fn a_finished_job_carries_the_count_of_the_open_that_created_it() {
    let mut jobs = Jobs::default();
    let a = owner(1000, 1);
    let Enrolled::New { req_id } = jobs.enroll((42, 7), a, fd(), 3).outcome else {
        panic!("expected a new job")
    };
    let _ = jobs.enroll((42, 7), a, fd(), 5);
    assert_eq!(jobs.finish(req_id, a).unwrap().since, 3);
}

#[test]
fn another_user_cannot_finish_someone_elses_hydration() {
    let mut jobs = Jobs::default();
    let mine = owner(1000, 1);
    let Enrolled::New { req_id } = jobs.enroll((42, 7), mine, fd(), 0).outcome else {
        panic!("expected a new job")
    };
    assert!(jobs.finish(req_id, owner(1001, 2)).is_none(), "wrong uid");
    assert!(jobs.finish(req_id, owner(1000, 2)).is_none(), "right uid, wrong connection");
    assert!(jobs.finish(999, mine).is_none(), "unknown request id");
    assert!(jobs.finish(req_id, mine).is_some(), "the owner can still finish it");
}

#[test]
fn a_disconnect_only_touches_that_connections_jobs() {
    let mut jobs = Jobs::default();
    let mine = owner(1000, 1);
    let theirs = owner(1001, 2);
    let Enrolled::New { req_id } = jobs.enroll((42, 7), mine, fd(), 0).outcome else {
        panic!("expected a new job")
    };
    let _ = jobs.enroll((42, 8), theirs, fd(), 0);

    let drained = jobs.retire(2);
    assert_eq!(drained.len(), 1, "only the disconnecting connection's job");
    assert_eq!(jobs.in_flight(), 1, "the other user's hydration survives");
    assert!(jobs.finish(req_id, mine).is_some(), "and can still be finished normally");
}

/// The window this closes: a worker holding a `Daemon` clone
/// from just before the cleanup ran must not be able to create a job on
/// a connection whose jobs have already been drained — nothing would
/// ever answer it, and there is no per-job timeout to rescue it.
#[test]
fn a_retired_connection_cannot_be_enrolled_against() {
    let mut jobs = Jobs::default();
    let gone = owner(1000, 1);
    let _ = jobs.enroll((42, 7), gone, fd(), 0);
    assert_eq!(jobs.retire(1).len(), 1);

    let enrollment = jobs.enroll((42, 9), gone, fd(), 0);
    assert_eq!(enrollment.outcome, Enrolled::ConnectionGone);
    assert_eq!(enrollment.evicted.len(), 1, "the descriptor comes back to be answered");
    assert_eq!(jobs.in_flight(), 0, "and no job is left behind for nobody to finish");

    // A fresh connection from the same user is unaffected.
    let fresh = owner(1000, 2);
    assert_eq!(jobs.enroll((42, 9), fresh, fd(), 0).outcome, Enrolled::New { req_id: 2 });
}

/// The set of retired connections is bounded, so a machine that
/// reconnects daemons all day does not grow it without end.
#[test]
fn the_retired_set_is_bounded() {
    let mut jobs = Jobs::default();
    for conn in 0..(RETIRED_REMEMBERED as u64 * 2) {
        let _ = jobs.retire(conn);
    }
    assert_eq!(jobs.retired.len(), RETIRED_REMEMBERED);
    assert_eq!(jobs.retired_order.len(), RETIRED_REMEMBERED);
    assert!(jobs.retired.contains(&(RETIRED_REMEMBERED as u64 * 2 - 1)), "the newest is kept");
    assert!(!jobs.retired.contains(&0), "the oldest is forgotten");
}

const MAX: u64 = MAX_OUTSTANDING_HYDRATIONS as u64;

/// Fills `conn`'s credit with hydrations of inodes `0..MAX`, then enrolls
/// `extra` more, and returns the extra ones' request ids.
fn beyond_the_credit(jobs: &mut Jobs, who: Owner, extra: u64) -> Vec<u64> {
    for ino in 0..MAX {
        let enrollment = jobs.enroll((42, ino), who, fd(), 0);
        assert!(matches!(enrollment.outcome, Enrolled::New { .. }));
        assert!(enrollment.dispatch.is_some_and(|d| d.fd.is_ok()), "and is sent at once");
    }
    (MAX..MAX + extra)
        .map(|ino| match jobs.enroll((42, ino), who, fd(), 0) {
            Enrollment { outcome: Enrolled::Queued { req_id }, evicted, dispatch } => {
                assert!(evicted.is_empty() && dispatch.is_none());
                req_id
            }
            other => panic!("beyond the credit a new hydration must queue: {:?}", other.outcome),
        })
        .collect()
}

/// Beyond its connection's credit a new hydration is
/// enrolled and held back — its opener suspended like any other — not
/// refused, and nothing more is handed to the daemon than its request
/// queue has room for (the circular wait `MAX_OUTSTANDING_HYDRATIONS`
/// exists to prevent).
#[test]
fn beyond_the_credit_a_new_hydration_waits_instead_of_being_refused() {
    let mut jobs = Jobs::default();
    let a = owner(1000, 1);
    let queued = beyond_the_credit(&mut jobs, a, 100);
    assert_eq!(queued.len(), 100);
    assert_eq!(jobs.outstanding_for(1), MAX_OUTSTANDING_HYDRATIONS, "no more than the credit");
    assert_eq!(jobs.queued_for(1), 100, "and every one beyond it waiting");
    assert_eq!(jobs.in_flight(), MAX_OUTSTANDING_HYDRATIONS + 100, "nobody refused");

    assert_eq!(
        jobs.enroll((42, MAX + 3), a, fd(), 0).outcome,
        Enrolled::Existing { req_id: queued[3] },
        "joining a queued hydration is joining it"
    );
    assert!(
        matches!(jobs.enroll((42, 9999), owner(1001, 2), fd(), 0).outcome, Enrolled::New { .. }),
        "another connection has a credit of its own"
    );
}

/// Every credit that comes back goes to the oldest waiting
/// hydration, in the same step, until none is left waiting — so every
/// opener beyond the credit is eventually answered, in arrival order.
#[test]
fn each_returned_credit_sends_the_oldest_waiting_hydration() {
    let mut jobs = Jobs::default();
    let a = owner(1000, 1);
    let queued = beyond_the_credit(&mut jobs, a, 100);

    let mut answered = 0;
    let mut sent_later = Vec::new();
    let mut in_hand: VecDeque<u64> = (1..=MAX).collect();
    while let Some(req_id) = in_hand.pop_front() {
        let finished = jobs.finish(req_id, a).expect("a sent hydration can be finished");
        answered += finished.waiters.len();
        if let Some(next) = finished.next {
            assert!(next.fd.is_ok(), "the promoted request carries a descriptor to fill");
            sent_later.push(next.req_id);
            in_hand.push_back(next.req_id);
        }
        assert!(jobs.outstanding_for(1) <= MAX_OUTSTANDING_HYDRATIONS);
    }
    assert_eq!(sent_later, queued, "every waiting hydration is sent, oldest first");
    assert_eq!(answered, MAX_OUTSTANDING_HYDRATIONS + 100, "and every opener answered");
    assert_eq!(jobs.in_flight(), 0);
    assert!(jobs.queued.is_empty() && jobs.outstanding.is_empty(), "nothing is left behind");
}

/// A queued hydration's request id has not been handed to anybody, so an
/// answer to it is a guess, and a guess must not finish it — nor take a
/// credit it does not hold.
#[test]
fn a_hydration_never_sent_cannot_be_finished() {
    let mut jobs = Jobs::default();
    let a = owner(1000, 1);
    let queued = beyond_the_credit(&mut jobs, a, 1);
    assert!(jobs.finish(queued[0], a).is_none(), "its daemon was never asked for it");
    assert_eq!(jobs.queued_for(1), 1, "and it still waits for its turn");
    assert_eq!(jobs.outstanding_for(1), MAX_OUTSTANDING_HYDRATIONS);
}

/// The disconnect half: a hydration waiting for credit is
/// its connection's as much as a sent one, and goes with it — its
/// openers come back to be denied, and no credit is left to send it with.
#[test]
fn queued_hydrations_are_taken_with_their_connection() {
    let mut jobs = Jobs::default();
    let a = owner(1000, 1);
    let queued = beyond_the_credit(&mut jobs, a, 10);
    let _ = jobs.enroll((42, MAX + 1), a, fd(), 0);

    let drained = jobs.retire(1);
    assert_eq!(drained.len(), MAX_OUTSTANDING_HYDRATIONS + 10, "sent and queued alike");
    assert_eq!(
        drained.iter().map(Vec::len).sum::<usize>(),
        MAX_OUTSTANDING_HYDRATIONS + 11,
        "with every opener joined to them"
    );
    assert_eq!(jobs.in_flight(), 0);
    assert!(jobs.queued.is_empty() && jobs.outstanding.is_empty());
    assert!(jobs.finish(queued[0], a).is_none());
}

/// The credit must follow a hydration through every way it can end, or
/// a connection would drift towards "no credit" for good and every open
/// for its user would wait forever. Finished and retired return it. An
/// eviction — another uid's hydration of the same inode — does **not**:
/// the daemon still holds that request and will answer it, and handing
/// its credit on early would let one more request in than the daemon's
/// queue has room for. Its answer returns it.
#[test]
fn the_credit_follows_every_way_a_hydration_ends() {
    let mut jobs = Jobs::default();
    let first = owner(1000, 1);
    let other_uid = owner(1001, 2);
    let queued = beyond_the_credit(&mut jobs, first, 1);

    let finished = jobs.finish(1, first).unwrap();
    assert_eq!(
        finished.next.map(|next| next.req_id),
        Some(queued[0]),
        "a finished hydration hands its credit to the next"
    );
    assert_eq!(jobs.outstanding_for(1), MAX_OUTSTANDING_HYDRATIONS);

    // Request 2 is inode (42, 1), sent. The file changes owner.
    let rechowned = jobs.enroll((42, 1), other_uid, fd(), 0);
    assert_eq!(rechowned.evicted.len(), 1, "its opener comes back to be answered");
    assert_eq!(
        jobs.outstanding_for(1),
        MAX_OUTSTANDING_HYDRATIONS,
        "but the daemon still holds that request, so the credit stays taken"
    );
    let answered = jobs.finish(2, first).expect("the daemon's answer is still accepted");
    assert!(answered.waiters.is_empty(), "with nobody left to answer");
    assert_eq!(jobs.outstanding_for(1), MAX_OUTSTANDING_HYDRATIONS - 1, "and it returns it");

    assert_eq!(jobs.retire(1).len(), MAX_OUTSTANDING_HYDRATIONS - 1);
    assert_eq!(jobs.outstanding_for(1), 0, "a retired connection's hydrations no longer count");
    assert!(!jobs.outstanding.contains_key(&1), "and a connection with none holds no entry");
    assert_eq!(jobs.outstanding_for(2), 1, "nor does retiring one touch another");
}

/// A hydration evicted while it was still waiting for credit is gone for
/// good: nobody was asked for it, and its turn is skipped rather than
/// sending a request for a job with no openers.
#[test]
fn an_evicted_hydration_that_never_went_out_loses_its_turn() {
    let mut jobs = Jobs::default();
    let first = owner(1000, 1);
    let queued = beyond_the_credit(&mut jobs, first, 2);

    let rechowned = jobs.enroll((42, MAX), owner(1001, 2), fd(), 0);
    assert_eq!(rechowned.evicted.len(), 1);
    assert_eq!(jobs.queued_for(1), 1);

    let next = jobs.finish(1, first).unwrap().next.map(|next| next.req_id);
    assert_eq!(next, Some(queued[1]), "the evicted one's turn is skipped");
    assert_eq!(jobs.outstanding_for(1), MAX_OUTSTANDING_HYDRATIONS);
}

/// A uid's hydrations go to its newest connection, and an
/// older one stays live underneath it. An opener routed to the newer
/// connection, for a file the older one is already hydrating, joins that
/// hydration: the older connection will answer it with `HydrateDone`, or,
/// if it is in fact on its way out, its disconnect guard denies every
/// waiter on it. Starting over instead — as this did while one connection
/// per uid meant "the other one is dead" — denied the live daemon's
/// waiters `EIO` for nothing but a second connection appearing, and asked
/// the newcomer for a file already being filled.
#[test]
fn an_opener_joins_a_hydration_an_older_live_connection_has_in_hand() {
    let mut jobs = Jobs::default();
    let live = owner(1000, 1);
    let newer = owner(1000, 2);
    let _ = jobs.enroll((42, 7), live, fd(), 0);
    let _ = jobs.enroll((42, 7), live, fd(), 0);

    let joined = jobs.enroll((42, 7), newer, fd(), 0);
    assert!(joined.evicted.is_empty(), "the live daemon's waiters must not be denied");
    assert_eq!(joined.outcome, Enrolled::Existing { req_id: 1 }, "it joins the job in hand");
    assert_eq!(jobs.in_flight(), 1, "and asks nobody for the file again");
    assert!(jobs.finish(1, newer).is_none(), "only the connection that was asked answers it");
    assert_eq!(jobs.finish(1, live).unwrap().waiters.len(), 3, "and it answers all three");
}

/// The other half: if the older connection was on its way out after all,
/// its disconnect guard takes the joined opener along with its own, so
/// nobody is stranded.
#[test]
fn a_joined_opener_is_answered_when_the_older_connection_goes() {
    let mut jobs = Jobs::default();
    let old = owner(1000, 1);
    let newer = owner(1000, 2);
    let _ = jobs.enroll((42, 7), old, fd(), 0);
    let _ = jobs.enroll((42, 7), newer, fd(), 0);
    let drained = jobs.retire(1);
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].len(), 2, "both openers come back to be denied");
    assert_eq!(jobs.in_flight(), 0);
    assert!(
        matches!(jobs.enroll((42, 7), newer, fd(), 0).outcome, Enrolled::New { .. }),
        "and the next opener starts afresh on the connection that is left"
    );
}

/// Another uid's hydration of the same inode — the file changed owner
/// while it was being filled — is not joined: that daemon was asked on
/// behalf of somebody else. Its waiters are handed back to be answered,
/// rather than left to a daemon that is no longer the file's owner's,
/// and this open starts a hydration of its own.
#[test]
fn another_uids_hydration_of_the_same_inode_is_not_joined() {
    let mut jobs = Jobs::default();
    let before = owner(1000, 1);
    let after = owner(1001, 2);
    let _ = jobs.enroll((42, 7), before, fd(), 0);
    let _ = jobs.enroll((42, 7), before, fd(), 0);

    let enrollment = jobs.enroll((42, 7), after, fd(), 0);
    assert_eq!(enrollment.outcome, Enrolled::New { req_id: 2 });
    assert_eq!(enrollment.evicted.len(), 2, "both stranded openers come back to be answered");
    assert_eq!(jobs.finish(2, after).unwrap().waiters.len(), 1);
}

/// What `packaging/systemd/konedrive-helper.service` gives the helper:
/// `LimitNOFILE=65536`.
const HELPER_DESCRIPTORS: usize = 65536;

/// Lets this test process hold `wanted` descriptors at once, where its hard
/// limit allows it.
fn allow_descriptors(wanted: u64) {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: `limit` is a live, correctly sized `rlimit` for both calls.
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
        if limit.rlim_cur < wanted {
            limit.rlim_cur = wanted.min(limit.rlim_max);
            assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &limit), 0);
        }
    }
    assert!(limit.rlim_cur >= wanted, "this test needs {wanted} descriptors; the hard limit is lower");
}

/// One user must not be able to take every descriptor the helper has. Each
/// suspended open is an event fd kept in its job until the daemon answers,
/// and a daemon is free never to answer: the user's own program can connect,
/// register a folder of its own, take the requests and say nothing, while its
/// threads open placeholders in that folder. Once the helper is out of
/// descriptors the kernel denies every other user's intercepted open `EPERM`
/// and the helper denies the rest `EIO`
/// (`docs/kernel-behavior-7.2/suite.md`, "A real `EMFILE` does not end the
/// helper").
///
/// Run alone: it raises the process's descriptor limit and holds 65 536.
#[test]
#[ignore = "shows HE1: no bound per uid on suspended opens"]
fn one_uid_cannot_take_every_descriptor_the_helper_has() {
    allow_descriptors(HELPER_DESCRIPTORS as u64 + 1024);
    let mut jobs = Jobs::default();
    let silent = owner(1000, 1);
    let event = fd();
    for ino in 0..HELPER_DESCRIPTORS as u64 {
        let opener = event.try_clone().expect("the test process has descriptors left");
        // Whatever comes back — a request to send, an opener refused — is
        // closed here; what the job table keeps is what counts.
        drop(jobs.enroll((42, ino), silent, opener, 0));
    }
    let held: usize = jobs.jobs.values().map(|job| job.waiters.len()).sum();
    assert!(
        held < HELPER_DESCRIPTORS,
        "uid 1000, whose daemon answered nothing, holds {held} suspended opens: every one of the \
         {HELPER_DESCRIPTORS} descriptors the unit allows the helper, and nobody was refused"
    );
}
