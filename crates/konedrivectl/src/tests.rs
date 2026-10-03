/// Issue #54: `sync status` says whether changes from OneDrive arrive live, and nothing
/// while the socket is off (the pause or the hold says why).
#[test]
fn the_live_changes_line_says_live_or_every_minute() {
    assert_eq!(super::live_text("connected"), Some("live"));
    assert_eq!(super::live_text("connecting"), Some("every minute (connecting)"));
    assert_eq!(super::live_text("off"), None);
}

/// Issue #21: the browser opens only on a terminal, and never with the variable set.
#[test]
fn the_browser_opens_only_on_a_terminal_without_the_variable() {
    use std::ffi::OsStr;
    assert!(super::opens_browser(None, true));
    assert!(super::opens_browser(Some(OsStr::new("")), true));
    assert!(!super::opens_browser(None, false));
    assert!(!super::opens_browser(Some(OsStr::new("1")), true));
    assert!(!super::opens_browser(Some(OsStr::new("1")), false));
}

use super::{
    account_refusal_text, choose, command_prefix, dev_refusal_text, human_bytes, not_uploaded_text, outbox_text, parse_duration, quota_text,
    space_waiting_text,
    refusal_text, refusal_text_in, removed_text, rescue_dirs, shell_word, skip_reason_text, upload_reason_text,
    write_secret_atomically, AccountAction, AccountInfo,
    Context, NoChoice, Source, SyncAction,
};

/// `account remove` and `sync forget` refused while changes wait say how
/// to see them and how to drop them.
#[test]
fn a_remove_or_forget_refused_while_changes_wait_says_what_to_do() {
    let detail = "2 change(s) made here have not been uploaded yet";
    let removed = refusal_text(SyncAction::Remove("Test"), Some("org.konedrive.Error.PendingUploads"), detail, "/home/u/OneDrive");
    assert!(removed.contains("was not removed") && removed.contains(detail), "{removed}");
    assert!(removed.contains("`konedrivectl --account Test account mode read-only --force`"), "{removed}");
    let forgot = refusal_text(SyncAction::Forget, Some("org.konedrive.Error.PendingUploads"), detail, "/home/u/OneDrive");
    assert!(forgot.contains("still registered") && forgot.contains("`konedrivectl account mode read-only --force`"), "{forgot}");
}

/// `account mode` and `export-access-token --read-write` explain each refusal by its
/// name (`docs/design/writes.md` §11): the gate, uploads waiting, a read-only account.
#[test]
fn a_refused_mode_is_explained_by_its_name() {
    let prefix = "konedrivectl --account Test";
    let pending = account_refusal_text(
        AccountAction::SetMode("Test", "read-only", prefix),
        Some("org.konedrive.Error.PendingUploads"),
        "3 changes made here have not been uploaded yet",
    );
    assert!(pending.contains("3 changes") && pending.contains("konedrivectl --account Test account mode read-only --force"), "{pending}");
    let signed_out = account_refusal_text(AccountAction::SetMode("Test", "read-write", prefix), Some("org.konedrive.Error.NotSignedIn"), "x");
    assert!(signed_out.contains("`konedrivectl --account Test login`"), "{signed_out}");
    let other = account_refusal_text(AccountAction::SetMode("Test", "read-write", prefix), Some("org.konedrive.Error.Failed"), "no client id");
    assert_eq!(other, "Test was not switched to read-write: no client id");
    let read_only = dev_refusal_text(Some("org.konedrive.Error.ModeNotGranted"), "read-only", prefix);
    assert!(read_only.contains("`konedrivectl --account Test account mode read-write`"), "{read_only}");
    let gate = dev_refusal_text(Some("org.konedrive.Error.WritesNotAllowed"), "x", prefix);
    assert!(gate.contains("write_test_drive_ids") && gate.contains("Without --read-write"), "{gate}");
}

