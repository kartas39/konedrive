use tokio::task::JoinHandle;

use crate::fuse::kernel_requests::interruptible_reads::RequestId;
use super::InterruptibleReads;

/// A read that never ends by itself.
fn pending_read() -> JoinHandle<()> {
    tokio::spawn(std::future::pending())
}

async fn was_given_up(read: JoinHandle<()>) -> bool {
    read.await.is_err_and(|e| e.is_cancelled())
}

#[tokio::test]
async fn an_interrupted_request_has_its_read_given_up() {
    let interruptible_reads = InterruptibleReads::default();
    let (read, other) = (pending_read(), pending_read());
    interruptible_reads.add(RequestId(7), read.abort_handle());
    interruptible_reads.add(RequestId(8), other.abort_handle());
    interruptible_reads.interrupt(RequestId(7));
    assert!(was_given_up(read).await);
    // The other request's read goes on.
    assert!(!other.is_finished());
    other.abort();
}

#[tokio::test]
async fn a_read_that_ended_is_not_given_up_by_a_late_interrupt() {
    let interruptible_reads = InterruptibleReads::default();
    let read = pending_read();
    interruptible_reads.add(RequestId(7), read.abort_handle());
    interruptible_reads.remove(RequestId(7));
    interruptible_reads.interrupt(RequestId(7));
    // A request that reads nothing is interrupted without harm.
    interruptible_reads.interrupt(RequestId(99));
    tokio::task::yield_now().await;
    assert!(!read.is_finished());
    read.abort();
}

#[tokio::test]
async fn every_read_in_flight_is_given_up_when_the_mount_is_taken_off() {
    let interruptible_reads = InterruptibleReads::default();
    let (first, second) = (pending_read(), pending_read());
    interruptible_reads.add(RequestId(1), first.abort_handle());
    interruptible_reads.add(RequestId(2), second.abort_handle());
    interruptible_reads.abort_all();
    assert!(was_given_up(first).await && was_given_up(second).await);
}
