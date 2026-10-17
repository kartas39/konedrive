//! The pure pieces of the kernel's interface and the reads it keeps for the kernel's
//! interrupt and the mount's stop. The requests themselves are tested under it
//! (`item_kernel_requests/tests.rs`); the session is the VM suite's.

use std::ffi::OsString;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use fuser::{Errno, RenameFlags, TimeOrNow};
use konedrive_tree::ItemKind;

use super::*;
use crate::mount::item_fixture::ItemFixture;

#[test]
fn an_extended_attribute_is_answered_by_the_size_of_the_callers_buffer() {
    let cases = [
        // (the value's length, the buffer's size, the answer)
        (5, 0, Ok(XattrReply::Length(5))),
        (0, 0, Ok(XattrReply::Length(0))),
        (5, 4, Err(Errno::ERANGE)),
        (5, 5, Ok(XattrReply::Value)),
        (5, 64, Ok(XattrReply::Value)),
        (0, 8, Ok(XattrReply::Value)),
    ];
    for (value_length, size, reply) in cases {
        assert_eq!(decide_xattr_reply(value_length, size), reply, "a value of {value_length} into a buffer of {size}");
    }
}

#[test]
fn the_kernels_flags_of_a_rename_say_what_becomes_of_the_new_name() {
    let cases = [
        (RenameFlags::empty(), RenameMode::Replace),
        (RenameFlags::RENAME_NOREPLACE, RenameMode::NoReplace),
        (RenameFlags::RENAME_EXCHANGE, RenameMode::Exchange),
        // An exchange is refused below whatever else is asked with it.
        (RenameFlags::RENAME_EXCHANGE | RenameFlags::RENAME_NOREPLACE, RenameMode::Exchange),
    ];
    for (flags, mode) in cases {
        assert_eq!(to_rename_mode(flags), mode, "{flags:?}");
    }
}

#[test]
fn a_time_asked_of_setattr_is_in_whole_seconds_and_the_rest() {
    assert_eq!(to_unix_time(TimeOrNow::SpecificTime(UNIX_EPOCH + Duration::new(1_700_000_000, 123))), (1_700_000_000, 123));
    assert_eq!(to_unix_time(TimeOrNow::SpecificTime(UNIX_EPOCH)), (0, 0));
    assert_eq!(to_unix_time(TimeOrNow::SpecificTime(UNIX_EPOCH - Duration::from_secs(5))), (0, 0), "a time before 1970 is 0");
    assert!(to_unix_time(TimeOrNow::Now).0 > 1_700_000_000, "now is read from the clock");
}

#[test]
fn a_listed_item_is_named_by_its_local_place_and_one_with_no_local_version_has_no_name() {
    let fixture = ItemFixture::new();
    let shown = fixture.show_onedrive_file("docs.txt", b"docs");
    let listed_only = fixture.list_onedrive_file("only-listed.txt", b"listed");
    let directory = fixture.show_onedrive_directory("dir");

    assert_eq!(find_listed_name(&shown), Some(OsString::from("docs.txt").as_os_str()));
    assert_eq!(find_listed_name(&directory), Some(OsString::from("dir").as_os_str()));
    assert_eq!(directory.kind, ItemKind::Directory);
    assert_eq!(find_listed_name(&listed_only), None, "nothing shows it");
}

/// A task of `runtime` that never ends by itself, and whether it was dropped (ended or
/// aborted).
fn spawn_waiting_read(runtime: &tokio::runtime::Handle) -> (tokio::task::JoinHandle<()>, Arc<()>) {
    let held = Arc::new(());
    let in_task = Arc::clone(&held);
    let read = runtime.spawn(async move {
        let _held = in_task;
        std::future::pending::<()>().await
    });
    (read, held)
}

#[test]
fn the_kernels_interrupt_aborts_the_read_of_its_request_and_no_other() {
    let fixture = ItemFixture::new();
    let runtime = fixture.runtime();
    let reads = InterruptibleReads::default();
    let (first, _) = spawn_waiting_read(&runtime);
    let (second, _) = spawn_waiting_read(&runtime);
    reads.start(1, || first.abort_handle());
    reads.start(2, || second.abort_handle());

    reads.interrupt(1);
    reads.interrupt(7);

    assert!(runtime.block_on(first).unwrap_err().is_cancelled(), "the read of request 1 is aborted");
    assert!(!second.is_finished(), "the read of request 2 goes on");
    assert_eq!(crate::panic::lock(&reads.0).by_request.len(), 1);
    second.abort();
}

#[test]
fn a_read_that_ended_is_kept_no_more() {
    let reads = InterruptibleReads::default();
    let fixture = ItemFixture::new();
    let (read, _) = spawn_waiting_read(&fixture.runtime());
    reads.start(3, || read.abort_handle());

    reads.remove(3);

    assert!(crate::panic::lock(&reads.0).by_request.is_empty());
    reads.interrupt(3);
    assert!(!read.is_finished(), "an interrupt that comes late aborts nothing");
    read.abort();
}

#[test]
fn a_stop_gives_up_every_read_and_each_one_started_after_it() {
    let fixture = ItemFixture::new();
    let runtime = fixture.runtime();
    let reads = InterruptibleReads::default();
    let (first, first_held) = spawn_waiting_read(&runtime);
    let (second, second_held) = spawn_waiting_read(&runtime);
    reads.start(1, || first.abort_handle());
    reads.start(2, || second.abort_handle());

    reads.stop();

    assert!(runtime.block_on(first).unwrap_err().is_cancelled());
    assert!(runtime.block_on(second).unwrap_err().is_cancelled());
    assert_eq!((Arc::strong_count(&first_held), Arc::strong_count(&second_held)), (1, 1), "what the reads held is let go");
    assert!(crate::panic::lock(&reads.0).by_request.is_empty());

    // The session's threads answer until the unmount: a read that starts now is given up at once.
    let (late, late_held) = spawn_waiting_read(&runtime);
    reads.start(3, || late.abort_handle());
    assert!(runtime.block_on(late).unwrap_err().is_cancelled());
    assert_eq!(Arc::strong_count(&late_held), 1);
    assert!(crate::panic::lock(&reads.0).by_request.is_empty(), "and is not kept");
}
