use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

/// A source that only counts how often it was told that what was written counts no more.
#[derive(Default)]
struct RejectionCount(AtomicUsize);

#[async_trait::async_trait]
impl ContentSource for RejectionCount {
    async fn fetch(&self, item_id: &str, _from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        Err(SourceError::Transient(format!("nothing is fetched of {item_id} here")))
    }

    fn reject_written(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// What is on disk counts no more from the moment an attempt ends without trusting it —
/// also when no attempt follows — and a failure takes nothing back.
#[tokio::test]
async fn the_source_is_told_that_what_was_written_counts_no_more_when_an_attempt_ends_without_trusting_it() {
    let target = Target::new(tempfile::tempfile().unwrap());
    let tuning = Tuning { checkpoint_every: None, back_off: Duration::ZERO };
    let source = RejectionCount::default();
    let count = || source.0.load(Ordering::SeqCst);

    let mut start_over = StartOver::default();
    assert_eq!(start_over.after(End::Fail(libc::ENOSPC), &target, "id", &source, &tuning).await, Err(libc::ENOSPC));
    assert_eq!(count(), 0, "a failure: what was written is still the content's");
    assert_eq!(start_over.after(End::DropCheckpoint, &target, "id", &source, &tuning).await, Ok(()));
    assert_eq!(count(), 1, "a checkpoint that is not gone on from");
    assert_eq!(start_over.after(End::Mismatch, &target, "id", &source, &tuning).await, Ok(()));
    assert_eq!(count(), 2, "a hash that does not match: starting over");
    assert_eq!(start_over.after(End::Mismatch, &target, "id", &source, &tuning).await, Err(libc::EIO));
    assert_eq!(count(), 3, "the second mismatch: no attempt follows, and it is told all the same");

    let mut start_over = StartOver::default();
    assert_eq!(start_over.after(End::Changed, &target, "id", &source, &tuning).await, Ok(()));
    assert_eq!(count(), 4, "the file changed in OneDrive between two answers");
}

fn answer(served_from: u64, size: Option<u64>) -> Fetched {
    Fetched { served_from, size, mtime: std::time::UNIX_EPOCH, version: None, stream: Box::new(tokio::io::empty()) }
}

/// The size an answer gives the download, or the errno it is refused with.
fn check(fetched: &Fetched, asked_from: u64, original_size: u64) -> Result<u64, i32> {
    check_answer("id", fetched, asked_from, original_size).map_err(|end| match end {
        End::Fail(errno) => errno,
        End::Changed | End::Mismatch | End::DropCheckpoint => panic!("an answer is refused as a failure"),
    })
}

#[test]
fn an_answer_gives_the_size_it_says_and_one_that_says_none_is_refused() {
    assert_eq!(check(&answer(0, Some(10)), 0, 10), Ok(10));
    assert_eq!(check(&answer(4, Some(10)), 4, 10), Ok(10));
    assert_eq!(check(&answer(0, Some(12)), 0, 10), Ok(12), "another size than was asked for goes through");
    assert_eq!(check(&answer(0, Some(10)), 4, 10), Err(libc::EIO), "a stream that starts elsewhere than asked");
    // An empty file is 0 bytes long; 0 said for a file that has bytes is not obeyed here.
    assert_eq!(check(&answer(0, Some(0)), 0, 0), Ok(0));
    assert_eq!(check(&answer(0, Some(0)), 0, 10), Err(libc::EIO));
    // No size said: the download has nothing to go by, whatever size was asked for.
    assert_eq!(check(&answer(0, None), 0, 10), Err(libc::EIO));
    assert_eq!(check(&answer(0, None), 0, 0), Err(libc::EIO), "not said is not an empty file");
}
