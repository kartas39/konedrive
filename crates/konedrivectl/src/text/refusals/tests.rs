use super::{account_refusal_text, dev_refusal_text, refusal_text, refusal_text_in, AccountAction, Context, SyncAction};

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

/// An outbox command refused for want of a folder is told to register one; refused because
/// the folder is not up, or its sync is not running, it is told why, in the daemon's words,
/// and where to look.
#[test]
fn an_outbox_command_is_told_to_register_only_when_there_is_no_folder() {
    let told = |action, name: &str, detail| {
        refusal_text_in(action, Some(name), detail, Context { root: "/home/u/OneDrive", prefix: "konedrivectl --account Test", ..Context::default() })
    };
    for action in [SyncAction::Outbox, SyncAction::Pause, SyncAction::Refresh] {
        let none = told(action, "org.konedrive.Error.NoRoot", "no sync root is registered");
        assert!(none.contains("no sync folder is registered") && none.contains("sync register"), "{none}");
        let waiting = told(action, "org.konedrive.Error.NotUp", "the folder is not up: it waits for the konedrive helper");
        assert_eq!(
            waiting,
            "nothing was done for the sync folder (/home/u/OneDrive): the folder is not up: it waits for the konedrive \
             helper. `konedrivectl --account Test sync status` shows its state; `konedrivectl --account Test sync forget` \
             takes the folder away, and leaves its files as they are"
        );
    }
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
