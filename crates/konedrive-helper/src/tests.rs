use super::*;

/// A registration for connection `conn` of `uid`, backed by a real
/// outbox on a socket pair nobody reads — the registry only ever
/// looks at `uid` and `conn`.
fn daemon(uid: u32, conn: u64) -> (Daemon, UnixStream) {
    daemon_of_pid(uid, conn, 1)
}

fn daemon_of_pid(uid: u32, conn: u64, pid: i32) -> (Daemon, UnixStream) {
    use nix::sys::socket::socketpair;
    let (ours, theirs) =
        socketpair(AddressFamily::Unix, SockType::SeqPacket, None, SockFlag::empty()).unwrap();
    let ours = UnixStream::from(ours);
    let outbox = Outbox::start(ours.try_clone().unwrap(), ours, "test").unwrap();
    (Daemon { conn, uid, pid, outbox: Arc::new(outbox) }, UnixStream::from(theirs))
}

fn registered(daemons: &Registry, uid: u32) -> Option<u64> {
    daemons.top(uid).map(|d| d.conn)
}

/// The interleaving the VM suite hit. Two connections from
/// one uid: connection 1, accepted first, is a throwaway that connects
/// and drops; connection 2, accepted second, is the live daemon. Their
/// threads run in the opposite order, so the live daemon registers first
/// and the throwaway registers *last* — and then goes away.
///
/// Before the fix the late insert replaced the live daemon, the
/// throwaway's cleanup then found itself registered and removed it, and
/// the uid was left with no daemon while its daemon's socket was still
/// open: every placeholder open waited `DAEMON_WAIT` and was denied.
#[test]
fn an_older_connection_that_registers_late_does_not_evict_a_live_daemon() {
    let mut daemons = Registry::default();
    let (live, _live_peer) = daemon(1000, 2);
    let (throwaway, throwaway_peer) = daemon(1000, 1);

    assert!(daemons.register(live), "the first registration always lands");
    let late = daemons.register(throwaway);

    // The throwaway goes away; its `Disconnect` guard runs.
    drop(throwaway_peer);
    daemons.deregister(1000, 1);
    assert_eq!(
        registered(&daemons, 1000),
        Some(2),
        "the live daemon must still be the one registered after the throwaway's cleanup"
    );
    assert!(
        !late,
        "an older connection must not replace a newer one, whatever order they arrive in"
    );
}

/// The other half, which must keep working: a newer connection from the
/// same uid — a daemon that reconnected — does replace the older one, and
/// the older one's cleanup then leaves it alone.
#[test]
fn a_newer_connection_replaces_an_older_one_and_survives_its_cleanup() {
    let mut daemons = Registry::default();
    let (old, _old_peer) = daemon(1000, 1);
    let (new, _new_peer) = daemon(1000, 2);

    assert!(daemons.register(old));
    assert!(daemons.register(new), "a reconnecting daemon must take over");
    daemons.deregister(1000, 1);
    assert_eq!(registered(&daemons, 1000), Some(2));

    daemons.deregister(1000, 2);
    assert_eq!(registered(&daemons, 1000), None, "and its own cleanup removes it");
}

/// The sequential case H120 left open. A process of the
/// daemon's own uid connects after it — newest wins, so it takes over —
/// and then goes away. The live daemon underneath must get the uid back:
/// its socket is still open, so nothing will ever make it reconnect, and
/// a uid left with no registration has every open wait `DAEMON_WAIT` and
/// then be denied `EIO`, until the daemon restarts.
#[test]
fn a_newer_connection_that_goes_away_hands_the_uid_back_to_the_live_one() {
    let mut daemons = Registry::default();
    let (live, _live_peer) = daemon(1000, 1);
    let (transient, _transient_peer) = daemon(1000, 2);

    daemons.register(live);
    daemons.register(transient);
    assert_eq!(registered(&daemons, 1000), Some(2), "the newer connection takes over");

    daemons.deregister(1000, 2);
    assert_eq!(
        registered(&daemons, 1000),
        Some(1),
        "when the newer connection goes, the live daemon underneath must be the one \
         hydrations go to again"
    );
}

