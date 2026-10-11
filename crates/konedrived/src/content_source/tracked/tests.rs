use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use tokio::io::AsyncReadExt;

use super::*;
use crate::status::transfers::{Transfer, TransferEntry, Transfers};

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

/// The entry of a download that holds its slot, as its holder made it: `total` bytes by
/// the row.
fn start(transfers: &Transfers, total: u64) -> TransferEntry {
    transfers.start("/folder/a.bin".into(), total)
}

#[tokio::test]
async fn the_entry_is_its_holders_from_before_the_first_answer_to_after_the_last_byte() {
    let transfers = Transfers::default();
    let source = QueuedAnswers::of([Err(SourceError::Transient("no network".into())), Ok(Some(8))]);
    let entry = start(&transfers, 8);
    let tracked = Tracked::new(&source, entry.handle());
    assert_eq!(transfers.list(), shown(0, 8), "shown while nothing was asked yet");

    // An asking that is refused: still tried, still shown, nothing moved.
    assert!(tracked.fetch("id", 0, None).await.is_err());
    assert_eq!(transfers.list(), shown(0, 8));

    // The answer: moved on as its bytes are read.
    let fetched = tracked.fetch("id", 0, None).await.ok().expect("the answer");
    assert_eq!(transfers.list(), shown(0, 8));
    read_whole(fetched).await;
    assert_eq!(transfers.list(), shown(8, 8));

    // The source is done with; the download is not: its holder checks and lands it.
    drop(tracked);
    assert_eq!(transfers.list(), shown(8, 8), "shown after the last byte, for as long as its holder keeps it");
    drop(entry);
    assert_eq!(transfers.list(), []);
}

#[tokio::test]
async fn a_source_that_answers_nothing_moves_nothing_and_leaves_no_entry() {
    let transfers = Transfers::default();
    let refusals = [
        SourceError::Transient("no network".into()),
        SourceError::SignedOut("401".into()),
        SourceError::Throttled { detail: "429".into(), retry_after: None },
        SourceError::NotFound("404".into()),
    ];
    for refusal in refusals {
        let source = QueuedAnswers::of([Err(refusal)]);
        let entry = start(&transfers, 8);
        let tracked = Tracked::new(&source, entry.handle());
        assert!(tracked.fetch("id", 0, None).await.is_err());
        assert_eq!(transfers.list(), shown(0, 8), "the try is shown as it was");
        // The try ended: its holder lets the slot and the entry go.
        drop(entry);
        assert_eq!(transfers.list(), [], "nothing is left of it");
        // Whatever is still said through the source reaches no entry.
        tracked.progress(4, 8);
        tracked.reject_written();
        assert_eq!(transfers.list(), []);
    }
}

#[tokio::test]
async fn an_answer_that_tells_no_size_leaves_the_shown_total_as_it_is() {
    let transfers = Transfers::default();
    let source = QueuedAnswers::of([Ok(None), Ok(Some(8)), Ok(None)]);
    let entry = start(&transfers, 0);
    let tracked = Tracked::new(&source, entry.handle());
    drop(tracked.fetch("id", 0, None).await.ok().expect("the answer"));
    assert_eq!(transfers.list(), shown(0, 0), "not known yet");
    read_whole(tracked.fetch("id", 0, None).await.ok().expect("the answer")).await;
    assert_eq!(transfers.list(), shown(8, 8));
    read_whole(tracked.fetch("id", 0, None).await.ok().expect("the answer")).await;
    assert_eq!(transfers.list(), shown(8, 8), "nothing is counted against a size nobody said");
}

#[tokio::test]
async fn a_download_in_parts_is_shown_once_and_says_itself_how_far_it_is() {
    let transfers = Transfers::default();
    let source = QueuedAnswers::of([Ok(Some(8)), Ok(Some(8))]);
    let entry = start(&transfers, 8);
    let tracked = Tracked::new(&source, entry.handle());
    // Two pieces: neither stream moves the entry.
    read_whole(tracked.fetch("id", 0, Some(4)).await.ok().expect("the answer")).await;
    read_whole(tracked.fetch("id", 4, Some(8)).await.ok().expect("the answer")).await;
    assert_eq!(transfers.list(), shown(0, 8));
    tracked.progress(4, 8);
    assert_eq!(transfers.list(), shown(4, 8));
}

#[tokio::test]
async fn what_was_written_counts_no_more_for_the_source_it_fetches_through_and_in_what_is_shown() {
    let transfers = Transfers::default();
    let source = QueuedAnswers::of([Ok(Some(8))]);
    let entry = start(&transfers, 8);
    let tracked = Tracked::new(&source, entry.handle());
    read_whole(tracked.fetch("id", 0, None).await.ok().expect("the answer")).await;
    assert_eq!(transfers.list(), shown(8, 8));
    tracked.reject_written();
    assert_eq!(source.rejections.load(Ordering::SeqCst), 1, "the source below keeps a mark of its own");
    assert_eq!(transfers.list(), shown(0, 8), "nothing of it counts from this moment");
}
