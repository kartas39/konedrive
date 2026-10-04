use super::*;

use crate::shared::daemons::{
    connection_slots, waiter_slots, GLOBAL_MAX_DAEMON_WAITERS, MAX_CONNECTIONS_PER_UID,
    MAX_DAEMON_WAITERS,
};

/// one uid's connections are bounded, another
/// uid's are not affected, and a place comes back when its connection
/// goes.
#[test]
fn one_uid_holds_at_most_its_connections_and_no_more() {
    let counters = connection_slots();
    let held: Vec<UidSlot> = (0..MAX_CONNECTIONS_PER_UID)
        .map(|_| counters.take(1001).expect("under the bound"))
        .collect();
    assert!(counters.take(1001).is_none(), "the bound refuses the next");
    let other = counters.take(1000);
    assert!(other.is_some(), "another uid is not affected");
    drop(held);
    assert!(counters.take(1001).is_some(), "and places come back");
    drop(other);
}

fn waiting_for(counters: &UidSlots, uid: u32) -> usize {
    lock(&counters.held).get(&uid).copied().unwrap_or(0)
}

/// `Daemons::wait_for` is the only place a worker sleeps for
/// tens of seconds, so it is the only lever an unprivileged caller has on
/// the pool. The cap is what makes "open other people's placeholders
/// while their daemon is down" cost at most `MAX_DAEMON_WAITERS` workers
/// instead of every one of them.
#[test]
fn at_most_eight_opens_wait_for_a_daemon_at_once() {
    let counters = waiter_slots();
    let held: Vec<UidSlot> = (0..MAX_DAEMON_WAITERS)
        .map(|_| counters.take(1000).expect("under the cap"))
        .collect();
    assert_eq!(waiting_for(&counters, 1000), MAX_DAEMON_WAITERS);
    assert!(counters.take(1000).is_none(), "the cap must refuse the next one");

    drop(held);
    assert_eq!(waiting_for(&counters, 1000), 0, "every slot is released");
    assert!(lock(&counters.held).is_empty(), "and a uid with nobody waiting holds no entry");
    assert!(counters.take(1000).is_some(), "and the next open may wait again");
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
    let counters = waiter_slots();
    let hogged: Vec<UidSlot> = (0..MAX_DAEMON_WAITERS)
        .map(|_| counters.take(1000).expect("under the cap"))
        .collect();
    assert!(counters.take(1000).is_none(), "that uid has spent its budget");

    let victim = counters.take(1001);
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
    let counters = waiter_slots();
    let mut held = Vec::new();
    for uid in 1000..1005 {
        for _ in 0..MAX_DAEMON_WAITERS {
            if let Some(slot) = counters.take(uid) {
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
        counters.take(1005).is_none(),
        "a uid with an entirely unspent budget must still be refused once the \
         machine-wide backstop is full"
    );

    drop(held);
    assert!(lock(&counters.held).is_empty(), "every slot is released");
    assert!(counters.take(1005).is_some(), "and the backstop frees up again");
}

/// A slot is released however its holder leaves, including by panicking —
/// otherwise one panicking worker would permanently shrink the number of
/// opens that may ever wait.
#[test]
fn a_waiter_slot_is_released_even_if_its_holder_panics() {
    let counters = waiter_slots();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _slot = counters.take(1000).expect("under the cap");
        panic!("as a worker might");
    }));
    assert!(outcome.is_err());
    assert_eq!(waiting_for(&counters, 1000), 0);
}
