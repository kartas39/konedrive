use super::*;

fn item(number: i64) -> LocalItemId {
    LocalItemId::new(number)
}

fn backing_file() -> Arc<File> {
    Arc::new(tempfile::tempfile().unwrap())
}

fn content(ctag: &str) -> ContentVersion {
    ContentVersion::Hashed { size: 10, mtime: 1_700_000_000, mtime_nsec: 0, hash: None, ctag: Some(ctag.into()) }
}

#[test]
fn the_first_writer_of_an_item_is_told_so_and_a_second_is_not() {
    let handles = ItemHandles::new();
    assert!(!handles.is_open_for_writing(item(5)));
    let (first, first_rank) = handles.add_writer(item(5), backing_file());
    let (second, second_rank) = handles.add_writer(item(5), backing_file());
    let (other, other_rank) = handles.add_writer(item(6), backing_file());
    assert_eq!((first_rank, second_rank, other_rank), (WriterRank::First, WriterRank::Later, WriterRank::First));
    assert!(first != second && second != other && [first, second, other].iter().all(|fh| *fh != Fh::NOT_OPENED), "each open has an fh of its own");
    assert!(handles.is_open_for_writing(item(5)) && handles.is_open_for_writing(item(6)) && !handles.is_open_for_writing(item(7)));
    // A reader is no writer.
    handles.add_file(OpenItemFile::ReadingBackingFile { item: item(7), backing_file: backing_file() });
    assert!(!handles.is_open_for_writing(item(7)));
    // Once the first is gone, the next to open is a first again.
    handles.remove_writer(first);
    assert_eq!(handles.add_writer(item(5), backing_file()).1, WriterRank::Later);
    handles.remove_writer(other);
    assert_eq!(handles.add_writer(item(6), backing_file()).1, WriterRank::First);
}

#[test]
fn the_backing_file_of_an_items_writer_is_found_by_the_item() {
    let handles = ItemHandles::new();
    let file = backing_file();
    let (fh, _) = handles.add_writer(item(5), Arc::clone(&file));
    assert!(Arc::ptr_eq(&handles.find_writer_file(item(5)).unwrap(), &file));
    let (written_item, written_file) = handles.find_file_to_write(fh).unwrap();
    assert!(written_item == item(5) && Arc::ptr_eq(&written_file, &file));
    assert!(matches!(handles.find_file(fh), Some(OpenItemFile::Writing { item: found, .. }) if found == item(5)));
}

#[test]
fn an_item_with_no_writer_has_no_writers_file() {
    let handles = ItemHandles::new();
    assert!(handles.find_writer_file(item(5)).is_none());
    let reader = handles.add_file(OpenItemFile::ReadingBackingFile { item: item(5), backing_file: backing_file() });
    assert!(handles.find_writer_file(item(5)).is_none(), "a reader's file is not a writer's");
    assert!(handles.find_file_to_write(reader).is_none());
    handles.add_writer(item(6), backing_file());
    assert!(handles.find_writer_file(item(5)).is_none(), "nor is another item's writer");
    let directory = handles.add_directory();
    assert!(handles.find_file(directory).is_none() && handles.find_file_to_write(directory).is_none());
    assert!(handles.find_file(Fh(999)).is_none());
}

#[test]
fn the_last_close_says_whether_anything_was_written_and_the_writer_stays_until_it_is_removed() {
    let handles = ItemHandles::new();
    let (first, _) = handles.add_writer(item(5), backing_file());
    let (second, _) = handles.add_writer(item(5), backing_file());
    assert_eq!(handles.find_closing_writer(first).unwrap().outcome, CloseOutcome::StillOpenElsewhere);
    handles.remove_writer(first);
    assert!(handles.is_open_for_writing(item(5)));
    // The last one, with nothing written.
    let closing = handles.find_closing_writer(second).unwrap();
    assert_eq!((closing.item, closing.outcome), (item(5), CloseOutcome::ContentUntouched));
    assert!(handles.is_open_for_writing(item(5)), "nothing is removed by the question");
    handles.remove_writer(second);
    assert!(!handles.is_open_for_writing(item(5)));
    assert!(handles.find_closing_writer(second).is_none() && handles.find_file(second).is_none());
    // Written through one handle: the last close says so, whichever handle it is.
    let (first, _) = handles.add_writer(item(5), backing_file());
    let (second, _) = handles.add_writer(item(5), backing_file());
    handles.mark_written(item(5));
    handles.remove_writer(first);
    assert_eq!(handles.find_closing_writer(second).unwrap().outcome, CloseOutcome::ContentChanged);
    assert_eq!(handles.find_closing_writer(second).unwrap().outcome, CloseOutcome::ContentChanged, "asked again, the same");
    handles.remove_writer(second);
    // What was written is forgotten with the last writer.
    let (again, _) = handles.add_writer(item(5), backing_file());
    assert_eq!(handles.find_closing_writer(again).unwrap().outcome, CloseOutcome::ContentUntouched);
    handles.remove_writer(again);
    // Nothing is marked for an item no handle writes.
    handles.mark_written(item(5));
    let (later, _) = handles.add_writer(item(5), backing_file());
    assert_eq!(handles.find_closing_writer(later).unwrap().outcome, CloseOutcome::ContentUntouched);
    // A reader and a directory are no closing writers, and are not removed as one.
    let reader = handles.add_file(OpenItemFile::ReadingBackingFile { item: item(5), backing_file: backing_file() });
    let directory = handles.add_directory();
    assert!(handles.find_closing_writer(reader).is_none() && handles.find_closing_writer(directory).is_none());
    handles.remove_writer(reader);
    handles.remove_writer(directory);
    assert!(handles.find_file(reader).is_some());
    handles.keep_listed_items(directory, Arc::new(Vec::new()));
    assert!(handles.find_listed_items(directory).is_some());
}