/// The exemption follows the top. One pid per
/// uid is exempt at any moment — the one the uid's hydrations go to —
/// and it passes back down when the connection above it goes. A live
/// connection underneath is not exempt while it is not the top, and no
/// connection is ever exempt for another uid.
#[test]
fn only_the_top_connections_pid_is_exempt() {
    let mut daemons = Registry::default();
    let (live, _live_peer) = daemon_of_pid(1000, 1, 10);
    let (transient, _transient_peer) = daemon_of_pid(1000, 2, 20);
    daemons.register(live);
    assert!(daemons.is_top_pid(1000, 10), "a lone daemon is exempt for its uid");

    daemons.register(transient);
    assert!(daemons.is_top_pid(1000, 20), "the newer connection is on top");
    assert!(!daemons.is_top_pid(1000, 10), "and the one underneath is no longer exempt");
    assert!(!daemons.is_top_pid(1001, 20), "nor is anybody exempt for another uid");

    daemons.deregister(1000, 2);
    assert!(daemons.is_top_pid(1000, 10), "the exemption goes back down with the top");
    assert!(!daemons.is_top_pid(1000, 20), "and leaves with the connection that left");
}

/// Removing a connection from the middle of a stack keeps the order of
/// the rest, and a uid whose last connection goes holds no entry at all.
#[test]
fn a_connection_leaves_the_stack_from_wherever_it_sits() {
    let mut daemons = Registry::default();
    let mut peers = Vec::new();
    for conn in 1..=3 {
        let (d, peer) = daemon(1000, conn);
        peers.push(peer);
        assert!(daemons.register(d), "each newer connection lands on top");
    }
    daemons.deregister(1000, 2);
    assert_eq!(registered(&daemons, 1000), Some(3), "the top is untouched");
    daemons.deregister(1000, 3);
    assert_eq!(registered(&daemons, 1000), Some(1), "and the oldest is under it");
    daemons.deregister(1000, 1);
    assert_eq!(registered(&daemons, 1000), None);
    assert!(daemons.by_uid.is_empty(), "no empty stack is kept");
    daemons.deregister(1000, 1);
    assert!(daemons.by_uid.is_empty(), "and removing it twice is harmless");
}

/// Ordering is per uid: another uid's newer connection is not a reason to
/// refuse this one.
#[test]
fn connection_order_is_compared_only_within_one_uid() {
    let mut daemons = Registry::default();
    let (other, _other_peer) = daemon(1001, 5);
    let (ours, _our_peer) = daemon(1000, 3);
    assert!(daemons.register(other));
    assert!(daemons.register(ours));
    assert_eq!(registered(&daemons, 1000), Some(3));
    assert_eq!(registered(&daemons, 1001), Some(5));
}

/// However a run of refusals is split into lines, the lines
/// add up to the refusals: the first is written at once, the rest of its
/// interval only counted, and the count after the last line is written
/// when the interval ends — by `flush`, since a burst that has stopped
/// has no next refusal to carry it. The event loop's throttle, which this
/// one is modelled on, never wrote that last count.
#[test]
fn a_throttle_accounts_for_every_occurrence() {
    let every = Duration::from_millis(50);
    let mut throttle = Throttle::every(every);
    let mut written: Vec<u64> = Vec::new();
    written.extend(throttle.admit());
    assert_eq!(written, [1], "the first occurrence is written at once");
    for _ in 0..99 {
        written.extend(throttle.admit());
    }
    assert_eq!(written, [1], "the rest of its interval is only counted");
    assert_eq!(throttle.flush(), None, "and not written before the interval is over");

    std::thread::sleep(every * 2);
    written.extend(throttle.flush());
    assert_eq!(written, [1, 99], "the tail is written though nothing came after it");
    assert_eq!(throttle.flush(), None, "once");

    written.extend(throttle.admit());
    std::thread::sleep(every * 2);
    written.extend(throttle.flush());
    assert_eq!(written.iter().sum::<u64>(), 101, "every occurrence is in some line");
}

