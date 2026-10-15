use konedrive_dbus::rows::{Change, KeptBack, KeptBackFiles, KeptBackReason};
use konedrive_reason::{Group, Reason};

use super::{not_uploaded_text, upload_queue_text, quota_text, space_waiting_text, upload_reason_text};

#[test]
fn upload_queue_lines_say_what_waits_and_why() {
    let change = |seq, path: &str, state: &str, sent, total, reason: &str| Change {
        seq,
        kind: "create".to_owned(),
        path: path.to_owned(),
        state: state.to_owned(),
        sent,
        total,
        reason: reason.to_owned(),
        next_try: 0,
    };
    let rows = vec![change(1, "/f/a.txt", "running", 512, 2048, ""), change(2, "/f/a:b", "blocked", 0, 1, "name-characters")];
    let text = upload_queue_text(&rows, true, "konedrivectl");
    assert!(text.contains("running  create   /f/a.txt  25% of 2.0 KiB"), "{text}");
    assert!(text.contains("blocked  create   /f/a:b  (A name OneDrive refuses (it holds one of \" * : < > ? \\ |): rename it to upload it)\n"), "{text}");
    assert!(text.ends_with("`konedrivectl sync upload-queue --all` shows them all\n"), "{text}");
    assert_eq!(upload_queue_text(&[], false, "k"), "Nothing is waiting to upload.\n");
    assert_eq!(upload_reason_text("refused: bad name"), "OneDrive refused it: bad name");
}

/// `sync not-uploaded`: a full OneDrive is one line with its count; the
/// files of a per-file reason follow, capped, with how to see them all.
#[test]
fn not_uploaded_lists_reasons_then_the_files_of_per_file_ones() {
    let row = |group: &str, reason: &str, count, bytes| KeptBackReason { group: group.to_owned(), reason: reason.to_owned(), count, bytes };
    let summary = vec![row("one-action", "quota-exceeded", 5000, 3 << 30), row("per-file", "refused", 25, 0)];
    let items: Vec<KeptBack> = (0..20).map(|i| KeptBack { path: format!("/f/{i}"), reason: "refused: bad name".to_owned() }).collect();
    let text = not_uploaded_text(&summary, &[("refused".to_owned(), KeptBackFiles { items, total: 25 })], "konedrivectl");
    assert!(text.starts_with("Needs you: one action fixes them all:\n  5000, 3.0 GiB: OneDrive is full: free some space in OneDrive.\n"), "{text}");
    assert!(text.contains("\nRefused by OneDrive.\n  /f/0  (OneDrive refused it: bad name)\n"), "{text}");
    assert!(!text.contains("/f/20"), "{text}");
    assert!(text.ends_with("… and 5 more: `konedrivectl sync not-uploaded --all` lists them all\n"), "{text}");
    assert_eq!(not_uploaded_text(&[], &[], "k"), "Everything here is uploaded or waits to be.\n");
}

/// A sentence in brackets has no full stop before the bracket; a reason with no sentence is
/// shown as stored, and as a heading keeps its colon.
#[test]
fn a_reason_in_brackets_has_no_full_stop_and_one_without_a_sentence_keeps_its_colon() {
    let row = |reason: &str| KeptBackReason { group: "waiting".to_owned(), reason: reason.to_owned(), count: 2, bytes: 0 };
    let item = |path: &str, reason: &str| KeptBack { path: path.to_owned(), reason: reason.to_owned() };
    let files = vec![
        ("download-failed".to_owned(), KeptBackFiles { items: vec![item("/f/a", "download-failed: errno 5."), item("/f/b", "download-failed")], total: 2 }),
        ("network".to_owned(), KeptBackFiles { items: vec![item("/f/c", "not allowed now: errno 5")], total: 1 }),
    ];
    let text = not_uploaded_text(&[row("download-failed"), row("network")], &files, "konedrivectl");
    assert!(text.contains("\ndownload-failed:\n  /f/a  (download-failed: errno 5.)\n  /f/b\n"), "{text}");
    assert!(
        text.contains(
            "\nOneDrive could not be reached: tried again later.\n  /f/c  (Uploads are not allowed now: it goes on when they are (errno 5))\n"
        ),
        "{text}"
    );
    // The summary's lines are as they were: after a colon, a sentence whole.
    assert!(text.starts_with("Waiting: these go up by themselves:\n  2: download-failed\n  2: OneDrive could not be reached: tried again later.\n\n"), "{text}");
}

#[test]
fn waiting_for_space_is_one_line_and_too_big_says_what_it_needs() {
    assert_eq!(space_waiting_text(2029, 42 << 30), "2029 files (42.0 GiB) — OneDrive is full");
    assert_eq!(upload_reason_text("too-big:3221225472:1073741824"), "Too big: needs 3.0 GiB, 1.0 GiB free.");
    assert_eq!(upload_reason_text("waiting-for-space"), "Waiting for space: OneDrive is full.");
    assert_eq!(upload_reason_text("too-big"), "Too big for the space left in OneDrive: free up space there, then `sync refresh`.");
    assert_eq!(upload_reason_text("network"), "OneDrive could not be reached: tried again later.");
    assert_eq!(upload_reason_text("local-error"), "The local file could not be read: tried again later.");
    assert_eq!(upload_reason_text("index-error"), "KOneDrive's local index failed: tried again later.");
    assert_eq!(upload_reason_text("upload-error"), "The upload failed: tried again later.");
    assert_eq!(quota_text("nearing", 5 << 30, false), "OneDrive: 5.0 GiB free (quota nearing).\n");
    assert_eq!(quota_text("", 0, false), "");
}

