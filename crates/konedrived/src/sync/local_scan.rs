//! How the Full local scan goes, for `org.konedrive.LocalScan` (issue #8): whether one runs, why, since when
//! and what it has seen so far, and when the last one finished. Only a read-write folder
//! has a watcher, and so a local scan; a read-only one says `none`.
//!
//! The examination tells a [`ScanRun`] ([`ScanProgress`]) as it lists; the run puts that in
//! the folder's published state at most once per [`PUBLISH_EVERY`], and once when it ends.
//! A single place examined after a change is never told (`local::Examiner`).

use std::cell::Cell;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::local::{ScanProgress, ScanReason};
use super::SyncStateHandle;
use crate::config::Mode;

/// A running scan's counts reach the state at most this often.
pub const PUBLISH_EVERY: Duration = Duration::from_secs(1);

/// `LocalScan.State`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ScanState {
    /// A read-only folder: no watcher, no local scan.
    #[default]
    None,
    Idle,
    Running,
}

impl ScanState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Idle => "idle",
            Self::Running => "running",
        }
    }
}

/// The folder's local scan, as `LocalScan`'s properties publish it. While idle, the
/// reason, the start and the counts are the last scan's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalScan {
    pub state: ScanState,
    /// Empty before the first scan.
    pub reason: String,
    /// Unix seconds; 0 before the first scan.
    pub started: i64,
    /// Directories and other entries seen so far, the root not counted.
    pub directories: u64,
    pub files: u64,
    /// Items the base has placed in the folder when the scan started: about how many it
    /// will see. The disk's own count is not known in advance.
    pub expected: u64,
    /// Unix seconds when the last scan finished; 0 for none since the daemon started.
    pub finished: i64,
    /// How long the last finished scan took, in seconds.
    pub took: u32,
}

impl LocalScan {
    /// Follows the folder's mode: read-only has no scan; read-write is idle until one runs.
    pub fn follow(&mut self, mode: Mode) {
        self.state = match (mode, self.state) {
            (Mode::ReadOnly, _) => ScanState::None,
            (Mode::ReadWrite, ScanState::None) => ScanState::Idle,
            (Mode::ReadWrite, state) => state,
        };
    }
}

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
mod tests {
    use super::*;
    use crate::sync::SyncSnapshot;

    fn report(every: Duration) -> ScanReport {
        let state = SyncStateHandle::new(SyncSnapshot::default());
        state.update(|s| {
            s.items_placed = 50;
            s.scan.follow(Mode::ReadWrite);
        });
        ScanReport { state, every }
    }

    #[test]
    fn a_run_is_running_with_its_reason_and_growing_counts_then_idle_with_when_and_how_long() {
        let report = report(Duration::ZERO);
        assert_eq!(report.state.get().scan, LocalScan { state: ScanState::Idle, ..LocalScan::default() }, "not yet");
        let run = report.run(ScanReason::ReadWrite);
        run.started();
        let scan = report.state.get().scan;
        assert_eq!((scan.state, scan.reason.as_str(), scan.expected, scan.directories, scan.files), (ScanState::Running, "read-write", 50, 0, 0));
        assert!(scan.started > 0);
        run.seen(1, 4);
        assert_eq!((report.state.get().scan.directories, report.state.get().scan.files), (1, 4));
        run.seen(3, 9);
        assert_eq!((report.state.get().scan.directories, report.state.get().scan.files), (3, 9));
        run.finish(true);
        let scan = report.state.get().scan;
        assert_eq!((scan.state, scan.directories, scan.files, scan.took), (ScanState::Idle, 3, 9, 0));
        assert!(scan.finished >= scan.started && scan.finished > 0);
    }

    #[test]
    fn counts_reach_the_state_at_most_once_a_second_and_all_of_them_at_the_end() {
        let report = report(PUBLISH_EVERY);
        let run = report.run(ScanReason::Overflow);
        run.started();
        run.seen(1, 1);
        run.seen(2, 7);
        assert_eq!(report.state.get().scan.files, 0, "within the second");
        run.finish(true);
        assert_eq!((report.state.get().scan.directories, report.state.get().scan.files), (2, 7));
    }

    #[test]
    fn a_failed_scan_keeps_the_last_finish_and_one_never_started_says_nothing() {
        let report = report(Duration::ZERO);
        let before = report.state.get().scan;
        report.run(ScanReason::Start).finish(false);
        assert_eq!(report.state.get().scan, before, "no base yet: never started");
        let run = report.run(ScanReason::Start);
        run.started();
        run.finish(false);
        let scan = report.state.get().scan;
        assert_eq!((scan.state, scan.finished), (ScanState::Idle, 0));
    }

    #[test]
    fn a_read_only_folder_has_no_local_scan() {
        let mut scan = LocalScan::default();
        assert_eq!(scan.state, ScanState::None);
        scan.follow(Mode::ReadWrite);
        assert_eq!(scan.state, ScanState::Idle);
        scan.follow(Mode::ReadOnly);
        assert_eq!(scan.state.as_str(), "none");
    }
}