/// A condition that clears — descriptors come back, `accept` works again
/// — hands back the count no line had written, so the recovery line can
/// say it; the next occurrence is then written at once again.
#[test]
fn a_throttle_reset_hands_back_what_it_had_not_written() {
    let mut throttle = Throttle::every(Duration::from_secs(60));
    assert_eq!(throttle.admit(), Some(1));
    assert_eq!(throttle.admit(), None);
    assert_eq!(throttle.admit(), None);
    assert_eq!(throttle.reset(), 2, "the two nobody wrote down");
    assert_eq!(throttle.admit(), Some(1), "and the next one is written at once");
}

/// Each kind of refusal has a throttle of its own, found by its index, so
/// that a flood of one never silences another; and each summary keeps the
/// words of its per-occurrence line, which is what anyone searching the
/// journal — the VM suite included — looks for.
#[test]
fn every_kind_of_refusal_has_its_own_throttle_and_keeps_its_words() {
    for (index, kind) in Refusal::ALL.iter().enumerate() {
        assert_eq!(*kind as usize, index, "{kind:?} would share another kind's throttle");
    }
    let refusals = Refusals::new();
    assert_eq!(refusals.throttles.len(), Refusal::ALL.len());
    refusals.report(Refusal::PoolFull, || "first".into());
    assert_eq!(
        lock(&refusals.throttles[Refusal::NoRoot as usize]).admit(),
        Some(1),
        "another kind's first refusal is still written at once"
    );
    for (kind, words) in [
        (Refusal::PoolFull, "workers busy and"),
        (Refusal::NoRoot, "no registered root"),
        (Refusal::TooManyWaiters, "waiter backstop"),
        (Refusal::TimedOut, "did not connect within"),
        (Refusal::Undeliverable, "could not be queued"),
        (Refusal::StrayDone, "ignoring HydrateDone"),
        (Refusal::TooManyConnections, "already holding"),
        (Refusal::Unopenable, "could not hand over"),
        (Refusal::EventFdFailed, "could not open the descriptor"),
    ] {
        assert!(kind.summary().contains(words), "{kind:?}'s summary lost {words:?}");
    }
}

/// second guard, kept per uid: an unregistration withholds
/// ignore marks only from its own user's files, so nobody can keep other
/// users' files unmarked by unregistering roots of their own in a loop.
#[test]
fn an_unregistration_counts_against_its_own_users_files_only() {
    let unregistrations = Unregistrations::new();
    let read = unregistrations.now();
    assert!(!unregistrations.since(read, Some(1000)), "nothing has happened yet");

    unregistrations.bump(1000);
    assert!(unregistrations.since(read, Some(1000)), "the walk began after the read");
    assert!(!unregistrations.since(read, Some(1001)), "and it was not user 1001's root");
    assert!(unregistrations.since(read, None), "a file whose owner is unknown counts it");
    let later = unregistrations.now();
    unregistrations.bump(1000);
    assert!(unregistrations.since(later, Some(1000)), "the walk's end counts too");
    assert!(!unregistrations.since(unregistrations.now(), Some(1000)), "read after both");
}

/// Past what is remembered, nobody can say whose an unregistration was,
/// so it counts against everyone.
#[test]
fn an_unregistration_that_is_no_longer_remembered_counts_against_everyone() {
    let unregistrations = Unregistrations::new();
    let read = unregistrations.now();
    for _ in 0..=UNREGISTRATIONS_REMEMBERED {
        unregistrations.bump(1000);
    }
    assert!(unregistrations.since(read, Some(1001)));
}

/// one uid's connections are bounded, another
/// uid's are not affected, and a place comes back when its connection
/// goes.
#[test]
fn one_uid_holds_at_most_its_connections_and_no_more() {
    let counters = Arc::new(Mutex::new(HashMap::new()));
    let held: Vec<ConnectionSlot> = (0..MAX_CONNECTIONS_PER_UID)
        .map(|_| ConnectionSlot::take(&counters, 1001).expect("under the bound"))
        .collect();
    assert!(ConnectionSlot::take(&counters, 1001).is_none(), "the bound refuses the next");
    let other = ConnectionSlot::take(&counters, 1000);
    assert!(other.is_some(), "another uid is not affected");
    drop(held);
    assert!(ConnectionSlot::take(&counters, 1001).is_some(), "and places come back");
    drop(other);
}

fn waiting_for(counters: &Mutex<HashMap<u32, usize>>, uid: u32) -> usize {
    lock(counters).get(&uid).copied().unwrap_or(0)
}

