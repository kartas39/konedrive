//! The file a fill writes, and the blocking sections that touch it.
//!
//! `pwrite`, `fsync`, `fallocate` and the attribute calls wait on the disk, so a fill makes
//! none of them on a runtime thread: each run of calls that belong together is one *section*
//! on a blocking thread ([`Target::alone`], [`Target::beside`]).
//!
//! A section that has begun runs to its end, whatever becomes of the fill that started it. So
//! that nobody finds it still writing:
//!
//! - the fill's own next section waits for it (the gate): a roll-back after one stream of a
//!   download in parts failed runs when the other streams' writes are over;
//! - it keeps the inode locked, when the fill runs under the per-inode lock
//!   (`folder::locks::holding`): a fill dropped during a section lets go of the lock when the
//!   section ends.

use std::fs::File;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::folder::locks::{hold_in_force, InodeHold};

pub(super) struct Target {
    file: Arc<File>,
    /// Taken shared by a section that may run beside others, alone by every other one.
    gate: Arc<RwLock<()>>,
    hold: Option<InodeHold>,
}

impl Target {
    /// `file`, written under the per-inode lock the calling work runs under, if any.
    pub(super) fn new(file: File) -> Self {
        Self { file: Arc::new(file), gate: Arc::default(), hold: hold_in_force() }
    }

    /// For what is not a blocking call: handing the file to the helper's link.
    pub(super) fn file(&self) -> &File {
        &self.file
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
        let (file, hold) = (Arc::clone(&self.file), self.hold.clone());
        let section = tokio::task::spawn_blocking(move || {
            // Let go of when the section is over, not when the fill is.
            let _hold = hold;
            work(&file)
        });
        match section.await {
            Ok(done) => done,
            // A panic in a section is the fill's own panic, as it was when the calls ran in
            // place: `hydration::server` turns it into a denied open.
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(e) => panic!("a blocking section of a fill did not run: {e}"),
        }
    }
}

#[cfg(test)]
mod tests;
