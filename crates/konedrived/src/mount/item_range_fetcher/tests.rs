use std::sync::Mutex;

use konedrive_tree::{ContentVersion, DriveId, OneDriveItemId, OneDriveItemReference};

use super::*;
use crate::mount::range_readers::tests::range_readers;
use crate::content_source::source::{Fetched, SourceError};

/// The wait of an account OneDrive asked nothing of.
fn no_throttle_wait() -> ThrottleWait {
    ThrottleWait::new(crate::status::snapshot::SyncStateHandle::new(Default::default()))
}

/// A source that keeps what it was told to forget.
#[derive(Default)]
struct RememberingSource {
    forgotten: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ContentSource for RememberingSource {
    async fn fetch(&self, _item_id: &str, _from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        unreachable!("no range is fetched here")
    }

    fn forget(&self, item_id: &str) {
        self.forgotten.lock().unwrap().push(item_id.to_owned());
    }
}

fn range_source(item_id: &str) -> RangeSource {
    RangeSource {
        onedrive_item: OneDriveItemReference { drive_id: DriveId::new("drive").unwrap(), item_id: OneDriveItemId::new(item_id) },
        content: ContentVersion::Hashed { size: 10, mtime: 1_700_000_000, mtime_nsec: 0, hash: None, ctag: Some("c1".into()) },
    }
}

#[tokio::test]
async fn what_the_source_remembers_of_an_item_is_dropped_when_its_content_changes() {
    let (_pool, range_readers) = range_readers();
    let source = Arc::new(RememberingSource::default());
    let range_fetcher = ItemRangeFetcher::new(range_readers, Arc::clone(&source) as Arc<dyn ContentSource>, no_throttle_wait());
    range_fetcher.forget_ranges(&range_source("A1"));
    range_fetcher.forget_ranges(&range_source("B2"));
    assert_eq!(*source.forgotten.lock().unwrap(), ["A1", "B2"], "asked by the binding's item id, as the ranges are");
}

// The fetch, with the fixture's content source: what it serves by item id, and what it
// was asked.

use std::time::Duration;

use crate::mount::item_fixture::{ContentFetch, ContentRefusal, FixtureContentSource};
use crate::content_source::source::Version;

const MTIME: i64 = 1_700_000_000;
/// The item the ranges are of: named in the journal alone.
const ITEM: LocalItemId = LocalItemId::ROOT;

fn fetcher(source: Arc<dyn ContentSource>) -> ItemRangeFetcher {
    let (_pool, range_readers) = range_readers();
    ItemRangeFetcher::new(range_readers, source, no_throttle_wait())
}

/// The content a reader opened: ten bytes, with the cTag `c1`.
fn opened() -> RangeSource {
    range_source("A1")
}

fn version(ctag: &str) -> Option<Version> {
    Some(Version { ctag: ctag.into(), quick_xor: None })
}

#[tokio::test]
async fn a_range_is_asked_by_the_bindings_item_id_and_answered_from_the_offset_asked() {
    let source = Arc::new(FixtureContentSource::default());
    source.serve("A1", b"0123456789", MTIME, version("c1"));
    let range_fetcher = fetcher(Arc::clone(&source) as Arc<dyn ContentSource>);
    assert_eq!(range_fetcher.fetch_range(ITEM, &opened(), 2, 6).await, Ok(b"2345".to_vec()));
    assert_eq!(range_fetcher.fetch_range(ITEM, &opened(), 0, 10).await, Ok(b"0123456789".to_vec()));
    let asked = |from, end| ContentFetch { onedrive_item_id: "A1".into(), from, end: Some(end) };
    assert_eq!(source.list_fetches(), [asked(2, 6), asked(0, 10)], "by the binding's item id, the range alone");
}

/// A source that restarts the body at 0 whatever was asked, as a server that answers
/// `200` to a range request does, and serves more than was asked.
struct RestartingSource;

#[async_trait::async_trait]
impl ContentSource for RestartingSource {
    async fn fetch(&self, _item_id: &str, _from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        Ok(Fetched { served_from: 0, size: Some(10), mtime: UNIX_EPOCH + Duration::from_secs(MTIME as u64), version: None, stream: Box::new(std::io::Cursor::new(b"0123456789".to_vec())) })
    }
}

#[tokio::test]
async fn a_stream_that_starts_elsewhere_than_asked_is_an_error() {
    let range_fetcher = fetcher(Arc::new(RestartingSource));
    assert_eq!(range_fetcher.fetch_range(ITEM, &opened(), 4, 6).await, Err(RangeRefusal::Failed { errno: libc::EIO }), "never the bytes of another offset");
    // From the start the stream is the one asked for; no more than was asked is read.
    assert_eq!(range_fetcher.fetch_range(ITEM, &opened(), 0, 4).await, Ok(b"0123".to_vec()));
}

/// A source whose answer starts where it was asked and ends after two bytes, as a
/// connection that is cut does.
struct CutSource;

#[async_trait::async_trait]
impl ContentSource for CutSource {
    async fn fetch(&self, _item_id: &str, from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        let served: Vec<u8> = b"0123456789".iter().copied().skip(from as usize).take(2).collect();
        Ok(Fetched { served_from: from, size: None, mtime: UNIX_EPOCH + Duration::from_secs(MTIME as u64), version: None, stream: Box::new(std::io::Cursor::new(served)) })
    }
}

#[tokio::test]
async fn an_answer_that_ends_before_the_end_asked_is_an_error() {
    let range_fetcher = fetcher(Arc::new(CutSource));
    assert_eq!(range_fetcher.fetch_range(ITEM, &opened(), 4, 8).await, Err(RangeRefusal::Failed { errno: libc::EIO }), "never a part as if it were the range");
    assert_eq!(range_fetcher.fetch_range(ITEM, &opened(), 8, 10).await, Ok(b"89".to_vec()), "a range that is answered whole is given");
}

#[tokio::test]
async fn a_range_of_a_content_onedrive_no_longer_has_is_stale() {
    let source = Arc::new(FixtureContentSource::default());
    let range_fetcher = fetcher(Arc::clone(&source) as Arc<dyn ContentSource>);
    // OneDrive has another content of the item by now: told by its cTag.
    source.serve("A1", b"abcdefghij", MTIME, version("c2"));
    assert_eq!(range_fetcher.fetch_range(ITEM, &opened(), 2, 6).await, Err(RangeRefusal::AnotherContent), "no byte of one content is read as another's");
    // With no cTag from the source: by its size, or its time.
    source.serve("A1", b"0123456789ab", MTIME, None);
    assert_eq!(range_fetcher.fetch_range(ITEM, &opened(), 2, 6).await, Err(RangeRefusal::AnotherContent));
    source.serve("A1", b"abcdefghij", MTIME + 60, None);
    assert_eq!(range_fetcher.fetch_range(ITEM, &opened(), 2, 6).await, Err(RangeRefusal::AnotherContent));
    // OneDrive does not have the file any more.
    source.refuse("A1", Some(ContentRefusal::NotFound));
    assert_eq!(range_fetcher.fetch_range(ITEM, &opened(), 2, 6).await, Err(RangeRefusal::AnotherContent));
    // The content that was opened is read.
    source.refuse("A1", None);
    source.serve("A1", b"0123456789", MTIME, None);
    assert_eq!(range_fetcher.fetch_range(ITEM, &opened(), 2, 6).await, Ok(b"2345".to_vec()));
}

#[tokio::test]
async fn a_source_that_fails_is_an_error_of_the_read() {
    let source = Arc::new(FixtureContentSource::default());
    source.serve("A1", b"0123456789", MTIME, version("c1"));
    source.refuse("A1", Some(ContentRefusal::Transient));
    let range_fetcher = fetcher(Arc::clone(&source) as Arc<dyn ContentSource>);
    assert_eq!(range_fetcher.fetch_range(ITEM, &opened(), 2, 6).await, Err(RangeRefusal::Failed { errno: libc::EIO }));
}

#[test]
fn an_answer_tells_another_content_by_its_ctag_else_by_its_size_and_time_and_one_that_says_nothing_tells_none() {
    let opened = opened().content;
    let answer = |size: Option<u64>, mtime, ctag: Option<&str>| RangeAnswerFacts { size, mtime, ctag: ctag.map(str::to_owned) };
    // The cTags decide when both sides have one, whatever the size and the time say.
    assert!(!decide_is_another_content(&answer(Some(10), MTIME, Some("c1")), &opened));
    assert!(!decide_is_another_content(&answer(None, 0, Some("c1")), &opened));
    assert!(!decide_is_another_content(&answer(Some(0), 0, Some("c1")), &opened));
    assert!(!decide_is_another_content(&answer(Some(10), MTIME + 60, Some("c1")), &opened), "the time of a content can be set; its cTag is the content's");
    assert!(decide_is_another_content(&answer(Some(10), MTIME, Some("c2")), &opened));
    // No cTag on one side: the size and the time in whole seconds.
    assert!(!decide_is_another_content(&answer(Some(10), MTIME, None), &opened));
    assert!(decide_is_another_content(&answer(Some(11), MTIME, None), &opened));
    assert!(decide_is_another_content(&answer(Some(10), MTIME + 1, None), &opened));
    let without_ctag = ContentVersion::Hashed { size: 10, mtime: MTIME, mtime_nsec: 0, hash: None, ctag: None };
    assert!(!decide_is_another_content(&answer(Some(10), MTIME, Some("c2")), &without_ctag), "the limit: the same size and time read as the same content");
    assert!(decide_is_another_content(&answer(Some(11), MTIME, Some("c2")), &without_ctag));
    // Only what was said is held against the content: a size that was not said is not a
    // size of 0, and a time that was not said is no time.
    assert!(!decide_is_another_content(&answer(None, MTIME, None), &opened), "no size said: the time alone");
    assert!(decide_is_another_content(&answer(None, MTIME + 1, None), &opened));
    assert!(!decide_is_another_content(&answer(Some(10), 0, None), &opened), "no time said: the size alone");
    // A size of 0 that was said is a size: the file was emptied in OneDrive.
    assert!(decide_is_another_content(&answer(Some(0), 0, None), &opened), "0 said against a content that has bytes");
    let empty = ContentVersion::Hashed { size: 0, mtime: MTIME, mtime_nsec: 0, hash: None, ctag: None };
    assert!(!decide_is_another_content(&answer(Some(0), MTIME, None), &empty), "an empty file is 0 bytes long");
    assert!(decide_is_another_content(&answer(Some(5), MTIME, None), &empty));
    // An answer that says nothing of the item, as the source over Graph's: no other
    // content is told by it.
    assert!(!decide_is_another_content(&answer(None, 0, None), &opened));
    assert!(!decide_is_another_content(&answer(None, 0, None), &without_ctag));
}

/// A fetcher over `source` with a wait of its own, and the state the wait is published in.
fn fetcher_with_wait(source: Arc<dyn ContentSource>) -> (ItemRangeFetcher, crate::status::snapshot::SyncStateHandle) {
    let (_pool, range_readers) = range_readers();
    let state = crate::status::snapshot::SyncStateHandle::new(Default::default());
    (ItemRangeFetcher::new(range_readers, source, ThrottleWait::new(state.clone())), state)
}

fn publish_wait(state: &crate::status::snapshot::SyncStateHandle, seconds_left: u32) {
    state.set_throughput(konedrive_graph::pool::Throughput { retry_after: seconds_left, ..Default::default() });
}

#[tokio::test]
async fn a_range_is_refused_at_once_while_onedrive_asks_to_wait_and_fetched_after() {
    let source = Arc::new(FixtureContentSource::default());
    source.serve("A1", b"0123456789", MTIME, version("c1"));
    let (range_fetcher, state) = fetcher_with_wait(Arc::clone(&source) as Arc<dyn ContentSource>);
    publish_wait(&state, 300);
    assert_eq!(range_fetcher.fetch_range(ITEM, &opened(), 2, 6).await, Err(RangeRefusal::ThrottleWait));
    assert_eq!(source.list_fetches(), [], "OneDrive is not asked");
    publish_wait(&state, 0);
    assert_eq!(range_fetcher.fetch_range(ITEM, &opened(), 2, 6).await, Ok(b"2345".to_vec()), "the same read, asked again after the wait");
}

/// A source that never answers, as a request that waits out a throttle does; and one
/// that says the throttle itself.
struct SilentSource;

#[async_trait::async_trait]
impl ContentSource for SilentSource {
    async fn fetch(&self, _item_id: &str, _from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        std::future::pending().await
    }
}

struct ThrottledSource;

#[async_trait::async_trait]
impl ContentSource for ThrottledSource {
    async fn fetch(&self, _item_id: &str, _from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        Err(SourceError::Throttled { detail: "429".into(), retry_after: None })
    }
}

#[tokio::test]
async fn a_range_asked_when_the_wait_starts_is_refused_and_not_waited_for() {
    let (range_fetcher, state) = fetcher_with_wait(Arc::new(SilentSource));
    let read = tokio::spawn(async move { range_fetcher.fetch_range(ITEM, &opened(), 2, 6).await });
    tokio::task::yield_now().await;
    assert!(!read.is_finished(), "OneDrive has not answered");
    publish_wait(&state, 300);
    let refused = tokio::time::timeout(Duration::from_secs(5), read).await.expect("given up when the wait starts").unwrap();
    assert_eq!(refused, Err(RangeRefusal::ThrottleWait));

    // A source that says the throttle itself: the same refusal.
    let (range_fetcher, _state) = fetcher_with_wait(Arc::new(ThrottledSource));
    assert_eq!(range_fetcher.fetch_range(ITEM, &opened(), 2, 6).await, Err(RangeRefusal::ThrottleWait));
}
