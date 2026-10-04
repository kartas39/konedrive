//! The reader's timers with the time given by hand.

use super::*;

fn timers() -> Timers {
    Timers::new(&Timing { degraded_scan: Duration::from_secs(600), mark_retry: Duration::from_secs(60), ..Timing::default() })
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

const NOTHING: Due = Due { walk: false, retry: false };
const WALK: Due = Due { walk: true, retry: false };

/// A directory the map does not know costs a walk at once the first time, and
/// then at most one walk a minute however many events it raises; a lost event
/// costs one as soon as the queue is drained, whenever the last walk was.
#[test]
fn a_walk_for_an_unknown_directory_comes_at_most_once_a_minute() {
    let t0 = Instant::now();
    let mut timers = timers();
    assert_eq!((timers.next_wake(), timers.due(t0)), (None, NOTHING));

    // The bring-up walk was a second ago: the first unknown directory waits out the minute.
    timers.walking(t0);
    timers.walk_soon(t0 + secs(1));
    timers.walk_soon(t0 + secs(30));
    assert_eq!(timers.next_wake(), Some(t0 + UNKNOWN_WALK));
    assert_eq!(timers.due(t0 + secs(59)), NOTHING);
    assert_eq!(timers.due(t0 + secs(60)), WALK);
    timers.walking(t0 + secs(60));
    assert_eq!((timers.next_wake(), timers.due(t0 + secs(61))), (None, NOTHING), "one walk for all of them");

    // Long after the last walk, it is walked at once.
    timers.walk_soon(t0 + secs(500));
    assert_eq!(timers.due(t0 + secs(500)), WALK);
    timers.walking(t0 + secs(500));

    // A lost event does not wait for the minute, and is wanted until a walk begins.
    timers.lost();
    assert!(timers.walk_wanted());
    assert_eq!(timers.due(t0 + secs(501)), WALK);
    assert_eq!(timers.due(t0 + secs(501)), WALK);
    timers.walking(t0 + secs(501));
    assert_eq!((timers.walk_wanted(), timers.due(t0 + secs(502))), (false, NOTHING));
}

/// While part of the folder cannot be watched it is walked on the beat; and
/// the helper is asked again a while after it did not mark a directory, and
/// no more once every directory is marked.
#[test]
fn the_degraded_walk_and_the_mark_retry_run_on_their_beats() {
    let t0 = Instant::now();
    let mut timers = timers();
    timers.degrade(t0);
    timers.degrade(t0 + secs(100));
    assert_eq!(timers.next_wake(), Some(t0 + secs(600)), "from when it was first found so");
    assert_eq!(timers.due(t0 + secs(600)), WALK);
    assert_eq!(timers.next_wake(), None);
    timers.degrade(t0 + secs(601));
    assert_eq!(timers.next_wake(), Some(t0 + secs(1201)), "armed again by the reader's next turn");

    timers.uncovered(t0 + secs(700), 2);
    timers.uncovered(t0 + secs(710), 2);
    assert_eq!(timers.next_wake(), Some(t0 + secs(760)), "the retry, before the walk");
    assert_eq!(timers.due(t0 + secs(760)), Due { walk: false, retry: true });
    assert_eq!(timers.due(t0 + secs(761)), NOTHING, "asked once");
    // Still not marked: asked again a retry on. Marked: nothing more to ask.
    timers.uncovered(t0 + secs(761), 1);
    assert_eq!(timers.next_wake(), Some(t0 + secs(821)));
    timers.uncovered(t0 + secs(770), 0);
    assert_eq!(timers.next_wake(), Some(t0 + secs(1201)), "only the degraded walk is left");
}
