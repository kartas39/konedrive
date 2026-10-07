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

/// The helper's pid in these tests, and an opener's.
const OWN: i32 = 100;
const OTHER: i32 = 200;

/// Opens read from the kernel's queue, of a process that is not the helper.
fn others(opens: Vec<u32>) -> Vec<(u32, i32)> {
    opens.into_iter().map(|open| (open, OTHER)).collect()
}

/// For a stop that has no open of the helper's own to allow.
fn not_allowed(open: u32) {
    panic!("open {open} was allowed");
}

/// A stop that answered `answered` opens and left nothing.
fn clean(answered: usize) -> Stopped {
    Stopped { answered, left: 0, queue: QueueEnd::Empty, panicked: false }
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
        OWN,
        || std::mem::take(&mut table),
        || {
            let Some(batch) = queue.pop() else {
                return Queue::Empty;
            };
            unanswered.fetch_add(batch.len(), Ordering::SeqCst);
            Queue::Read(others(batch))
        },
        |open, errno| {
            answers.record(open, errno);
            unanswered.fetch_sub(1, Ordering::SeqCst);
        },
        not_allowed,
        || unanswered.load(Ordering::SeqCst),
    );

    assert_eq!(stopped, clean(8));
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
        OWN,
        || std::mem::take(&mut table),
        || {
            reads += 1;
            // Empty at the first read; open 3 is there at the second.
            if reads != 2 {
                return Queue::Empty;
            }
            unanswered.fetch_add(1, Ordering::SeqCst);
            Queue::Read(others(vec![3]))
        },
        |open, errno| {
            answers.record(open, errno);
            unanswered.fetch_sub(1, Ordering::SeqCst);
        },
        not_allowed,
        || unanswered.load(Ordering::SeqCst),
    );
    worker.join().unwrap();

    assert_eq!(stopped, clean(3), "the worker's open is counted");
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
        OWN,
        || std::mem::take(&mut table),
        || Queue::Empty,
        |open, errno| {
            answers.record(open, errno);
            unanswered.fetch_sub(1, Ordering::SeqCst);
        },
        not_allowed,
        || unanswered.load(Ordering::SeqCst),
    );
    let took = started.elapsed();

    assert_eq!(
        stopped,
        Stopped { answered: 1, left: 2, queue: QueueEnd::Empty, panicked: false }
    );
    assert!(!stopped.clean());
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
        OWN,
        Vec::new,
        || {
            next += 1;
            unanswered.fetch_add(1, Ordering::SeqCst);
            Queue::Read(others(vec![next]))
        },
        |_open: u32, _errno| {
            unanswered.fetch_sub(1, Ordering::SeqCst);
        },
        not_allowed,
        || unanswered.load(Ordering::SeqCst),
    );

    assert_eq!(stopped.left, 0, "nothing is held");
    assert_eq!(stopped.queue, QueueEnd::NeverEmptied, "but no read found the queue empty");
    assert!(!stopped.clean());
    assert!(stopped.answered > 0);
}

/// `EAGAIN` is also what a read returns when the open at the head of the
/// queue is of a leased file, with more opens queued behind it. While the
/// group says it has something to read, the stop reads on, and reaches them.
#[test]
fn an_eagain_with_the_queue_still_readable_does_not_end_the_stop() {
    let answers = Answers::default();
    let unanswered = AtomicUsize::new(0);
    // The kernel's queue: a leased file's open, which the read gives up as
    // `EAGAIN`, then two opens it hands over.
    let queue = RefCell::new(vec![Ok(vec![(1, OTHER), (2, OTHER)]), Err(nix::errno::Errno::EAGAIN)]);

    let stopped = deny_held(
        Duration::from_secs(5),
        OWN,
        Vec::new,
        || {
            let read = read_queue(
                || queue.borrow_mut().pop().unwrap_or(Err(nix::errno::Errno::EAGAIN)),
                || !queue.borrow().is_empty(),
            );
            if let Queue::Read(batch) = &read {
                unanswered.fetch_add(batch.len(), Ordering::SeqCst);
            }
            read
        },
        |open, errno| {
            answers.record(open, errno);
            unanswered.fetch_sub(1, Ordering::SeqCst);
        },
        not_allowed,
        || unanswered.load(Ordering::SeqCst),
    );

    assert_eq!(stopped, clean(2));
    assert_eq!(answers.eio_once(), [1, 2], "the opens behind the leased one are answered");
}

/// A read of the group that fails for good is not an empty queue: the stop
/// still answers what the helper holds, and does not end clean.
#[test]
fn a_read_that_fails_for_good_is_not_an_empty_queue() {
    let answers = Answers::default();
    let unanswered = AtomicUsize::new(1);
    let mut table = vec![1];
    let reads = AtomicUsize::new(0);

    let stopped = deny_held(
        Duration::from_secs(5),
        OWN,
        || std::mem::take(&mut table),
        || {
            reads.fetch_add(1, Ordering::SeqCst);
            read_queue(|| Err::<Vec<(u32, i32)>, _>(nix::errno::Errno::EBADF), || true)
        },
        |open, errno| {
            answers.record(open, errno);
            unanswered.fetch_sub(1, Ordering::SeqCst);
        },
        not_allowed,
        || unanswered.load(Ordering::SeqCst),
    );

    assert_eq!(
        stopped,
        Stopped {
            answered: 1,
            left: 0,
            queue: QueueEnd::Unreadable(nix::errno::Errno::EBADF),
            panicked: false
        }
    );
    assert!(!stopped.clean(), "the helper exits with status 1");
    assert_eq!(reads.load(Ordering::SeqCst), 1, "a group that cannot be read is not read again");
    assert_eq!(answers.eio_once(), [1]);
    assert!(stopped.line(Duration::ZERO).contains("could not be read"));
}

