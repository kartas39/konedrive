use std::sync::Arc;
use std::time::Duration;

use konedrive_graph::pool::{Class, TransferPool};

use super::{RangeReaders, READ_LINGERS};

pub(in crate::fuse) fn range_readers() -> (Arc<TransferPool>, Arc<RangeReaders>) {
    let pool = TransferPool::new(4);
    let range_readers = Arc::new(RangeReaders::new(Arc::clone(&pool), tokio::runtime::Handle::current()));
    (pool, range_readers)
}

/// Time goes on by `duration`, and the timers that are due run.
async fn pass(duration: Duration) {
    tokio::time::sleep(duration).await;
    tokio::task::yield_now().await;
}

#[tokio::test(start_paused = true)]
async fn a_file_being_read_holds_one_slot_however_many_reads_it_has() {
    let (pool, range_readers) = range_readers();
    let first = range_readers.begin_read("A").await;
    let second = range_readers.begin_read("A").await;
    assert_eq!(pool.held(Class::Open), 1);
    // Another file's reads hold a slot of their own.
    let other = range_readers.begin_read("B").await;
    assert_eq!(pool.held(Class::Open), 2);
    drop((first, second, other));
}

#[tokio::test(start_paused = true)]
async fn the_slot_is_held_until_the_read_lingered_out() {
    let (pool, range_readers) = range_readers();
    drop(range_readers.begin_read("A").await);
    pass(READ_LINGERS - Duration::from_millis(100)).await;
    assert_eq!(pool.held(Class::Open), 1);
    pass(Duration::from_millis(200)).await;
    assert_eq!(pool.held(Class::Open), 0);
}

#[tokio::test(start_paused = true)]
async fn a_read_in_flight_keeps_the_slot_however_long_it_takes() {
    let (pool, range_readers) = range_readers();
    let active_read = range_readers.begin_read("A").await;
    pass(READ_LINGERS * 5).await;
    assert_eq!(pool.held(Class::Open), 1);
    // It lingers from the end of the read, not from its start.
    drop(active_read);
    pass(READ_LINGERS - Duration::from_millis(100)).await;
    assert_eq!(pool.held(Class::Open), 1);
    pass(READ_LINGERS).await;
    assert_eq!(pool.held(Class::Open), 0);
}

#[tokio::test(start_paused = true)]
async fn the_next_read_of_a_program_keeps_the_file_being_read() {
    let (pool, range_readers) = range_readers();
    drop(range_readers.begin_read("A").await);
    pass(READ_LINGERS / 2).await;
    drop(range_readers.begin_read("A").await);
    pass(READ_LINGERS - Duration::from_millis(100)).await;
    assert_eq!(pool.held(Class::Open), 1);
    pass(READ_LINGERS).await;
    assert_eq!(pool.held(Class::Open), 0);
}