/// Getattr never finds a file with neither a writer nor its new size in the row.
#[test]
fn a_writer_that_is_closing_still_gives_its_file_for_the_size() {
    let handles = ItemHandles::new();
    let file = backing_file();
    let (fh, _) = handles.add_writer(item(5), Arc::clone(&file));
    let closing = handles.find_closing_writer(fh).unwrap();
    assert!(Arc::ptr_eq(&closing.backing_file, &file), "the close reads the size and the times from it");
    assert!(Arc::ptr_eq(&handles.find_writer_file(item(5)).unwrap(), &file), "and so does getattr, until the row is written");
    handles.remove_writer(fh);
    assert!(handles.find_writer_file(item(5)).is_none());
}

#[test]
fn a_reader_not_on_disk_keeps_the_content_it_was_opened_as() {
    let handles = ItemHandles::new();
    let fh = handles.add_file(OpenItemFile::ReadingNotOnDisk { item: item(5), content: content("c1") });
    let other = handles.add_file(OpenItemFile::ReadingNotOnDisk { item: item(5), content: content("c2") });
    assert!(matches!(handles.find_file(fh), Some(OpenItemFile::ReadingNotOnDisk { item: found, content: opened_as }) if found == item(5) && opened_as == content("c1")));
    assert!(matches!(handles.find_file(other), Some(OpenItemFile::ReadingNotOnDisk { content: opened_as, .. }) if opened_as == content("c2")));
    handles.close_reader(fh);
    assert!(handles.find_file(fh).is_none() && handles.find_file(other).is_some());
    // A writer and a directory are not closed as a reader.
    let (writer, _) = handles.add_writer(item(6), backing_file());
    let directory = handles.add_directory();
    handles.close_reader(writer);
    handles.close_reader(directory);
    assert!(handles.is_open_for_writing(item(6)));
    handles.keep_listed_items(directory, Arc::new(Vec::new()));
    assert!(handles.find_listed_items(directory).is_some());
}

#[test]
fn a_reader_that_was_not_on_disk_goes_on_from_the_backing_file_once_it_landed() {
    let handles = ItemHandles::new();
    let fh = handles.add_file(OpenItemFile::ReadingNotOnDisk { item: item(5), content: content("c1") });
    let file = backing_file();
    handles.switch_to_backing_file(fh, Arc::clone(&file));
    assert!(matches!(handles.find_file(fh), Some(OpenItemFile::ReadingBackingFile { item: found, backing_file }) if found == item(5) && Arc::ptr_eq(&backing_file, &file)));
    assert!(!handles.is_open_for_writing(item(5)));
    // Only such a reader is switched.
    let (writer, _) = handles.add_writer(item(6), Arc::clone(&file));
    let other = backing_file();
    handles.switch_to_backing_file(writer, Arc::clone(&other));
    handles.switch_to_backing_file(fh, Arc::clone(&other));
    assert!(Arc::ptr_eq(&handles.find_writer_file(item(6)).unwrap(), &file));
    assert!(matches!(handles.find_file(fh), Some(OpenItemFile::ReadingBackingFile { backing_file, .. }) if Arc::ptr_eq(&backing_file, &file)));
}

#[test]
fn a_directory_read_in_parts_reads_the_items_it_listed_first() {
    let handles = ItemHandles::new();
    let fh = handles.add_directory();
    let other = handles.add_directory();
    assert!(handles.find_listed_items(fh).is_none(), "not read yet");
    let listed = Arc::new(Vec::new());
    handles.keep_listed_items(fh, Arc::clone(&listed));
    assert!(Arc::ptr_eq(&handles.find_listed_items(fh).unwrap(), &listed));
    assert!(handles.find_listed_items(other).is_none());
    // A file's fh holds no listing, and is not closed as a directory.
    let (writer, _) = handles.add_writer(item(5), backing_file());
    handles.keep_listed_items(writer, Arc::clone(&listed));
    assert!(handles.find_listed_items(writer).is_none());
    handles.close_directory(writer);
    assert!(handles.is_open_for_writing(item(5)));
    handles.close_directory(fh);
    assert!(handles.find_listed_items(fh).is_none());
    handles.keep_listed_items(fh, listed);
    assert!(handles.find_listed_items(fh).is_none(), "closed: nothing is kept for it");
}