fn account(id: &str, label: &str, email: &str) -> AccountInfo {
    AccountInfo {
        path: konedrive_dbus::account_path(id).unwrap(),
        id: id.into(),
        label: label.into(),
        email: email.into(),
    }
}

/// Design §5.1: an exact id, else a label, else an email, the last two
/// whatever the case; with no name, the only account; several and no
/// name is a mistake on the command line (exit status 2).
#[test]
fn an_account_is_chosen_by_id_label_or_email() {
    let accounts =
        [account("3f9a1c0e5b7d", "Personal", "ann@outlook.com"), account("8c21d07a44e1", "Family", "")];
    let chosen = |name: &str| choose(&accounts, Some((name, Source::Option))).map(|a| a.label.as_str());
    assert_eq!(chosen("8c21d07a44e1"), Ok("Family"));
    assert_eq!(chosen("family"), Ok("Family"));
    assert_eq!(chosen("ANN@outlook.com"), Ok("Personal"));
    let unknown = chosen("nobody").unwrap_err();
    assert_eq!(unknown.exit_status(), 2);
    assert!(unknown.to_string().contains("Personal, Family"), "{unknown}");

    let several = choose(&accounts, None).unwrap_err();
    assert_eq!(several, NoChoice::Several { labels: vec!["Personal".into(), "Family".into()] });
    assert_eq!(several.to_string(), "Several accounts: choose one with --account (Personal, Family)");
    assert_eq!(several.exit_status(), 2);
    assert_eq!(choose(&accounts[..1], None).unwrap().label, "Personal");
    assert_eq!(choose(&[], None).unwrap_err().exit_status(), 1);
    assert!(choose(&[], None).unwrap_err().to_string().contains("konedrivectl account add <label>"));
}

/// A name that fits two accounts — one's id and the other's label, or one label twice in a
/// hand-edited `config.toml` — is refused with exit status 2, listing both, never taken
/// as the first: `account remove` asks nothing.
#[test]
fn a_name_that_fits_two_accounts_is_refused() {
    let accounts = [account("3f9a1c0e5b7d", "Personal", ""), account("8c21d07a44e1", "3f9a1c0e5b7d", "")];
    let refused = choose(&accounts, Some(("3f9a1c0e5b7d", Source::Argument))).unwrap_err();
    assert_eq!(refused.exit_status(), 2);
    let said = refused.to_string();
    assert!(said.contains("Personal (3f9a1c0e5b7d), 3f9a1c0e5b7d (8c21d07a44e1)"), "{said}");
    assert_eq!(choose(&accounts, Some(("8c21d07a44e1", Source::Option))).unwrap().label, "3f9a1c0e5b7d");

    let twice = [account("3f9a1c0e5b7d", "Personal", ""), account("8c21d07a44e1", "personal", "")];
    assert!(matches!(choose(&twice, Some(("PERSONAL", Source::Option))), Err(NoChoice::Ambiguous { .. })));
}

/// Named from the environment with no account at all: exit status 1, naming the variable.
#[test]
fn a_name_with_no_account_at_all_says_where_it_came_from() {
    let refused = choose(&[], Some(("Test", Source::Environment))).unwrap_err();
    assert_eq!(refused.exit_status(), 1);
    assert!(refused.to_string().starts_with("KONEDRIVE_ACCOUNT names the account \"Test\""), "{refused}");
}

/// A suggested command names the account whenever the bare one could act on another.
#[test]
fn a_suggested_command_names_the_account_when_it_has_to() {
    assert_eq!(command_prefix(Some("Family"), false, false), "konedrivectl");
    assert_eq!(command_prefix(Some("Family"), true, false), "konedrivectl --account Family");
    assert_eq!(command_prefix(Some("My Home"), false, true), "konedrivectl --account 'My Home'");
    assert_eq!(command_prefix(None, true, false), "konedrivectl --account <account>");
    let context = Context { prefix: "konedrivectl --account Family", ..Context::default() };
    let text = refusal_text_in(
        SyncAction::Register("/home/u/Other"),
        Some("org.konedrive.Error.AlreadyRegistered"),
        "",
        Context { root: "/home/u/Family", ..context },
    );
    assert!(text.contains("run `konedrivectl --account Family sync forget` first"), "{text}");
}

