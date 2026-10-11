use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use konedrive_graph::pool::{Class, TransferPool};
use konedrive_tree::Kind;

use super::RangeFetcher;
use crate::fuse::directory_contents::tests::row;
use crate::fuse::range_readers::tests::range_readers;
use crate::hydration::source::{ContentSource, Fetched, SourceError};

/// A source whose every file holds the bytes `0..=9`, and which serves a range from where it
/// is asked, or from the file's start when it is `restarts`. It counts what it was told to
/// forget.
#[derive(Default)]
struct TenBytes {
    restarts: bool,
    forgotten: AtomicUsize,
}

#[async_trait::async_trait]
impl ContentSource for TenBytes {
    async fn fetch(&self, _item_id: &str, from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        let served_from = if self.restarts { 0 } else { from };
        let content: Vec<u8> = (served_from as u8..10).collect();
        Ok(Fetched { served_from, size: Some(10), mtime: UNIX_EPOCH, version: None, stream: Box::new(std::io::Cursor::new(content)) })
    }

    fn forget(&self, _item_id: &str) {
        self.forgotten.fetch_add(1, Ordering::SeqCst);
    }
}

fn range_fetcher_over(source: TenBytes) -> (Arc<TransferPool>, Arc<TenBytes>, RangeFetcher) {
    let (pool, range_readers) = range_readers();
    let source = Arc::new(source);
    (pool, Arc::clone(&source), RangeFetcher::new(range_readers, source))
}

#[tokio::test(start_paused = true)]
async fn a_range_is_the_bytes_asked_for_and_no_more() {
    let (pool, _source, range_fetcher) = range_fetcher_over(TenBytes::default());
    let file = row("A", Some("R"), "a.txt", Kind::File);
    assert_eq!(range_fetcher.fetch_range(&file, 2, 5).await, Ok(vec![2, 3, 4]));
    // The file counts as read for a while after.
    assert_eq!(pool.held(Class::Open), 1);
}

#[tokio::test(start_paused = true)]
async fn a_stream_that_starts_elsewhere_than_asked_is_refused() {
    let (_pool, _source, range_fetcher) = range_fetcher_over(TenBytes { restarts: true, ..TenBytes::default() });
    let file = row("A", Some("R"), "a.txt", Kind::File);
    assert_eq!(range_fetcher.fetch_range(&file, 2, 5).await, Err(libc::EIO));
}

#[tokio::test(start_paused = true)]
async fn the_cached_ranges_of_a_file_are_dropped_in_the_source() {
    let (_pool, source, range_fetcher) = range_fetcher_over(TenBytes::default());
    range_fetcher.drop_cached_ranges("A");
    assert_eq!(source.forgotten.load(Ordering::SeqCst), 1);
}
