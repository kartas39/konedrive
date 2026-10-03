use super::*;
use crate::status::snapshot::SyncSnapshot;

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
