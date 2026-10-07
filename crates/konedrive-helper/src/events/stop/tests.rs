use super::*;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// The opens of a test, by number, and every answer each one got.
#[derive(Default)]
struct Answers(RefCell<BTreeMap<u32, Vec<Errno>>>);

impl Answers {
    fn record(&self, open: u32, errno: Errno) {
        self.0.borrow_mut().entry(open).or_default().push(errno);
    }

    /// The opens that were answered, each exactly once and with `EIO`.
    fn eio_once(&self) -> Vec<u32> {
        let answers = self.0.borrow();
        for (open, got) in answers.iter() {
            assert_eq!(got, &[Errno::EIO], "open {open} must be answered once, with EIO");
        }
        answers.keys().copied().collect()
    }
}

/// Opens waiting for a daemon's answer and opens still in the kernel's queue
/// are each answered once, with `EIO`, and the stop says it finished.
#[test]
fn every_held_open_is_answered_once_with_eio() {
    let answers = Answers::default();
    // Five wait in the table, and are counted from the start; three are in
    // the kernel's queue, in two reads, and are counted as they are read.
    let unanswered = AtomicUsize::new(5);
    let mut table = vec![1, 2, 3, 4, 5];
    let mut queue = vec![vec![8], vec![6, 7]];

    let stopped = deny_held(
        Duration::from_secs(5),
        || std::mem::take(&mut table),
        || {
            let batch = queue.pop()?;
            unanswered.fetch_add(batch.len(), Ordering::SeqCst);
            Some(batch)
        },
        |open, errno| {
            answers.record(open, errno);
            unanswered.fetch_sub(1, Ordering::SeqCst);
        },
        || unanswered.load(Ordering::SeqCst),
    );

    assert_eq!(stopped, Stopped { answered: 8, left: None });
    assert_eq!(answers.eio_once(), [1, 2, 3, 4, 5, 6, 7, 8]);
}

/// An open that arrives while the stop runs is answered too: one a worker
/// still holds when the stop begins, which its thread answers a moment
/// later, and one that reaches the kernel's queue after a read has already
/// found it empty. The stop does not end before both have their answer.
#[test]
fn an_open_that_arrives_during_the_stop_is_answered_too() {
    let answers = Answers::default();
    // Open 1 waits in the table; open 2 is in a worker's hands.
    let unanswered = Arc::new(AtomicUsize::new(2));
    let mut table = vec![1];
    let mut reads = 0;

    let in_hand = Arc::clone(&unanswered);
    let worker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        in_hand.fetch_sub(1, Ordering::SeqCst);
    });

    let stopped = deny_held(
        Duration::from_secs(5),
        || std::mem::take(&mut table),
        || {
            reads += 1;
            // Empty at the first read; open 3 is there at the second.
            if reads != 2 {
                return None;
            }
            unanswered.fetch_add(1, Ordering::SeqCst);
            Some(vec![3])
        },
        |open, errno| {
            answers.record(open, errno);
            unanswered.fetch_sub(1, Ordering::SeqCst);
        },
        || unanswered.load(Ordering::SeqCst),
    );
    worker.join().unwrap();

    assert_eq!(stopped, Stopped { answered: 3, left: None }, "the worker's open is counted");
    assert_eq!(answers.eio_once(), [1, 3]);
}

/// A stop that cannot finish — a thread holds two opens and never answers
/// them — ends at its bound, and says how many it answered and how many were
/// left.
#[test]
fn the_bound_ends_a_stop_that_cannot_finish() {
    let answers = Answers::default();
    let unanswered = AtomicUsize::new(3);
    let mut table = vec![1];
    let bound = Duration::from_millis(100);

    let started = Instant::now();
    let stopped = deny_held(
        bound,
        || std::mem::take(&mut table),
        || None,
        |open, errno| {
            answers.record(open, errno);
            unanswered.fetch_sub(1, Ordering::SeqCst);
        },
        || unanswered.load(Ordering::SeqCst),
    );
    let took = started.elapsed();

    assert_eq!(stopped, Stopped { answered: 1, left: Some(2) });
    assert_eq!(answers.eio_once(), [1]);
    assert!(took >= bound, "it waits out its bound for the opens other threads hold");
    assert!(took < Duration::from_secs(3), "and no longer: took {took:?}");
}

/// A kernel queue that never empties does not keep the stop past its bound
/// either, and the stop does not say it finished.
#[test]
fn the_bound_ends_a_stop_whose_queue_never_empties() {
    let unanswered = AtomicUsize::new(0);
    let mut next = 0;

    let stopped = deny_held(
        Duration::from_millis(100),
        Vec::new,
        || {
            next += 1;
            unanswered.fetch_add(1, Ordering::SeqCst);
            Some(vec![next])
        },
        |_open: u32, _errno| {
            unanswered.fetch_sub(1, Ordering::SeqCst);
        },
        || unanswered.load(Ordering::SeqCst),
    );

    assert_eq!(stopped.left, Some(0), "nothing is held, but no read found the queue empty");
    assert!(stopped.answered > 0);
}
