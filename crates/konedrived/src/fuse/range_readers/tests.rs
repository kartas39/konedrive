use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use konedrive_graph::pool::{Class, TransferPool};
use konedrive_tree::Kind;

use super::{RangeReaders, READ_LINGERS};
use crate::fuse::directory_contents::tests::row;
use crate::hydration::source::{ContentSource, Fetched, SourceError};

/// A source whose every file holds the bytes `0..=9`, and which serves a range from where it
/// is asked, or from the file's start when it is `restarts`.
struct TenBytes {
    restarts: bool,
}

#[async_trait::async_trait]
impl ContentSource for TenBytes {
    async fn fetch(&self, _item_id: &str, from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        let served_from = if self.restarts { 0 } else { from };
        let content: Vec<u8> = (served_from as u8..10).collect();
        Ok(Fetched { served_from, size: 10, mtime: UNIX_EPOCH, version: None, stream: Box::new(std::io::Cursor::new(content)) })
    }
}

fn range_readers_over(source: TenBytes) -> (Arc<TransferPool>, Arc<RangeReaders>) {
    let pool = TransferPool::new(4);
    let range_readers = Arc::new(RangeReaders::new(Arc::clone(&pool), Arc::new(source), tokio::runtime::Handle::current()));
    (pool, range_readers)
}

fn range_readers() -> (Arc<TransferPool>, Arc<RangeReaders>) {
    range_readers_over(TenBytes { restarts: false })
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

#[tokio::test(start_paused = true)]
async fn a_range_is_the_bytes_asked_for_and_no_more() {
    let (pool, range_readers) = range_readers();
    let file = row("A", Some("R"), "a.txt", Kind::File);
    assert_eq!(range_readers.fetch_range(&file, 2, 5).await, Ok(vec![2, 3, 4]));
    // The file counts as read for a while after.
    assert_eq!(pool.held(Class::Open), 1);
}

#[tokio::test(start_paused = true)]
async fn a_stream_that_starts_elsewhere_than_asked_is_refused() {
    let (_pool, range_readers) = range_readers_over(TenBytes { restarts: true });
    let file = row("A", Some("R"), "a.txt", Kind::File);
    assert_eq!(range_readers.fetch_range(&file, 2, 5).await, Err(libc::EIO));
}
