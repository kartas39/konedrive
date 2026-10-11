use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use tokio::io::AsyncReadExt;

use super::*;
use crate::status::transfers::Transfer;

/// A source that answers what the test queued — the size it tells, or its refusal — and
/// counts how often it was told that what was written counts no more.
#[derive(Default)]
struct QueuedAnswers {
    answers: Mutex<VecDeque<Result<Option<u64>, SourceError>>>,
    rejections: AtomicUsize,
}

impl QueuedAnswers {
    fn of(answers: impl IntoIterator<Item = Result<Option<u64>, SourceError>>) -> Self {
        QueuedAnswers { answers: Mutex::new(answers.into_iter().collect()), rejections: AtomicUsize::new(0) }
    }
}

#[async_trait]
impl ContentSource for QueuedAnswers {
    async fn fetch(&self, _item_id: &str, from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        let size = self.answers.lock().unwrap().pop_front().expect("an answer the test queued")?;
        let bytes = vec![b'a'; (size.unwrap_or(8) - from) as usize];
        Ok(Fetched { served_from: from, size, mtime: std::time::UNIX_EPOCH, version: None, stream: Box::new(std::io::Cursor::new(bytes)) })
    }

    fn reject_written(&self) {
        self.rejections.fetch_add(1, Ordering::SeqCst);
    }
}

async fn read_whole(fetched: Fetched) {
    let (mut stream, mut bytes) = (fetched.stream, Vec::new());
    stream.read_to_end(&mut bytes).await.unwrap();
}

fn shown(done: u64, total: u64) -> Vec<Transfer> {
    vec![Transfer { path: "/folder/a.bin".into(), done, total, open: false }]
}

#[tokio::test]
async fn a_download_is_shown_from_the_first_answer_of_its_source_not_from_its_asking() {
    let transfers = Transfers::default();
    let changes = transfers.subscribe();
    let source = QueuedAnswers::of([Err(SourceError::Transient("no network".into())), Err(SourceError::NotFound("404".into())), Ok(Some(8))]);
    let tracked = Tracked::new(&source, transfers.clone(), "/folder/a.bin");

    // A source that answers nothing: there is no transfer, and never was one.
    assert!(tracked.fetch("id", 0, None).await.is_err());
    assert!(tracked.fetch("id", 0, None).await.is_err());
    assert!(!changes.has_changed().unwrap(), "no entry was added, not even for a moment");
    assert_eq!(tracked.fetched(), None);

    // The first answer: shown from here, and moved on as its bytes are read.
    let fetched = tracked.fetch("id", 0, None).await.ok().expect("the answer");
    assert_eq!(transfers.list(), shown(0, 8));
    read_whole(fetched).await;
    assert_eq!(transfers.list(), shown(8, 8));
    assert_eq!(tracked.fetched(), Some(8));
    drop(tracked);
    assert_eq!(transfers.list(), []);
}

#[tokio::test]
async fn an_answer_that_tells_no_size_leaves_the_shown_total_as_it_is() {
    let transfers = Transfers::default();
    let source = QueuedAnswers::of([Ok(None), Ok(Some(8)), Ok(None)]);
    let tracked = Tracked::new(&source, transfers.clone(), "/folder/a.bin");
    drop(tracked.fetch("id", 0, None).await.ok().expect("the answer"));
    assert_eq!(transfers.list(), shown(0, 0), "not known yet");
    read_whole(tracked.fetch("id", 0, None).await.ok().expect("the answer")).await;
    assert_eq!(transfers.list(), shown(8, 8));
    read_whole(tracked.fetch("id", 0, None).await.ok().expect("the answer")).await;
    assert_eq!(transfers.list(), shown(8, 8), "nothing is counted against a size nobody said");
}

#[tokio::test]
async fn what_was_written_counts_no_more_for_the_source_it_fetches_through_and_in_what_is_shown() {
    let transfers = Transfers::default();
    let source = QueuedAnswers::of([Ok(Some(8))]);
    let tracked = Tracked::new(&source, transfers.clone(), "/folder/a.bin");
    // Before anything was fetched: told on, and nothing is shown.
    tracked.reject_written();
    assert_eq!(source.rejections.load(Ordering::SeqCst), 1, "the source below keeps a mark of its own");
    assert_eq!(transfers.list(), []);

    read_whole(tracked.fetch("id", 0, None).await.ok().expect("the answer")).await;
    assert_eq!(transfers.list(), shown(8, 8));
    tracked.reject_written();
    assert_eq!(source.rejections.load(Ordering::SeqCst), 2);
    assert_eq!(transfers.list(), shown(0, 8), "nothing of it counts from this moment");
}