/// `account remove` names where the listed conflicts' files were rescued, and nothing it
/// cannot know.
#[test]
fn removal_says_where_the_listed_rescues_are() {
    let rescued = |original: &str, kept: &str| (0, original.to_owned(), kept.to_owned(), "rescued".to_owned());
    let conflicts = [
        rescued("/home/u/OneDrive/docs/a.txt", "/data/rescued/id/t1/docs/a.txt"),
        rescued("/home/u/OneDrive/b.txt", "/home/u/.konedrive-rescued-OneDrive/t2/b.txt"),
        rescued("/home/u/OneDrive/c.txt", "/data/rescued/id/t1/c.txt"),
        (0, "/home/u/OneDrive/d.txt".to_owned(), "/home/u/OneDrive/d-fedora.txt".to_owned(), "copy".to_owned()),
    ];
    let dirs = rescue_dirs("/home/u/OneDrive", &conflicts);
    assert_eq!(dirs, ["/data/rescued/id/t1", "/home/u/.konedrive-rescued-OneDrive/t2"], "a copy stays in the folder");
    let listed = super::conflicts_text(&conflicts);
    assert!(listed.contains("moved to /data/rescued/id/t1/c.txt"), "{listed}");
    assert!(listed.contains("changed here and in OneDrive: yours is kept beside it as /home/u/OneDrive/d-fedora.txt"), "{listed}");
    let said = removed_text("Home", "", &[]);
    assert!(said.contains("It had no folder.") && !said.contains("Rescued"), "{said}");
}

#[test]
fn a_label_is_quoted_for_the_shell_only_when_it_has_to_be() {
    assert_eq!(shell_word("Family"), "Family");
    assert_eq!(shell_word("Семья"), "Семья");
    assert_eq!(shell_word("Ann's work"), r"'Ann'\''s work'");
}

/// `Files` refuses a path in no account's folder `OutsideRoot`: with no
/// folder at all, that is said as `NoRoot` is; with folders, they are named.
#[test]
fn a_path_in_no_folder_is_told_which_folders_there_are() {
    let outside = |folders: &[String]| {
        let context = Context { folders, ..Context::default() };
        refusal_text_in(SyncAction::Hydrate("/tmp/x"), Some("org.konedrive.Error.OutsideRoot"), "", context)
    };
    assert!(outside(&[]).contains("no sync folder is registered"), "{}", outside(&[]));
    let two = ["/home/u/OneDrive".to_owned(), "/home/u/Family".to_owned()];
    let text = outside(&two);
    assert!(text.contains("/home/u/OneDrive, /home/u/Family"), "{text}");
}

/// Pins every `Skipped()` reason's sentence — the same wording as the
/// window's `whyText` (`app/synccontroller.cpp`), which is what keeps a
/// user reading the same explanation from `konedrivectl sync skipped`
/// and from the window regardless of which one they happen to use.
#[test]
fn skip_reason_text_matches_the_windows_wording() {
    assert_eq!(
        skip_reason_text("name-too-long"),
        "The name is longer than Linux allows (255 bytes; a Cyrillic letter takes two)."
    );
    assert_eq!(
        skip_reason_text("personal-vault"),
        "The Personal Vault is locked separately and is not synced."
    );
    assert_eq!(
        skip_reason_text("shared"),
        "A shared folder added to your OneDrive; shared folders are not synced yet."
    );
    assert_eq!(skip_reason_text("onenote"), "A OneNote notebook, which is not a file.");
    assert_eq!(
        skip_reason_text("reserved-name"),
        "The name begins with .konedrive-, which konedrive keeps for itself."
    );
    assert_eq!(
        skip_reason_text("unsupported"),
        "It is neither a file nor a folder konedrive can show."
    );
    assert_eq!(
        skip_reason_text("something-nobody-invented-yet"),
        "It is neither a file nor a folder konedrive can show."
    );
}