/// The line says which it was: opens left without an answer, or a queue
/// that never emptied with nothing left, or both.
#[test]
fn the_line_tells_opens_left_from_a_queue_that_never_emptied() {
    let took = Duration::from_millis(7);
    let stopped = |left, queue| Stopped { answered: 4, left, queue, panicked: false };

    let finished = stopped(0, QueueEnd::Empty).line(took);
    assert!(finished.ends_with("none is left"), "{finished}");

    let opens_left = stopped(2, QueueEnd::Empty).line(took);
    assert!(opens_left.contains("2 more could not be"), "{opens_left}");
    assert!(!opens_left.contains("never emptied"), "{opens_left}");

    let never_emptied = stopped(0, QueueEnd::NeverEmptied).line(took);
    assert!(never_emptied.contains("never emptied"), "{never_emptied}");
    assert!(never_emptied.contains("none that it had read is left"), "{never_emptied}");
    assert!(!never_emptied.contains("more could not be"), "{never_emptied}");

    let both = stopped(2, QueueEnd::NeverEmptied).line(took);
    assert!(both.contains("2 more could not be") && both.contains("never emptied"), "{both}");
}

/// An open of the helper's own that the stop reads is allowed, as the event
/// loop allows it; the opens read with it are denied.
#[test]
fn an_open_of_the_helpers_own_is_allowed() {
    let answers = Answers::default();
    let allowed = RefCell::new(Vec::new());
    let unanswered = AtomicUsize::new(0);
    let mut queue = vec![vec![(1, OTHER), (2, OWN), (3, OTHER)]];

    let stopped = deny_held(
        Duration::from_secs(5),
        OWN,
        Vec::new,
        || {
            let Some(batch) = queue.pop() else {
                return Queue::Empty;
            };
            unanswered.fetch_add(batch.len(), Ordering::SeqCst);
            Queue::Read(batch)
        },
        |open, errno| {
            answers.record(open, errno);
            unanswered.fetch_sub(1, Ordering::SeqCst);
        },
        |open| {
            allowed.borrow_mut().push(open);
            unanswered.fetch_sub(1, Ordering::SeqCst);
        },
        || unanswered.load(Ordering::SeqCst),
    );

    assert_eq!(stopped, clean(3));
    assert_eq!(*allowed.borrow(), [2]);
    assert_eq!(answers.eio_once(), [1, 3]);
}

/// A held open as `PendingOpen` is one: dropped with no answer, it is denied
/// `EIO`, and it is counted until it is gone.
struct Held<'a> {
    open: u32,
    answered: bool,
    answers: &'a Answers,
    unanswered: &'a AtomicUsize,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        if !self.answered {
            self.answers.record(self.open, Errno::EIO);
        }
        self.unanswered.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A panic inside the stop does not unwind out of it: the open in hand is
/// denied by its drop, the stop is run again and answers the rest, and it
/// does not end clean.
#[test]
fn a_panic_inside_the_stop_still_answers_what_is_held() {
    let answers = Answers::default();
    let unanswered = AtomicUsize::new(3);
    let held = |open| Held { open, answered: false, answers: &answers, unanswered: &unanswered };
    // Two wait in the table; the third is in the kernel's queue.
    let table = RefCell::new(vec![held(1), held(2)]);
    let queue = RefCell::new(vec![vec![(held(3), OTHER)]]);
    let panicked = AtomicUsize::new(0);

    let stopped = contain(
        || {
            deny_held(
                Duration::from_secs(5),
                OWN,
                // One at a time, so that the panic leaves one in the table.
                || table.borrow_mut().pop().into_iter().collect(),
                || queue.borrow_mut().pop().map_or(Queue::Empty, Queue::Read),
                |mut open: Held, errno| {
                    if panicked.fetch_add(1, Ordering::SeqCst) == 0 {
                        panic!("deliberate: while answering the first open");
                    }
                    open.answered = true;
                    answers.record(open.open, errno);
                },
                |open: Held| panic!("open {} was allowed", open.open),
                || unanswered.load(Ordering::SeqCst),
            )
        },
        || unanswered.load(Ordering::SeqCst),
    );

    assert!(stopped.panicked);
    assert_eq!(stopped.left, 0);
    assert_eq!(stopped.queue, QueueEnd::Empty);
    assert!(!stopped.clean(), "the helper exits with status 1");
    assert_eq!(answers.eio_once(), [1, 2, 3]);
    assert!(stopped.line(Duration::ZERO).contains("panicked"));
}

/// A stop that panics again when it is run again ends there, and says how
/// many opens are left.
#[test]
fn a_stop_that_panics_twice_ends_and_says_what_is_left() {
    let stopped = contain(|| panic!("deliberate: every run of the stop"), || 4);
    assert_eq!(
        stopped,
        Stopped { answered: 0, left: 4, queue: QueueEnd::NeverEmptied, panicked: true }
    );
    assert!(!stopped.clean());
}
