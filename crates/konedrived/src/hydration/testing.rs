//! Test support of `hydration/`: a source with the faults and the counting a test asks for
//! ([`Faulty`]), and the area's fixture. Built for the crate's own tests and, with
//! `fault-injection` ([`Faulty`] only), for the VM suite; no daemon that ships has it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;

use super::source::{ContentSource, Fetched, SourceError};

/// `inner`, with its fetches counted, held back, or broken off.
pub struct Faulty<S> {
    inner: S,
    fail_at: Option<u64>,
    /// With `fail_at`, whether the break applies to the first fetch only. A permanent break
    /// can only reach the give-up path; one that heals lets a test follow a file across two
    /// fetches, which is where the resume arithmetic is.
    heal_after_first_failure: bool,
    fetches: AtomicU64,
    delay: Option<Duration>,
}

impl<S: ContentSource> Faulty<S> {
    pub fn new(inner: S) -> Self {
        Self { inner, fail_at: None, heal_after_first_failure: false, fetches: AtomicU64::new(0), delay: None }
    }

    /// Break the stream at this offset of the file, on every fetch.
    pub fn fail_at(mut self, bytes: u64) -> Self {
        self.fail_at = Some(bytes);
        self.heal_after_first_failure = false;
        self
    }

    /// Break the stream at this offset of the file on the first fetch only; every later
    /// fetch serves the rest of the file.
    pub fn fail_once_at(mut self, bytes: u64) -> Self {
        self.fail_at = Some(bytes);
        self.heal_after_first_failure = true;
        self
    }

    /// Wait this long before every fetch.
    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    /// How many times `fetch` has been called.
    pub fn fetches(&self) -> u64 {
        self.fetches.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl<S: ContentSource> ContentSource for Faulty<S> {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        let fetch_number = self.fetches.fetch_add(1, Ordering::SeqCst);
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        let mut fetched = self.inner.fetch(item_id, from, end).await?;
        match self.fail_at {
            Some(limit) if !self.heal_after_first_failure || fetch_number == 0 => {
                fetched.stream = Box::new(fetched.stream.take(limit.saturating_sub(fetched.served_from)));
            }
            _ => {}
        }
        Ok(fetched)
    }

    fn progress(&self, done: u64, size: u64) {
        self.inner.progress(done, size);
    }
}

/// A placeholder `name` in `dir` for item `item_id`, `size` bytes long, open for reading
/// and writing.
#[cfg(test)]
pub(crate) fn placeholder(dir: &std::path::Path, name: &str, item_id: &str, size: u64) -> std::fs::File {
    let handle = std::fs::File::open(dir).unwrap();
    let mtime = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    konedrive_fs::placeholder::create_placeholder(&handle, name, item_id, size, mtime).unwrap();
    std::fs::File::options().read(true).write(true).open(dir.join(name)).unwrap()
}

/// `size` bytes that differ from one seed to the next and from one 4 KiB to the next.
#[cfg(test)]
pub(crate) fn content(size: usize, seed: u8) -> Vec<u8> {
    (0..size).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed).wrapping_add((i >> 12) as u8)).collect()
}

/// Everything `file` holds, read at its offsets (the file's own offset is not moved).
#[cfg(test)]
pub(crate) fn read_back(file: &std::fs::File) -> Vec<u8> {
    use std::os::unix::fs::FileExt;
    let mut out = vec![0u8; file.metadata().unwrap().len() as usize];
    file.read_exact_at(&mut out, 0).unwrap();
    out
}

/// Fills `file` from `source` as an open of an `online-only` file does (no clearance): the
/// errno the opener would be answered, 0 for a fill that went through.
#[cfg(test)]
pub(crate) async fn fill(file: &std::fs::File, source: &dyn ContentSource) -> i32 {
    use std::os::fd::AsFd;
    let fd = file.as_fd().try_clone_to_owned().unwrap();
    super::source::hydrate_with(fd, source, None).await.err().map_or(0, |e| e.errno())
}

/// Waits until `done` says so, for five seconds at most.
#[cfg(test)]
pub(crate) async fn until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..5000 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("{what} never happened");
}