/// Being signed out gets its own sentence, distinct from a locked wallet
/// or a network error, which keep the daemon's own message: the fix for
/// each is different, so folding them into one generic sentence would
/// hide which one applies.
#[test]
fn dev_refusal_text_distinguishes_signed_out_from_other_failures() {
    let signed_out = dev_refusal_text(Some("org.konedrive.Error.NotSignedIn"), "nobody is signed in", "konedrivectl");
    assert!(signed_out.to_lowercase().contains("signed in"), "{signed_out}");

    let locked = dev_refusal_text(Some("org.konedrive.Error.Failed"), "secret storage is locked", "konedrivectl");
    assert!(locked.contains("secret storage is locked"), "{locked}");
    assert!(
        !locked.to_lowercase().contains("are you signed in"),
        "a locked wallet is not the same thing as being signed out: {locked}"
    );

    let network = dev_refusal_text(
        Some("org.konedrive.Error.Failed"),
        "Microsoft rejected the token refresh: invalid_client: bad request",
        "konedrivectl",
    );
    assert!(network.contains("Microsoft rejected the token refresh"), "{network}");

    // A name from outside `org.konedrive.Error` is not mistaken for
    // `NotSignedIn` just because the detail happens to mention signing in.
    let bus_error = dev_refusal_text(Some("org.freedesktop.DBus.Error.NoReply"), "no reply", "konedrivectl");
    assert!(!bus_error.to_lowercase().contains("are you signed in"), "{bus_error}");
}

#[test]
fn write_secret_atomically_creates_a_private_file() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("token");
    write_secret_atomically(&out, b"AT-1").unwrap();
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "AT-1");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&out).unwrap().permissions().mode() & 0o777, 0o600);
    // No temporary file left behind.
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n != "token")
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// The heart of I1: `rename(2)` replaces the symlink itself, so its
/// target is never opened, truncated, or written through.
#[test]
fn write_secret_atomically_replaces_a_symlink_without_touching_its_target() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("real-file");
    std::fs::write(&target, b"do not touch").unwrap();
    let link = dir.path().join("out-link");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    write_secret_atomically(&link, b"AT-2").unwrap();

    assert!(
        !std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(),
        "the link must be replaced by a regular file, not written through"
    );
    assert_eq!(std::fs::read_to_string(&link).unwrap(), "AT-2");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "do not touch", "the old target is untouched");
}

/// The other half of I1: an fd opened before the export keeps reading
/// the old inode's content — `rename(2)` never truncates it in place.
#[test]
fn write_secret_atomically_does_not_disturb_a_reader_of_the_old_file() {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("token");
    std::fs::write(&out, b"old-content").unwrap();
    std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut held_open = std::fs::File::open(&out).unwrap();

    write_secret_atomically(&out, b"AT-3").unwrap();

    assert_eq!(std::fs::read_to_string(&out).unwrap(), "AT-3");
    assert_eq!(std::fs::metadata(&out).unwrap().permissions().mode() & 0o777, 0o600);
    let mut still_reads = String::new();
    held_open.read_to_string(&mut still_reads).unwrap();
    assert_eq!(still_reads, "old-content", "an fd opened before the export keeps its own inode");
}

