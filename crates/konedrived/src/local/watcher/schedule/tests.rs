//! The examiner's schedule with the time given by hand: what comes back to be
//! examined, and when.

use std::ffi::OsStr;
use std::path::Path;

use super::*;

/// The design's clocks, shortened to whole seconds that are easy to add up.
fn timing() -> Timing {
    Timing {
        quiet: Duration::from_secs(2),
        ceiling: Duration::from_secs(30),
        recheck: Duration::from_secs(30),
        retry: Duration::from_secs(5),
        degraded_scan: Duration::from_secs(600),
        mark_retry: Duration::from_secs(60),
    }
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// A batch of one name in the folder's root.
fn name(name: &str) -> Batch {
    let mut batch = Batch::new();
    batch.name(Path::new(""), OsStr::new(name));
    batch
}

fn both(a: &str, b: &str) -> Batch {
    let mut batch = name(a);
    batch.merge(name(b));
    batch
}

/// Examined, with nothing to see again.
fn fine() -> Handled {
    Handled::Done { recheck: Batch::new(), passed: Box::default() }
}

/// Examined, with `passed` passed over.
fn passing_over(passed: &str) -> Handled {
    Handled::Done { recheck: Batch::new(), passed: Box::new(name(passed)) }
}

fn failed() -> Handled {
    Handled::Failed("the store is closed".into())
}

/// What is taken at `now`, when nothing hurries it.
fn taken(schedule: &mut Schedule, now: Instant) -> Option<Batch> {
    schedule.take(now, Take::WhenDue).cloned()
}

/// An entry that keeps being passed over has one recheck pending, however
/// many runs passed it over, and its wait doubles up to the longest; a recheck
/// that passes nothing over starts the waits again.
#[test]
fn a_passed_over_entry_has_one_recheck_that_backs_off() {
    let t0 = Instant::now();
    let mut schedule = Schedule::new(Timing { degraded_scan: secs(12), ..timing() });
    schedule.add(Batch::scan(ScanReason::Start));
    assert!(taken(&mut schedule, t0).is_some_and(|batch| batch.is_full()));
    assert_eq!(schedule.done(passing_over("stuck.txt"), t0), Outcome::Examined);
    assert_eq!(schedule.wake_at(), Some(t0 + secs(5)), "the first recheck, one retry on");

    // Two more runs beside the pending recheck: they join it, and it keeps its time.
    for (n, file) in ["one.txt", "two.txt"].into_iter().enumerate() {
        let now = t0 + secs(1 + n as u64);
        schedule.add(name(file));
        assert_eq!(taken(&mut schedule, now), Some(name(file)));
        schedule.done(passing_over("stuck.txt"), now);
        assert_eq!(schedule.wake_at(), Some(t0 + secs(5)));
    }

    // Each recheck that passes it over again waits twice as long, up to the longest wait.
    let mut at = t0 + secs(5);
    for wait in [10, 12, 12] {
        assert_eq!(taken(&mut schedule, at - secs(1)), None, "not before its time");
        assert_eq!(taken(&mut schedule, at), Some(name("stuck.txt")), "one recheck, of the entry alone");
        schedule.done(passing_over("stuck.txt"), at);
        at += secs(wait);
        assert_eq!(schedule.wake_at(), Some(at));
    }

    // A recheck that passes nothing over ends it; the next entry passed over starts at one retry.
    assert_eq!(taken(&mut schedule, at), Some(name("stuck.txt")));
    schedule.done(fine(), at);
    assert_eq!(schedule.wake_at(), None, "nothing waits");
    schedule.add(name("other.txt"));
    assert_eq!(taken(&mut schedule, at), Some(name("other.txt")));
    schedule.done(passing_over("other.txt"), at);
    assert_eq!(schedule.wake_at(), Some(at + secs(5)));
}

/// A batch that failed is offered again with what came meanwhile, after a wait
/// that doubles at each failure in a row; the third failure in a row is said;
/// a flush does not wait.
#[test]
fn a_failed_batch_backs_off_with_what_came_meanwhile_and_a_flush_cuts_the_wait() {
    let t0 = Instant::now();
    let mut schedule = Schedule::new(timing());
    schedule.add(name("a.txt"));
    assert_eq!(taken(&mut schedule, t0), Some(name("a.txt")));
    assert_eq!(schedule.done(failed(), t0), Outcome::Failed { why: "the store is closed".into(), wait: secs(5), said: false });

    schedule.add(name("b.txt"));
    assert_eq!(taken(&mut schedule, t0 + secs(4)), None, "what came meanwhile waits with the batch that failed");
    assert_eq!(schedule.wake_at(), Some(t0 + secs(5)));
    assert_eq!(taken(&mut schedule, t0 + secs(5)), Some(both("a.txt", "b.txt")));
    assert_eq!(schedule.done(failed(), t0 + secs(5)), Outcome::Failed { why: "the store is closed".into(), wait: secs(10), said: false });
    assert_eq!(schedule.wake_at(), Some(t0 + secs(15)));

    // A flush takes it now, and the third failure in a row is one to say.
    let now = t0 + secs(6);
    assert_eq!(schedule.take(now, Take::Now).cloned(), Some(both("a.txt", "b.txt")));
    assert_eq!(schedule.done(failed(), now), Outcome::Failed { why: "the store is closed".into(), wait: secs(20), said: true });
    assert_eq!(taken(&mut schedule, now + secs(19)), None);

    // One that passes ends the waits: the next failure waits one retry again.
    assert_eq!(taken(&mut schedule, now + secs(20)), Some(both("a.txt", "b.txt")));
    assert_eq!(schedule.done(fine(), now + secs(20)), Outcome::Examined);
    assert_eq!((taken(&mut schedule, now + secs(3600)), schedule.wake_at()), (None, None), "nothing is left");
    schedule.add(name("c.txt"));
    assert!(taken(&mut schedule, now + secs(21)).is_some());
    assert_eq!(schedule.done(failed(), now + secs(21)), Outcome::Failed { why: "the store is closed".into(), wait: secs(5), said: false });
}

/// A folder with no completed listing yet is not a failing one: its batch is
/// offered again one retry on, however often, and the failures before it are
/// not counted on.
#[test]
fn a_batch_that_cannot_be_examined_yet_is_not_a_failure() {
    let t0 = Instant::now();
    let mut schedule = Schedule::new(timing());
    schedule.add(Batch::scan(ScanReason::Start));
    for n in 0..2 {
        assert!(schedule.take(t0, Take::Now).is_some());
        assert!(matches!(schedule.done(failed(), t0), Outcome::Failed { said: false, .. }), "failure {n}");
    }
    let mut now = t0;
    for _ in 0..4 {
        assert!(schedule.take(now, Take::Now).is_some());
        assert_eq!(schedule.done(Handled::NotYet, now), Outcome::NotYet);
        assert_eq!(taken(&mut schedule, now + secs(4)), None);
        now += secs(5);
        assert_eq!(schedule.wake_at(), Some(now), "one retry on, without doubling");
    }
    let batch = taken(&mut schedule, now).expect("the batch is kept until it can be examined");
    assert_eq!(batch.reason(), Some(ScanReason::Start), "the Full scan it was, and why");
    assert_eq!(schedule.done(failed(), now), Outcome::Failed { why: "the store is closed".into(), wait: secs(5), said: false });
}

/// What the sink asked to see again comes back one recheck on; and while part
/// of the folder cannot be watched, a Full local scan comes on the beat with
/// nothing handed over at all.
#[test]
fn a_recheck_comes_back_and_a_folder_watched_in_part_is_scanned_on_the_beat() {
    let t0 = Instant::now();
    let mut schedule = Schedule::new(timing());
    schedule.add(name("busy.txt"));
    assert!(taken(&mut schedule, t0).is_some());
    schedule.done(Handled::Done { recheck: name("busy.txt"), passed: Box::default() }, t0 + secs(1));
    assert_eq!(schedule.wake_at(), Some(t0 + secs(31)), "counted from the end of the examination");
    assert_eq!(taken(&mut schedule, t0 + secs(30)), None);
    assert_eq!(taken(&mut schedule, t0 + secs(31)), Some(name("busy.txt")));
    schedule.done(fine(), t0 + secs(31));
    assert_eq!(schedule.wake_at(), None);

    // The folder is found to be watched only in part, and stays so.
    schedule.degrade(t0 + secs(40));
    schedule.degrade(t0 + secs(50));
    assert_eq!(schedule.wake_at(), Some(t0 + secs(640)), "from when it was first found so");
    assert_eq!(taken(&mut schedule, t0 + secs(639)), None);
    let scan = taken(&mut schedule, t0 + secs(640)).expect("the periodic scan");
    assert_eq!((scan.is_full(), scan.reason()), (true, Some(ScanReason::Periodic)));
    schedule.done(fine(), t0 + secs(650));
    assert_eq!(schedule.wake_at(), Some(t0 + secs(1240)), "the beat is kept, however long a scan takes");
}