/// Every reason of the settlement's table has its sentence, and one
/// with a detail behind it keeps the detail.
#[test]
fn the_reasons_the_worker_writes_have_sentences() {
    let keys = [
        "paused",
        "upload-session-open",
        "name-held-by-an-upload",
        "changed in OneDrive again and again",
        "the upload session ended twice",
        "not allowed now",
        "blocked",
    ];
    for key in keys {
        assert_ne!(upload_reason_text(key), key, "{key}");
    }
    assert_eq!(
        upload_reason_text("not allowed now: the folder is read-only"),
        "Uploads are not allowed now: it goes on when they are (the folder is read-only)."
    );
    assert!(upload_reason_text("blocked").contains("(blocked)"));
    assert_eq!(upload_reason_text("not-found: errno 5"), "not-found: errno 5", "a key with no sentence stays as stored");
}

/// How a row's reason is stored and sent, written out, and whether
/// the catalogue (`konedrive-text`) has a sentence for it (the rest
/// `konedrivectl` shows as stored). A new variant does not compile until it
/// is spelled here.
fn spelled(reason: &Reason) -> (&'static str, bool) {
    match reason {
        Reason::OpenForWriting => ("open-for-writing", true),
        Reason::NameCharacters => ("name-characters", true),
        Reason::NameSpaces => ("name-spaces", true),
        Reason::NameReserved => ("name-reserved", true),
        Reason::NameNotUtf8 => ("name-not-utf8", true),
        Reason::TooLarge => ("too-large", true),
        Reason::Quota => ("quota-exceeded", true),
        Reason::Forbidden => ("forbidden", true),
        Reason::Refused(_) => ("refused", true),
        Reason::Locked => ("locked", true),
        Reason::NotFound => ("not-found", false),
        Reason::Changed => ("changed-while-sending", false),
        Reason::Parent => ("parent-not-in-onedrive", false),
        Reason::Hash => ("hash-mismatch", false),
        Reason::Paused => ("paused", true),
        Reason::SessionOpen => ("upload-session-open", true),
        Reason::NameHeld => ("name-held-by-an-upload", true),
        Reason::ChangedAgain => ("changed in OneDrive again and again", true),
        Reason::SessionEnded => ("the upload session ended twice", true),
        Reason::NotAllowed(_) => ("not allowed now", true),
        Reason::Blocked => ("blocked", true),
        Reason::NotReadOnDisk => ("not-read-on-disk", true),
        Reason::BeingChecked => ("being-checked", true),
        Reason::QuietTime => ("quiet-time", true),
        Reason::Network => ("network", true),
        Reason::LocalIo => ("local-error", true),
        Reason::Store => ("index-error", true),
        Reason::Failed => ("upload-error", true),
        Reason::WaitingForSpace => ("waiting-for-space", true),
        Reason::TooBig(_) => ("too-big", true),
        Reason::Other(_) => unreachable!("not a spelling of its own"),
    }
}

/// The spellings stored in the store and sent over
/// D-Bus are the contract with the database and with the window's and
/// Dolphin's tables. Each one, written out here, is its variant's key and
/// reads back as the variant; and `konedrivectl` words it, or shows it as
/// stored where it has no sentence yet. A reworded key fails here.
#[test]
fn every_reason_is_stored_under_its_spelling_and_is_worded() {
    assert_eq!(Reason::ALL.len(), 30);
    let mut seen = std::collections::BTreeSet::new();
    for reason in Reason::ALL {
        let (spelling, worded) = spelled(&reason);
        assert_eq!((reason.key(), reason.to_string().as_str()), (spelling, spelling));
        assert_eq!(Reason::parse(spelling), reason, "{spelling}");
        assert_eq!(upload_reason_text(spelling) != spelling, worded, "{spelling}");
        assert!(seen.insert(spelling), "{spelling} is spelled twice");
    }
    // The forms with something behind the key, as the daemon writes them.
    let detailed = [
        ("refused: The name is not allowed", Reason::Refused(Some("The name is not allowed".into())), "OneDrive refused it: The name is not allowed"),
        ("too-big:3221225472:1073741824", Reason::TooBig(Some((3221225472, 1073741824))), "Too big: needs 3.0 GiB, 1.0 GiB free."),
        (
            "not allowed now: the folder is read-only",
            Reason::NotAllowed(Some("the folder is read-only".into())),
            "Uploads are not allowed now: it goes on when they are (the folder is read-only).",
        ),
    ];
    for (stored, reason, text) in detailed {
        assert_eq!(Reason::parse(stored), reason, "{stored}");
        assert_eq!(reason.to_string(), stored);
        assert_eq!(upload_reason_text(stored), text);
    }
    // What neither table knows is shown as it came.
    for stored in ["something-new", "network: connection reset", "error sending request for url (<url>)", "unreadable state \"frobnicate\""] {
        assert_eq!(upload_reason_text(stored), stored);
    }
    assert_eq!(upload_reason_text("too-big:03:1"), "Too big: needs 3 B, 1 B free.");
    let groups: Vec<&str> = Group::ALL.iter().map(|g| g.as_str()).collect();
    assert_eq!(groups, ["one-action", "per-file", "waiting"]);
}
