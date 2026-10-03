//! How the Full local scan goes, for `org.konedrive.LocalScan` (issue #8): whether one runs, why, since when
//! and what it has seen so far, and when the last one finished. Only a read-write folder
//! has a watcher, and so a local scan; a read-only one says `none`.
//!
//! The examination tells a [`ScanRun`] ([`ScanProgress`]) as it lists; the run puts that in
//! the folder's published state at most once per [`PUBLISH_EVERY`], and once when it ends.
//! A single place examined after a change is never told (`local::Examiner`).

use std::cell::Cell;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::local::{ScanProgress, ScanReason};
use crate::status::snapshot::{ScanState, SyncStateHandle};

/// A running scan's counts reach the state at most this often.
pub const PUBLISH_EVERY: Duration = Duration::from_secs(1);

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Where a folder's scans are told: the folder's published state. Kept by the
/// watcher's sink (`watcher::ExamineSink`).
#[derive(Clone)]
pub struct ScanReport {
    pub state: SyncStateHandle,
    /// [`PUBLISH_EVERY`]; tests shorten it.
    pub every: Duration,
}

impl ScanReport {
    pub fn new(state: SyncStateHandle) -> Self {
        Self { state, every: PUBLISH_EVERY }
    }

    /// One Full local scan, for `reason`; nothing is said until the examination has
    /// started it ([`ScanProgress::started`]).
    pub fn run(&self, reason: ScanReason) -> ScanRun<'_> {
        ScanRun { report: self, reason, began: Cell::new(None), last: Cell::new(None), seen: Cell::new((0, 0)) }
    }
}

/// One Full local scan as it goes. [`finish`](Self::finish) it with how it ended.
pub struct ScanRun<'r> {
    report: &'r ScanReport,
    reason: ScanReason,
    /// When the examination started it; `None` until then (no base yet, the root gone).
    began: Cell<Option<Instant>>,
    /// When the counts last reached the state.
    last: Cell<Option<Instant>>,
    seen: Cell<(u64, u64)>,
}

impl ScanProgress for ScanRun<'_> {
    fn started(&self) {
        let now = Instant::now();
        self.began.set(Some(now));
        self.last.set(Some(now));
        self.seen.set((0, 0));
        let (reason, started) = (self.reason.as_str().to_owned(), unix_now());
        self.report.state.update(|s| {
            let expected = s.items_placed;
            let scan = &mut s.scan;
            scan.state = ScanState::Running;
            scan.reason = reason;
            scan.started = started;
            scan.directories = 0;
            scan.files = 0;
            scan.expected = expected;
        });
    }

    fn seen(&self, directories: u64, files: u64) {
        self.seen.set((directories, files));
        let now = Instant::now();
        if self.last.get().is_some_and(|last| now.duration_since(last) < self.report.every) {
            return;
        }
        self.last.set(Some(now));
        self.report.state.update(|s| {
            s.scan.directories = directories;
            s.scan.files = files;
        });
    }
}

impl ScanRun<'_> {
    /// The scan ended: `done` when the examination finished it. One that failed part way
    /// leaves the last finished scan's time as it was (it runs again).
    pub fn finish(self, done: bool) {
        let Some(began) = self.began.get() else { return };
        let (directories, files) = self.seen.get();
        let took = u32::try_from(began.elapsed().as_secs()).unwrap_or(u32::MAX);
        let finished = unix_now();
        self.report.state.update(|s| {
            let scan = &mut s.scan;
            // A switch to read-only meanwhile has the last word.
            if scan.state == ScanState::Running {
                scan.state = ScanState::Idle;
            }
            scan.directories = directories;
            scan.files = files;
            if done {
                scan.finished = finished;
                scan.took = took;
            }
        });
    }
}

#[cfg(test)]
mod tests;
