use std::fs::File;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::SeqCst;
use std::time::SystemTime;

use super::*;
use crate::status::report::Report;
use crate::status::snapshot::SyncSnapshot;

fn in_folder(root: &str) -> SyncStateHandle {
    SyncStateHandle::new(SyncSnapshot { root_path: root.into(), ..SyncSnapshot::default() })
}

/// `LocalBytes` is what the files occupy (`st_blocks ×
/// 512`): a placeholder counts as the nothing it takes, a downloaded file
/// as its blocks, and neither konedrive's own `.konedrive-*` entries nor
/// a symbolic link to a file count at all — the walk passes them by.
#[test]
fn local_bytes_count_what_files_occupy_not_their_size() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let handle = File::open(root).unwrap();
    konedrive_fs::placeholder::create_placeholder(&handle, "online.bin", "P", 1 << 20, SystemTime::UNIX_EPOCH)
        .unwrap();
    std::fs::create_dir(root.join("docs")).unwrap();
    std::fs::write(root.join("docs/here.bin"), vec![7u8; 64 * 1024]).unwrap();
    std::fs::create_dir(root.join(".konedrive-holding")).unwrap();
    std::fs::write(root.join(".konedrive-holding/held.bin"), vec![1u8; 64 * 1024]).unwrap();
    std::fs::write(root.join("docs/.konedrive-new-X"), vec![1u8; 64 * 1024]).unwrap();
    std::os::unix::fs::symlink(root.join("docs/here.bin"), root.join("link.bin")).unwrap();

    let blocks = |rel: &str| std::fs::symlink_metadata(root.join(rel)).unwrap().blocks() * 512;
    assert!(blocks("online.bin") < 8 * 512, "a placeholder takes next to nothing");
    assert!(blocks("docs/here.bin") >= 64 * 1024);
    assert_eq!(local_bytes(root), blocks("online.bin") + blocks("docs/here.bin"));
}

/// A `Report` that reports nowhere — the plain
/// `serve_hydrations`' — starts no walker, and a walker stopped starts
/// again at the next kick.
#[tokio::test(start_paused = true)]
async fn a_report_for_nowhere_starts_no_walker_and_a_stopped_one_starts_again() {
    let nowhere = Report::nowhere();
    nowhere.space.kick();
    assert!(!nowhere.space.running(), "a throwaway report spawned a walker");

    let report = Report::new(in_folder("/r"));
    report.space.kick();
    assert!(report.space.running());
    report.space.stop();
    tokio::task::yield_now().await;
    assert!(!report.space.running(), "stopped");
    report.space.kick();
    assert!(report.space.running(), "and started again when asked");
}

/// Measured at once when asked, then at most every five
/// seconds — asking twice meanwhile makes one more walk, when the time is
/// up. On the paused clock: no real second passes.
#[tokio::test(start_paused = true)]
async fn local_space_is_measured_at_once_then_at_most_every_five_seconds() {
    let state = SyncStateHandle::new(SyncSnapshot { root_path: "/r".into(), ..SyncSnapshot::default() });
    let walks = Arc::new(AtomicU64::new(0));
    let counted = Arc::clone(&walks);
    // Each walk "measures" how many walks there have been.
    let space = LocalSpace::measuring(state.clone(), Arc::new(move |_: &Path| counted.fetch_add(1, SeqCst) + 1));
    let mut seen = state.subscribe();

    let start = tokio::time::Instant::now();
    space.kick();
    seen.wait_for(|s| s.local_bytes == 1).await.unwrap();
    assert!(start.elapsed() < SPACE_SPACING, "the first walk waits for nothing");

    let asked = tokio::time::Instant::now();
    space.kick();
    space.kick();
    seen.wait_for(|s| s.local_bytes == 2).await.unwrap();
    assert!(asked.elapsed() >= SPACE_SPACING - Duration::from_millis(1), "{:?}", asked.elapsed());
    tokio::time::sleep(SPACE_SPACING * 3).await;
    assert_eq!(walks.load(SeqCst), 2, "two kicks meanwhile make one walk");
}
