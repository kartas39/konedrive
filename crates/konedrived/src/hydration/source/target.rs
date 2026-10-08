//! The file a download writes, and the blocking sections that touch it.
//!
//! `pwrite`, `fsync`, `fallocate` and the attribute calls wait on the disk, so a download makes
//! none of them on a runtime thread: each run of calls that belong together is one *section*
//! on a blocking thread ([`Target::alone`], [`Target::beside`]).
//!
//! A section that has begun runs to its end, whatever becomes of the download that started
//! it. So that nobody finds it still writing, the download's own next section waits for it
//! (the gate): what follows a failed stream of a download in parts runs when the other
//! streams' writes are over.

use std::fs::File;
use std::sync::Arc;

use tokio::sync::RwLock;

pub(super) struct Target {
    file: Arc<File>,
    /// Taken shared by a section that may run beside others, alone by every other one.
    gate: Arc<RwLock<()>>,
}

impl Target {
    pub(super) fn new(file: File) -> Self {
        Self { file: Arc::new(file), gate: Arc::default() }
    }

    /// Runs `work` on a blocking thread once no other section of this file is running, and
    /// with none beside it.
    pub(super) async fn alone<T: Send + 'static>(&self, work: impl FnOnce(&File) -> T + Send + 'static) -> T {
        let gate = Arc::clone(&self.gate).write_owned().await;
        self.run(move |file| {
            let _gate = gate;
            work(file)
        })
        .await
    }

    /// Runs `work` on a blocking thread, beside other sections started this way: the writes
    /// of the streams of a download in parts, each at its own offset.
    pub(super) async fn beside<T: Send + 'static>(&self, work: impl FnOnce(&File) -> T + Send + 'static) -> T {
        let gate = Arc::clone(&self.gate).read_owned().await;
        self.run(move |file| {
            let _gate = gate;
            work(file)
        })
        .await
    }

    async fn run<T: Send + 'static>(&self, work: impl FnOnce(&File) -> T + Send + 'static) -> T {
        let file = Arc::clone(&self.file);
        match tokio::task::spawn_blocking(move || work(&file)).await {
            Ok(done) => done,
            // A panic in a section is the download's own panic.
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(e) => panic!("a blocking section of a download did not run: {e}"),
        }
    }
}

#[cfg(test)]
mod tests;