/// The name decides, never the message. A refusal named
/// `ModifiedLocally` whose message happens to read like `NotHydrated`'s
/// must still be explained as edits that would be lost — matching on
/// the prose is exactly what a named error exists to replace.
#[test]
fn a_refusal_is_explained_by_its_name_not_its_message() {
    let text = refusal_text(
        SyncAction::Dehydrate("/r/doc.bin"),
        Some("org.konedrive.Error.ModifiedLocally"),
        "the file is not downloaded",
        "/r",
    );
    assert!(text.contains("lose your edits"), "{text}");
    assert!(!text.contains("no space to free"), "{text}");

    let text = refusal_text(
        SyncAction::Dehydrate("/r/doc.bin"),
        Some("org.konedrive.Error.NotHydrated"),
        "the file was modified locally",
        "/r",
    );
    assert!(text.contains("no space to free"), "{text}");
    assert!(!text.contains("lose your edits"), "{text}");
}

/// A name from outside `org.konedrive.Error` — the bus's own, say — is
/// not mistaken for one of ours even when its last component matches.
#[test]
fn only_names_under_the_konedrive_prefix_are_ours() {
    let text = refusal_text(
        SyncAction::Hydrate("/r/doc.bin"),
        Some("org.freedesktop.DBus.Error.InUse"),
        "something else entirely",
        "/r",
    );
    assert_eq!(text, "downloading /r/doc.bin failed: something else entirely");
}

/// A Forget of a folder registered with the helper now
/// needs the helper, and is refused `NoHelper` without one. The generic
/// `NoHelper` text was written for registering — "… and  was not
/// registered … `register-without-interception `" with an empty path —
/// which reads as nonsense after `sync forget`, and says nothing about
/// the folder still being registered.
#[test]
fn a_forget_refused_for_want_of_the_helper_says_the_folder_is_still_registered() {
    let text = refusal_text(
        SyncAction::Forget,
        Some("org.konedrive.Error.NoHelper"),
        "the konedrive helper is not connected",
        "/home/u/OneDrive",
    );
    assert!(text.contains("/home/u/OneDrive"), "{text}");
    assert!(text.contains("still registered"), "{text}");
    assert!(text.contains("once the helper is back"), "{text}");
    assert!(!text.contains("was not registered"), "{text}");
    assert!(!text.contains("register-without-interception"), "{text}");
}

/// follow-up: `Hydrate` of a file that may carry an ignore
/// mark — one a cancelled "free up space" left half done, or one labelled
/// downloaded with nothing to prove it — needs the helper to clear that
/// mark first, and is refused `NoHelper` without one. The generic text
/// talks about registering a folder.
#[test]
fn a_download_refused_for_want_of_the_helper_says_nothing_changed() {
    let text = refusal_text(
        SyncAction::Hydrate("/home/u/OneDrive/doc.bin"),
        Some("org.konedrive.Error.NoHelper"),
        "the konedrive helper is not connected",
        "/home/u/OneDrive",
    );
    assert!(text.contains("/home/u/OneDrive/doc.bin"), "{text}");
    assert!(text.contains("nothing was changed"), "{text}");
    assert!(text.contains("once the helper is back"), "{text}");
    assert!(!text.contains("was not registered"), "{text}");
}

/// a populate source that overlaps the sync
/// folder is refused `Unsupported`, whose text was written for a folder
/// that cannot be registered.
#[test]
fn a_populate_source_refused_as_unsupported_is_not_called_a_sync_folder() {
    let text = refusal_text(
        SyncAction::PopulateFrom("/home/u/OneDrive/src"),
        Some("org.konedrive.Error.Unsupported"),
        "/home/u/OneDrive/src is inside the sync folder /home/u/OneDrive, and a folder \
         cannot be filled from itself",
        "/home/u/OneDrive",
    );
    assert!(!text.contains("cannot be used as the sync folder"), "{text}");
    assert!(text.contains("cannot be filled from itself"), "{text}");
}

