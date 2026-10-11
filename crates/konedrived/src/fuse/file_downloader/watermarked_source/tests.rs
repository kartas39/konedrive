use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use tokio::io::AsyncReadExt;

use super::WatermarkedSource;
use crate::hydration::source::{ContentSource, Fetched, SourceError};

/// A source whose file is the bytes 0 to 9.
struct TenBytes;

#[async_trait::async_trait]
impl ContentSource for TenBytes {
    async fn fetch(&self, _item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        let content: Vec<u8> = (from as u8..end.unwrap_or(10) as u8).collect();
        Ok(Fetched { served_from: from, size: Some(10), mtime: UNIX_EPOCH, version: None, stream: Box::new(std::io::Cursor::new(content)) })
    }
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(future)
}

/// The mark is where the stream starts, and moves with every byte read from it.
#[test]
fn the_mark_moves_as_the_one_stream_is_read() {
    let written_up_to = Arc::new(AtomicU64::new(0));
    let source = WatermarkedSource { inner: &TenBytes, written_up_to: Arc::clone(&written_up_to) };
    block_on(async {
        let mut fetched = source.fetch("A", 0, None).await.unwrap();
        assert_eq!(written_up_to.load(Ordering::SeqCst), 0);
        let mut piece = [0u8; 4];
        fetched.stream.read_exact(&mut piece).await.unwrap();
        assert_eq!((piece, written_up_to.load(Ordering::SeqCst)), ([0, 1, 2, 3], 4));
        let mut rest = Vec::new();
        fetched.stream.read_to_end(&mut rest).await.unwrap();
        assert_eq!((rest.len(), written_up_to.load(Ordering::SeqCst)), (6, 10));
    });
}

/// A download that is continued, or starts over: what is on disk counts from where the
/// stream is served from.
#[test]
fn the_mark_starts_where_the_stream_is_served_from() {
    let written_up_to = Arc::new(AtomicU64::new(9));
    let source = WatermarkedSource { inner: &TenBytes, written_up_to: Arc::clone(&written_up_to) };
    block_on(async {
        let mut fetched = source.fetch("A", 6, None).await.unwrap();
        assert_eq!(written_up_to.load(Ordering::SeqCst), 6);
        let mut rest = Vec::new();
        fetched.stream.read_to_end(&mut rest).await.unwrap();
        assert_eq!((rest, written_up_to.load(Ordering::SeqCst)), (vec![6, 7, 8, 9], 10));
    });
}

/// A download in parts writes out of order: its pieces tell nothing.
#[test]
fn a_piece_of_a_download_in_parts_does_not_move_the_mark() {
    let written_up_to = Arc::new(AtomicU64::new(0));
    let source = WatermarkedSource { inner: &TenBytes, written_up_to: Arc::clone(&written_up_to) };
    block_on(async {
        let mut fetched = source.fetch("A", 4, Some(8)).await.unwrap();
        let mut piece = Vec::new();
        fetched.stream.read_to_end(&mut piece).await.unwrap();
        assert_eq!((piece, written_up_to.load(Ordering::SeqCst)), (vec![4, 5, 6, 7], 0));
    });
}
