/// The pool line (issue #50): the slots in use of the pool's size, then the large files
/// and their streams; in use above the size is shown as it is.
#[test]
fn transfers_start_with_the_pool_summary() {
    let summary = super::TransferSummary {
        active_downloads: 12,
        download_speed: 8_808_038,
        active_uploads: 3,
        upload_speed: 1_258_291,
        pool_in_use: 7,
        pool_size: 32,
        large_files: 1,
        large_streams: 4,
        large_stream_limit: 4,
        retry_after: 0,
        ..Default::default()
    };
    let text = super::transfers_text(&summary, &[], &[]);
    assert_eq!(
        text,
        "Downloading: 12 now, 8.4 MiB/s\nUploading:    3 now, 1.2 MiB/s\nPool: 7 of 32 · large files: 1 (4 of 4 streams)\nNothing is downloading or uploading.\n"
    );
    let waiting = super::TransferSummary { retry_after: 30, ..summary };
    assert_eq!(super::pool_text(&waiting), "Pool: 7 of 32 · large files: 1 (4 of 4 streams) — OneDrive asked to wait 30 s");
    let over = super::TransferSummary { pool_in_use: 18, pool_size: 16, ..summary };
    assert!(super::pool_text(&over).starts_with("Pool: 18 of 16 · "), "{}", super::pool_text(&over));
}

/// Issue #16: each summary line says what is left — files down, changes up — its size and
/// about how long it takes, and what this run has done; the time only when it is known.
#[test]
fn the_summary_lines_say_what_is_left_and_done() {
    use super::QueueTotals;
    let summary = super::TransferSummary {
        active_downloads: 12,
        download_speed: 8_808_038,
        downloads: QueueTotals { left_count: 1234, left_bytes: 51_754_355_917, done_bytes: 3_328_599_654, time_left: 720 },
        active_uploads: 3,
        upload_speed: 1_258_291,
        uploads: QueueTotals { left_count: 6, left_bytes: 1_825_361_101, done_bytes: 262_144_000, time_left: 120 },
        ..Default::default()
    };
    let text = super::transfers_text(&summary, &[], &[]);
    assert!(
        text.starts_with(
            "Downloading: 12 now, 1 234 files left (48.2 GiB, about 12 min), 3.1 GiB done, 8.4 MiB/s\n\
             Uploading:    3 now, 6 changes left (1.7 GiB, about 2 min), 250.0 MiB done, 1.2 MiB/s\n"
        ),
        "{text}"
    );
    let unknown = super::TransferSummary {
        uploads: QueueTotals { left_count: 1, left_bytes: 0, done_bytes: 0, time_left: 0 },
        ..summary
    };
    assert_eq!(super::uploading_line(&unknown), "Uploading:    3 now, 1 change left, 0 B done, 1.2 MiB/s");
    assert_eq!(super::waiting_download_text(1234, 51_754_355_917), "1 234 files (48.2 GiB)");
    assert_eq!(super::waiting_download_text(0, 0), "nothing");
    assert_eq!(
        [45, 3600, 3700, 90_000].map(super::time_left_text),
        ["about 45 s", "about 1 h", "about 1 h 2 min", "about 1 d 1 h"]
    );
}
