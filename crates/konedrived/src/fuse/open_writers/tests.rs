use super::{CloseOutcome, OpenWriters};

/// A file goes up once: when the last of the handles that write it is closed.
#[test]
fn only_the_last_writer_to_close_leaves_the_content_changed() {
    let open_writers = OpenWriters::default();
    assert!(open_writers.add_writer(7, false));
    assert!(!open_writers.add_writer(7, false));
    open_writers.mark_written(7);
    assert_eq!(open_writers.remove_writer(7), CloseOutcome::StillOpenElsewhere);
    assert!(open_writers.is_open(7));
    assert_eq!(open_writers.remove_writer(7), CloseOutcome::ContentChanged);
    assert!(!open_writers.is_open(7));
}

/// A program that opens a file for writing and writes nothing has not changed it.
#[test]
fn a_file_closed_without_a_write_has_its_content_untouched() {
    let open_writers = OpenWriters::default();
    open_writers.add_writer(7, false);
    assert_eq!(open_writers.remove_writer(7), CloseOutcome::ContentUntouched);
}

/// A file whose content was the user's own before it was opened is sent whatever this
/// writer did: cut to nothing at the open, or modified here earlier and not sent yet.
#[test]
fn a_file_whose_content_was_changed_before_stays_changed() {
    let open_writers = OpenWriters::default();
    open_writers.add_writer(7, true);
    assert_eq!(open_writers.remove_writer(7), CloseOutcome::ContentChanged);
    // The next writer starts anew.
    open_writers.add_writer(7, false);
    assert_eq!(open_writers.remove_writer(7), CloseOutcome::ContentUntouched);
}

/// What is written through a handle the mount never opened for writing changes nothing.
#[test]
fn a_write_to_a_file_nobody_opened_is_not_counted() {
    let open_writers = OpenWriters::default();
    open_writers.mark_written(7);
    assert_eq!(open_writers.remove_writer(7), CloseOutcome::StillOpenElsewhere);
    assert!(!open_writers.is_open(7));
}
