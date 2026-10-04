use super::*;

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
