//! A stop of the daemon between two system calls, which nothing but a
//! switch can make happen. Per thread: for tests that call the materializer
//! on their own thread.

use std::cell::Cell;

thread_local! {
    static STOP_AFTER_UNLINK: Cell<bool> = const { Cell::new(false) };
}

pub(in crate::remote::materialize) fn stop_after_unlink(stop: bool) {
    STOP_AFTER_UNLINK.with(|s| s.set(stop));
}

pub(super) fn stops_after_unlink() -> bool {
    STOP_AFTER_UNLINK.with(Cell::get)
}
