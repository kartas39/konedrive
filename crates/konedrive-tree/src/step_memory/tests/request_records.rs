//! The requests on record: written before a request leaves, read by the plan and by the
//! one that settles them, and gone with their item's row.

use super::*;

fn record(item: LocalItemId, onedrive_item: Option<&str>, request: RecordedRequest, sent_at: i64) -> RequestRecord {
    RequestRecord { item, onedrive_item: onedrive_item.map(reference), request, sent_at }
}

/// Every kind of request is read back as it was written, a name that is not UTF-8 among
/// them, the one sent first first.
#[test]
fn each_kind_of_request_is_read_back_as_written_in_the_order_sent() {
    use std::os::unix::ffi::OsStringExt;
    let mut store = TreeStore::in_memory().unwrap();
    let items: Vec<LocalItemId> = (0..6).map(|number| make_file(&mut store, ROOT, &format!("f{number}"))).collect();
    let odd_name = std::ffi::OsString::from_vec(vec![b'a', 0xff, b'b']);
    let records = vec![
        record(items[0], None, RecordedRequest::MakeDirectory { parent: reference("D"), name: "new".into() }, 10),
        record(items[1], None, RecordedRequest::SendNewFile { parent: reference("D"), name: odd_name, sent_hash: "h".into() }, 11),
        record(items[2], Some("C"), RecordedRequest::SendContent { sent_hash: "h2".into() }, 12),
        record(items[3], Some("M"), RecordedRequest::SendMove { parent: reference("E"), name: "moved".into() }, 13),
        record(items[4], Some("F"), RecordedRequest::SendFileRemoval, 14),
        record(items[5], Some("G"), RecordedRequest::SendDirectoryRemoval, 15),
    ];
    // Written in another order than they were sent in.
    for written in records.iter().rev() {
        store.step_memory().remember_request_record(written).unwrap();
    }
    assert_eq!(store.read_items(|items| items.list_request_records(10)).unwrap(), records);
    assert_eq!(store.read_items(|items| items.list_request_records(2)).unwrap(), records[..2]);
    let destinations: Vec<Option<OneDriveItemReference>> = records.iter().map(|record| record.request.destination().cloned()).collect();
    assert_eq!(destinations, [Some(reference("D")), Some(reference("D")), None, Some(reference("E")), None, None]);
}

/// One record per item: a step writes its record over the one that was left. A record
/// is refused for an id with no row, and goes with its item's row.
#[test]
fn a_record_is_one_per_item_refused_with_no_row_and_goes_with_the_row() {
    let mut store = TreeStore::in_memory().unwrap();
    let (file, other) = (make_file(&mut store, ROOT, "report"), make_file(&mut store, ROOT, "other"));
    let first = record(file, None, RecordedRequest::SendNewFile { parent: reference("D"), name: "report".into(), sent_hash: "h".into() }, 10);
    let second = record(file, None, RecordedRequest::MakeDirectory { parent: reference("D"), name: "report".into() }, 20);
    let of_other = record(other, Some("O"), RecordedRequest::SendFileRemoval, 15);
    store.step_memory().remember_request_record(&first).unwrap();
    store.step_memory().remember_request_record(&of_other).unwrap();
    store.step_memory().remember_request_record(&second).unwrap();
    assert_eq!(store.read_items(|items| items.list_request_records(10)).unwrap(), [of_other.clone(), second]);

    assert!(is_refused(store.step_memory().remember_request_record(&record(LocalItemId::new(9999), None, RecordedRequest::SendFileRemoval, 1))));

    remove(&mut store, file);
    assert_eq!(store.read_items(|items| items.list_request_records(10)).unwrap(), [of_other]);
    store.step_memory().forget_request_record(other).unwrap();
    store.step_memory().forget_request_record(other).unwrap();
    assert_eq!(count(&store, "request_records"), 0);
}

/// The one that settles a record forgets the record it read and no later one: a record a
/// step wrote since, at another time or of another kind, stays.
#[test]
fn a_record_read_some_time_ago_is_forgotten_only_while_it_is_still_the_items() {
    let mut store = TreeStore::in_memory().unwrap();
    let file = make_file(&mut store, ROOT, "report");
    let read = record(file, Some("A"), RecordedRequest::SendMove { parent: reference("D"), name: "x".into() }, 10);
    store.step_memory().remember_request_record(&read).unwrap();

    let later = record(file, Some("A"), RecordedRequest::SendMove { parent: reference("D"), name: "x".into() }, 11);
    store.step_memory().remember_request_record(&later).unwrap();
    assert!(!store.step_memory().forget_request_record_as_read(&read).unwrap(), "written at another time since");
    let of_another_kind = record(file, Some("A"), RecordedRequest::SendFileRemoval, 10);
    store.step_memory().remember_request_record(&of_another_kind).unwrap();
    assert!(!store.step_memory().forget_request_record_as_read(&read).unwrap(), "another request since");
    assert_eq!(store.read_items(|items| items.list_request_records(10)).unwrap(), [of_another_kind.clone()]);

    assert!(store.step_memory().forget_request_record_as_read(&of_another_kind).unwrap());
    assert!(!store.step_memory().forget_request_record_as_read(&of_another_kind).unwrap(), "gone already");
    assert_eq!(count(&store, "request_records"), 0);
}
