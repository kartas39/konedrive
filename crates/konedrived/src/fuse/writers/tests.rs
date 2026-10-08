use super::{Closed, Writers};

/// A file goes up once: when the last of the handles that write it is closed.
#[test]
fn only_the_last_writer_to_close_leaves_the_file_ready() {
    let writers = Writers::default();
    assert!(writers.opened(7, false));
    assert!(!writers.opened(7, false));
    writers.written(7);
    assert_eq!(writers.closed(7), Closed::StillOpen);
    assert!(writers.open(7));
    assert_eq!(writers.closed(7), Closed::Changed);
    assert!(!writers.open(7));
}

/// A program that opens a file for writing and writes nothing has not changed it.
#[test]
fn a_file_closed_without_a_write_is_untouched() {
    let writers = Writers::default();
    writers.opened(7, false);
    assert_eq!(writers.closed(7), Closed::Untouched);
}

/// A file whose content was the user's own before it was opened is sent whatever this
/// writer did: cut to nothing at the open, or changed earlier and not sent yet.
#[test]
fn a_file_that_was_changed_before_stays_changed() {
    let writers = Writers::default();
    writers.opened(7, true);
    assert_eq!(writers.closed(7), Closed::Changed);
    // The next writer starts anew.
    writers.opened(7, false);
    assert_eq!(writers.closed(7), Closed::Untouched);
}

/// What is written through a handle the mount never opened for writing changes nothing.
#[test]
fn a_write_to_a_file_nobody_opened_is_not_counted() {
    let writers = Writers::default();
    writers.written(7);
    assert_eq!(writers.closed(7), Closed::StillOpen);
    assert!(!writers.open(7));
}