/// `wait_for_daemon` is the only place a worker sleeps for
/// tens of seconds, so it is the only lever an unprivileged caller has on
/// the pool. The cap is what makes "open other people's placeholders
/// while their daemon is down" cost at most `MAX_DAEMON_WAITERS` workers
/// instead of every one of them.
#[test]
fn at_most_eight_opens_wait_for_a_daemon_at_once() {
    let counters = Mutex::new(HashMap::new());
    let held: Vec<WaiterSlot<'_>> = (0..MAX_DAEMON_WAITERS)
        .map(|_| WaiterSlot::take(&counters, 1000).expect("under the cap"))
        .collect();
    assert_eq!(waiting_for(&counters, 1000), MAX_DAEMON_WAITERS);
    assert!(WaiterSlot::take(&counters, 1000).is_none(), "the cap must refuse the next one");

    drop(held);
    assert_eq!(waiting_for(&counters, 1000), 0, "every slot is released");
    assert!(lock(&counters).is_empty(), "and a uid with nobody waiting holds no entry");
    assert!(WaiterSlot::take(&counters, 1000).is_some(), "and the next open may wait again");
}

/// The cap is one budget **per uid**, not one for the machine.
///
/// As a single counter it was itself the denial of service it was meant to
/// prevent: a local user could hold all eight slots by opening another
/// user's placeholders, and a third user's legitimate early-boot open —
/// one whose own daemon was seconds from connecting — was then refused
/// `EIO` without waiting at all.
#[test]
fn one_uid_filling_its_slots_does_not_stop_another_waiting() {
    let counters = Mutex::new(HashMap::new());
    let hogged: Vec<WaiterSlot<'_>> = (0..MAX_DAEMON_WAITERS)
        .map(|_| WaiterSlot::take(&counters, 1000).expect("under the cap"))
        .collect();
    assert!(WaiterSlot::take(&counters, 1000).is_none(), "that uid has spent its budget");

    let victim = WaiterSlot::take(&counters, 1001);
    assert!(victim.is_some(), "another uid's open must still be allowed to wait");
    assert_eq!(waiting_for(&counters, 1001), 1);
    assert_eq!(waiting_for(&counters, 1000), MAX_DAEMON_WAITERS, "and budgets do not mix");

    drop(hogged);
    assert_eq!(waiting_for(&counters, 1000), 0);
    assert_eq!(waiting_for(&counters, 1001), 1, "releasing one uid's slots frees only its own");
}

/// The follow-up to: the per-uid cap alone restored fairness
/// between uids at the cost of the flat pool bound the machine used to
/// have — the worst case became `MAX_DAEMON_WAITERS` times the number of
/// uids with a registered root whose daemon is down, which is unbounded
/// on a machine with enough such uids. Five uids spending their whole
/// budget (`5 * MAX_DAEMON_WAITERS == 40`) comfortably exceeds
/// `GLOBAL_MAX_DAEMON_WAITERS` (32) while every one of them stays inside
/// its own per-uid cap the whole time, so nothing here depends on the
/// per-uid cap ever tripping.
#[test]
fn a_global_backstop_binds_even_though_every_uid_stays_under_its_own_cap() {
    let counters = Mutex::new(HashMap::new());
    let mut held = Vec::new();
    for uid in 1000..1005 {
        for _ in 0..MAX_DAEMON_WAITERS {
            if let Some(slot) = WaiterSlot::take(&counters, uid) {
                held.push(slot);
            }
        }
    }
    assert_eq!(
        held.len(),
        GLOBAL_MAX_DAEMON_WAITERS,
        "the machine-wide backstop must bind before every uid reaches its own cap \
         (5 uids * {MAX_DAEMON_WAITERS} each = 40, which must not all be granted)"
    );

    // The discriminator: a uid that has spent none of its own budget is
    // still refused once the backstop is full. A per-uid-only cap would
    // grant this.
    assert!(
        WaiterSlot::take(&counters, 1005).is_none(),
        "a uid with an entirely unspent budget must still be refused once the \
         machine-wide backstop is full"
    );

    drop(held);
    assert!(lock(&counters).is_empty(), "every slot is released");
    assert!(WaiterSlot::take(&counters, 1005).is_some(), "and the backstop frees up again");
}