/// The same folder asked for again — which is what a restored folder
/// waiting for its helper now answers `AlreadyRegistered` to — is not
/// "use this one instead".
#[test]
fn registering_the_folder_that_is_already_registered_says_so() {
    let text = refusal_text(
        SyncAction::RegisterWithoutInterception("/home/u/OneDrive"),
        Some("org.konedrive.Error.AlreadyRegistered"),
        "a sync root is already registered; forget it first",
        "/home/u/OneDrive",
    );
    assert!(text.contains("already the sync folder"), "{text}");
    assert!(!text.contains("instead"), "{text}");

    let text = refusal_text(
        SyncAction::RegisterWithoutInterception("/home/u/Other"),
        Some("org.konedrive.Error.AlreadyRegistered"),
        "a sync root is already registered; forget it first",
        "/home/u/OneDrive",
    );
    assert!(text.contains("To use /home/u/Other instead"), "{text}");
}

#[test]
fn durations_read_as_sync_pause_takes_them() {
    assert_eq!(parse_duration("90"), Some(90));
    assert_eq!(parse_duration("30m"), Some(1800));
    assert_eq!(parse_duration("2h"), Some(7200));
    assert_eq!(parse_duration("1d"), Some(86_400));
    assert_eq!(parse_duration("1h30m"), Some(5400));
    for bad in ["", "0", "soon", "2x", "h", "30m5", "999999999999"] {
        assert_eq!(parse_duration(bad), None, "{bad:?}");
    }
}

/// `sync open` refused `Unreachable`: the sentence, then the daemon's own
/// message when there is one — the cause is not always the network.
#[test]
fn unreachable_is_followed_by_the_daemons_cause() {
    let name = format!("{}.Unreachable", konedrive_dbus::ERROR_PREFIX);
    let told = |detail: &str| refusal_text(SyncAction::Open("/f/a.txt"), Some(&name), detail, "/f");
    assert_eq!(told("the secret storage is locked"), "OneDrive could not be reached: the secret storage is locked");
    assert_eq!(told(""), "OneDrive could not be reached");
    assert_eq!(told(&name), "OneDrive could not be reached", "an error with no message");
}

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

/// the outbox on the bus: a `FreeUp` of several paths refused `NotUploaded` names the
/// one path that has a change waiting, not all of them.
#[test]
fn a_free_up_refused_not_uploaded_names_the_one_path() {
    let detail = "/f/B/y is not uploaded yet, so freeing it up would lose the changes made here";
    assert_eq!(super::refused_path_of("org.konedrive.Error.NotUploaded", detail), Some("/f/B/y"));
    assert_eq!(super::refused_path_of("org.konedrive.Error.Failed", detail), None);
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

#[test]
fn the_local_scan_line_says_how_far_it_got_or_when_it_last_finished() {
    let now = 1_000_000;
    let running = super::LocalScan {
        state: "running".into(),
        reason: "read-write".into(),
        started: now - 130,
        directories: 1_234,
        files: 45_678,
        expected: 50_000,
        ..Default::default()
    };
    assert_eq!(
        super::local_scan_text(&running, now),
        "running — 1 234 folders and 45 678 files, of about 50 000 (2 min, after the switch to read-write)"
    );
    let idle = super::LocalScan { state: "idle".into(), finished: now - 300, took: 40, ..running.clone() };
    assert_eq!(super::local_scan_text(&idle, now), "last finished 5 min ago (took 40 s)");
    let never = super::LocalScan { state: "idle".into(), ..Default::default() };
    assert_eq!(super::local_scan_text(&never, now), "not yet since the daemon started");
    let none = super::LocalScan { state: "none".into(), ..Default::default() };
    assert_eq!(super::local_scan_text(&none, now), "none — read-only");
    assert_eq!(super::grouped(999), "999");
    assert_eq!(super::grouped(1_000_000), "1 000 000");
    assert_eq!(super::seconds_text(3_900), "1 h 5 min");
}

#[test]
fn human_bytes_uses_binary_units() {
    assert_eq!(human_bytes(0), "0 B");
    assert_eq!(human_bytes(1023), "1023 B");
    assert_eq!(human_bytes(1536), "1.5 KiB");
    assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
}
