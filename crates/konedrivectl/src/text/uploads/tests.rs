use super::{not_uploaded_text, outbox_text, quota_text, space_waiting_text, upload_reason_text};

#[test]
fn outbox_lines_say_what_waits_and_why() {
    let rows = vec![
        (1, "create".to_owned(), "/f/a.txt".to_owned(), "running".to_owned(), 512, 2048, String::new(), 0),
        (2, "create".to_owned(), "/f/a:b".to_owned(), "blocked".to_owned(), 0, 1, "name-characters".to_owned(), 0),
    ];
    let text = outbox_text(&rows, true, "konedrivectl");
    assert!(text.contains("running  create   /f/a.txt  25% of 2.0 KiB"), "{text}");
    assert!(text.contains("blocked  create   /f/a:b  (a name OneDrive refuses"), "{text}");
    assert!(text.ends_with("`konedrivectl sync outbox --all` shows them all\n"), "{text}");
    assert_eq!(outbox_text(&[], false, "k"), "Nothing is waiting to upload.\n");
    assert_eq!(upload_reason_text("refused: bad name"), "OneDrive refused it: bad name");
}

/// `sync not-uploaded`: a full OneDrive is one line with its count; the
/// files of a per-file reason follow, capped, with how to see them all.
#[test]
fn not_uploaded_lists_reasons_then_the_files_of_per_file_ones() {
    let summary = vec![
        ("one-action".to_owned(), "quota-exceeded".to_owned(), 5000, 3 << 30),
        ("per-file".to_owned(), "refused".to_owned(), 25, 0),
        ("never".to_owned(), "symlink".to_owned(), 1, 0),
    ];
    let items: Vec<(String, String)> = (0..20).map(|i| (format!("/f/{i}"), "refused: bad name".to_owned())).collect();
    let text = not_uploaded_text(&summary, &[("refused".to_owned(), items, 25)], "konedrivectl");
    assert!(text.starts_with("Needs you: one action fixes them all:\n  5000, 3.0 GiB: OneDrive is full"), "{text}");
    assert!(text.contains("Never uploaded:\n  1: a symbolic link"), "{text}");
    assert!(text.contains("\nrefused by OneDrive:\n  /f/0  (OneDrive refused it: bad name)\n"), "{text}");
    assert!(!text.contains("/f/20"), "{text}");
    assert!(text.ends_with("… and 5 more: `konedrivectl sync not-uploaded --all` lists them all\n"), "{text}");
    assert_eq!(not_uploaded_text(&[], &[], "k"), "Everything here is uploaded or waits to be.\n");
}

#[test]
fn waiting_for_space_is_one_line_and_too_big_says_what_it_needs() {
    assert_eq!(space_waiting_text(2029, 42 << 30), "2029 files (42.0 GiB) — OneDrive is full");
    assert_eq!(upload_reason_text("too-big:3221225472:1073741824"), "too big: needs 3.0 GiB, 1.0 GiB free");
    assert_eq!(upload_reason_text("waiting-for-space"), "waiting for space: OneDrive is full");
    assert!(upload_reason_text("too-big").starts_with("too big for the space left"));
    assert_eq!(upload_reason_text("network"), "OneDrive could not be reached: tried again later");
    assert_eq!(upload_reason_text("local-error"), "the local file could not be read: tried again later");
    assert_eq!(upload_reason_text("index-error"), "konedrive's local index failed: tried again later");
    assert_eq!(upload_reason_text("upload-error"), "the upload failed: tried again later");
    assert_eq!(quota_text("nearing", 5 << 30, false), "OneDrive: 5.0 GiB free (quota nearing).\n");
    assert_eq!(quota_text("", 0, false), "");
}

/// Quality finding `UP3`: every reason the worker's table gained has its sentence, and one
/// with a detail behind it keeps the detail.
#[test]
fn the_reasons_the_worker_writes_have_sentences() {
    let keys = [
        "paused",
        "moved-out-not-opened",
        "upload-session-open",
        "name-held-by-an-upload",
        "changed in OneDrive again and again",
        "changing in OneDrive again and again",
        "the upload session ended twice",
        "not allowed now",
        "state-unreadable",
        "no-name",
        "no-item",
        "no-guard",
        "no-handle",
        "bad-handle",
        "another-item",
        "blocked",
    ];
    for key in keys {
        assert_ne!(upload_reason_text(key), key, "{key}");
    }
    assert_eq!(
        upload_reason_text("not allowed now: the folder is read-only"),
        "uploads are not allowed now: it goes on when they are (the folder is read-only)"
    );
    assert!(upload_reason_text("no-guard").contains("(no-guard)"));
    assert_eq!(upload_reason_text("download-failed: errno 5"), "download-failed: errno 5", "a key with no sentence stays as stored");
}