/// A slot is released however its holder leaves, including by panicking —
/// otherwise one panicking worker would permanently shrink the number of
/// opens that may ever wait.
#[test]
fn a_waiter_slot_is_released_even_if_its_holder_panics() {
    let counters = Mutex::new(HashMap::new());
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _slot = WaiterSlot::take(&counters, 1000).expect("under the cap");
        panic!("as a worker might");
    }));
    assert!(outcome.is_err());
    assert_eq!(waiting_for(&counters, 1000), 0);
}

/// And the governing property of this whole component: an
/// application must never read zeros where real content should be.
///
/// `fanotify(7)` allows every outstanding permission event when the
/// group's descriptor closes, so the helper exiting is **silent data
/// loss**, while the helper denying is an errno the application sees. That
/// asymmetry is what makes running out of descriptors — a resource
/// problem, and a self-correcting one, since the descriptors are held by
/// hydrations that are all going to finish — something to survive rather
/// than something to die of.
#[test]
fn running_out_of_descriptors_does_not_end_the_process() {
    assert_eq!(
        classify_read_failure(Errno::EMFILE),
        ReadFailure::Exhausted,
        "this process being out of descriptors must not close the fanotify group"
    );
    assert_eq!(
        classify_read_failure(Errno::ENFILE),
        ReadFailure::Exhausted,
        "nor must the machine being out of them"
    );
}

/// The other arms, so that "survivable" did not quietly become
/// "everything is survivable": an errno the group fd itself reports still
/// ends the loop, because a group that cannot be read cannot be answered.
#[test]
fn the_event_loops_other_read_failures_keep_their_meaning() {
    assert_eq!(classify_read_failure(Errno::EAGAIN), ReadFailure::Drained);
    assert_eq!(classify_read_failure(Errno::EINTR), ReadFailure::Interrupted);
    for errno in [Errno::EBADF, Errno::EINVAL, Errno::EFAULT] {
        assert_eq!(classify_read_failure(errno), ReadFailure::Fatal, "{errno} is not handled");
    }
}

/// The kernel opens each event's
/// descriptor with the group's `O_RDWR` against the **opener's** mount,
/// and when that open fails `read()` of the group returns its errno for
/// that one event — which the kernel has already denied. Measured:
/// `EROFS` for an open through a read-only mount (a Flatpak app with
/// `home:ro`), `ETXTBSY` for a second open of a running executable. As
/// `Fatal`, either one ended the helper, and every suspended open was
/// then allowed onto its unfilled placeholder: 65 536 zero bytes. Any
/// errno but the three the group descriptor itself can report is one
/// event's, and the loop goes on reading.
#[test]
fn an_event_the_kernel_could_not_hand_over_does_not_end_the_helper() {
    for errno in [
        Errno::EROFS,
        Errno::ETXTBSY,
        Errno::EACCES,
        Errno::EPERM,
        Errno::EIO,
        Errno::ENOMEM,
        Errno::ENXIO,
        Errno::ENODEV,
        Errno::EOVERFLOW,
        Errno::ESTALE,
    ] {
        assert_ne!(
            classify_read_failure(errno),
            ReadFailure::Fatal,
            "{errno}: one event the kernel could not hand over must not close the group — \
             closing it allows every suspended open onto its placeholder"
        );
    }
}

/// As `register_root` asks it: the id names an entry but does
/// not own one, so an entry already held by somebody else is refused and
/// a user's own is a re-registration.
#[test]
fn a_root_id_held_by_another_user_is_refused() {
    let mut roots = roots::Roots::default();
    roots.insert(roots::Root {
        uid: 1000,
        dev: 42,
        ino: 7,
        path: "/home/alice/OneDrive".into(),
        root_id: "shared-id".into(),
    });
    let refused = |uid: u32| roots.owner_of("shared-id").is_some_and(|other| other != uid);
    assert!(refused(1001), "another user must not take over the id");
    assert!(!refused(1000), "the owner re-registering its own root must not be refused");
    assert!(
        !roots.owner_of("unused-id").is_some_and(|other| other != 1001),
        "an unused id is free for anyone"
    );
}
