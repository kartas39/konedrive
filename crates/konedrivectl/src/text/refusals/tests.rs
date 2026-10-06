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
/// name (`docs/design/desktop.md` §2.6): uploads waiting, a read-only account, and, for the token
/// alone, a drive `write_test_drive_ids` does not list.
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
    let unlisted = dev_refusal_text(Some("org.konedrive.Error.WritesNotAllowed"), "x", prefix);
    assert!(unlisted.contains("write_test_drive_ids") && unlisted.contains("Without --read-write"), "{unlisted}");
    assert!(!unlisted.contains("can be read-write"), "the list is about the token only: {unlisted}");
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
            "the sync folder (/home/u/OneDrive) is not up, so the command could not go ahead: it waits for the konedrive \
             helper. `konedrivectl --account Test sync status` shows its state; `konedrivectl --account Test sync forget` \
             takes the folder away, and leaves its files as they are"
        );
    }
    // A `Refresh` that tried to bring the folder up says so.
    let tried = told(SyncAction::Refresh, "org.konedrive.Error.NotUp", "the folder is not up: bringing it up was tried just now and failed: errno 5");
    assert!(tried.contains("could not go ahead: bringing it up was tried just now and failed: errno 5."), "{tried}");
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
    assert_eq!(told(""), "OneDrive could not be reached.");
    assert_eq!(told(&name), "OneDrive could not be reached.", "an error with no message");
}

fn named(refusal: &str) -> String {
    format!("{}.{refusal}", konedrive_dbus::ERROR_PREFIX)
}

/// `sync pin`, `unpin`, `free` and `open` say the catalogue's sentences (`konedrive-text`),
/// as they are written there: the one Dolphin shows where there is one for both, with the
/// path where Dolphin has the file's name, and the command line's own where it names a
/// command or the folder.
#[test]
fn a_file_operation_is_told_in_the_catalogues_words() {
    let told = |action, refusal: &str, detail: &str| {
        let context = Context { root: "/r", prefix: "konedrivectl --account Test", ..Context::default() };
        refusal_text_in(action, Some(&named(refusal)), detail, context)
    };
    // One sentence for Dolphin and the command line.
    assert_eq!(
        told(SyncAction::Pin("/r/own.txt"), "NotManaged", "x"),
        "“/r/own.txt” is not a OneDrive file: it is a file of your own in the sync folder, so there is nothing for KOneDrive to keep downloaded."
    );
    assert_eq!(
        told(SyncAction::Unpin("/r/own.txt"), "NotManaged", "x"),
        "“/r/own.txt” is not a OneDrive file: it is a file of your own in the sync folder, so it was never pinned."
    );
    assert_eq!(
        told(SyncAction::Free("/r/a.bin"), "InUse", "x"),
        "“/r/a.bin” is open in another program, so its space cannot be freed right now. Close it there and try again."
    );
    assert_eq!(told(SyncAction::Free("/r/a.bin"), "NotHydrated", "x"), "“/r/a.bin” is not downloaded, so there is no space to free — it already takes none.");
    assert_eq!(
        told(SyncAction::Open("/r/own.txt"), "NotManaged", "x"),
        "“/r/own.txt” is not a OneDrive file: it is a file of your own in the sync folder, so it has no page in OneDrive."
    );
    // A refusal with no sentence of its own: the daemon's message, kept whole.
    assert_eq!(told(SyncAction::Pin("/r/a.bin"), "Failed", "disk full"), "Keeping “/r/a.bin” on this device failed: disk full");
    assert_eq!(told(SyncAction::Unpin("/r/a.bin"), "Failed", "disk full"), "Unpinning “/r/a.bin” failed: disk full");
    assert_eq!(told(SyncAction::Free("/r/a.bin"), "SomethingNew", "disk full"), "Freeing up “/r/a.bin” failed: disk full");
    assert_eq!(told(SyncAction::Open("/r/a.bin"), "Failed", "gone"), "Opening “/r/a.bin” in OneDrive failed: gone");
    // The command line's own: the command for this account, and the folder.
    assert_eq!(
        told(SyncAction::Open("/r/new.txt"), "NotUploaded", "x"),
        "/r/new.txt is not uploaded yet, so it has no page in OneDrive. It can be opened there once it is uploaded (`konedrivectl --account Test sync outbox`)."
    );
    assert_eq!(
        told(SyncAction::Open("/r/a.bin"), "NotSignedIn", "x"),
        "The account is not signed in, so OneDrive cannot be asked for the page of /r/a.bin. Sign in with `konedrivectl --account Test login` and try again."
    );
    assert_eq!(
        told(SyncAction::Free("/r/new.txt"), "NotUploaded", "/r/new.txt is not uploaded yet, so freeing it up would lose the changes made here"),
        "/r/new.txt is not uploaded yet, so freeing up its space would lose the changes made here. It was left as it is; its space can be freed once it is \
         uploaded (`konedrivectl --account Test sync outbox`)."
    );
    assert_eq!(
        told(SyncAction::Pin("/r/link"), "OutsideRoot", "x"),
        "/r/link: only files and folders inside the sync folder (/r) can be kept on this device or freed up — not symbolic links, or anything outside it."
    );
    assert_eq!(
        told(SyncAction::Open("/r/link"), "OutsideRoot", "x"),
        "/r/link: only files and folders inside the sync folder (/r), and the folder itself, have a page in OneDrive — not symbolic links, or anything outside it."
    );
    assert_eq!(
        told(SyncAction::Pin("/r/a.bin"), "ModifiedLocally", "x"),
        "“/r/a.bin” was changed here and has not been uploaded, so freeing its space would lose your edits. It was left exactly as it is."
    );
}

/// Where the catalogue has no sentence for the command line, a file operation is told as
/// this table tells its other commands: a folder above that keeps the file, no folder, no
/// helper.
#[test]
fn a_file_operation_the_catalogue_leaves_to_the_command_line_keeps_its_own_words() {
    let told = |action, refusal: &str, detail: &str| refusal_text(action, Some(&named(refusal)), detail, "/r");
    let pinned = "/r/Docs/a.bin is pinned by /r/Docs: unpin it first";
    assert!(told(SyncAction::Unpin("/r/Docs/a.bin"), "NotAllowed", pinned).ends_with("`konedrivectl sync unpin /r/Docs` stops keeping the folder"));
    assert!(told(SyncAction::Free("/r/Docs/a.bin"), "NotAllowed", pinned).contains("free up the folder first: `konedrivectl sync free /r/Docs`"));
    assert!(told(SyncAction::Pin("/r/a.bin"), "NoRoot", "x").starts_with("no sync folder is registered. Register one first"));
    assert!(told(SyncAction::Pin("/r/a.bin"), "NoHelper", "x").contains("so /r/a.bin was not registered"));
}

/// `sync free` without the helper: several paths may have been given and some freed, so
/// the command line has its own sentence, and how to start the helper follows it.
#[test]
fn a_free_up_without_the_helper_says_nothing_more_was_freed() {
    let context = Context { root: "/r", helper: "stopped", ..Context::default() };
    let text = refusal_text_in(SyncAction::Free("/r/a.bin"), Some(&named("NoHelper")), "x", context);
    assert!(text.starts_with("The konedrive helper is not connected, so nothing more was freed up. Freeing up a file must first"), "{text}");
    assert!(
        text.ends_with("it reconnects on its own. The konedrive helper is not running: start it with `sudo systemctl start konedrive-helper`"),
        "{text}"
    );
    assert_eq!(refusal_text(SyncAction::FreeUpSpace, Some(&named("NoHelper")), "x", "/r"), refusal_text(SyncAction::Free("/r/a.bin"), Some(&named("NoHelper")), "x", "/r"));
}

/// `sync pin`, `unpin` and `free` take several paths. A sentence about one file is said of
/// the one path given; of several, none of which the refusal names, the list form is said,
/// the paths in no quotes.
#[test]
fn several_paths_are_never_put_inside_one_files_quotes() {
    let told = |action, refusal: &str, several| {
        let context = Context { root: "/r", prefix: "konedrivectl --account Test", several, ..Context::default() };
        refusal_text_in(action, Some(&named(refusal)), "disk full", context)
    };
    let two = "/r/a, /r/own.txt";
    let not_managed = "one of these is not a OneDrive file but a file of your own in the sync folder";
    let listed = [
        (SyncAction::Pin(two), "NotManaged", format!("{two}: {not_managed}, so there is nothing for KOneDrive to keep downloaded.")),
        (SyncAction::Unpin(two), "NotManaged", format!("{two}: {not_managed}, so it was never pinned.")),
        (SyncAction::Free(two), "NotManaged", format!("{two}: {not_managed}, and KOneDrive never frees the space of a file it could not download again.")),
        (SyncAction::Free(two), "InUse", format!("{two}: one of these is open in another program, so its space cannot be freed right now. Close it there and try again.")),
        (SyncAction::Free(two), "NotHydrated", format!("{two}: one of these is not downloaded, so there is no space to free — it already takes none.")),
        (
            SyncAction::Pin(two),
            "ModifiedLocally",
            format!("{two}: one of these was changed here and has not been uploaded, so freeing its space would lose your edits. It was left exactly as it is."),
        ),
        (
            SyncAction::Free(two),
            "ModifiedLocally",
            format!("{two}: one of these was changed here and has not been uploaded, so freeing its space would lose your edits. It was left exactly as it is."),
        ),
        (SyncAction::Pin(two), "Failed", format!("Keeping {two} on this device failed: disk full")),
        (SyncAction::Unpin(two), "Failed", format!("Unpinning {two} failed: disk full")),
        (SyncAction::Free(two), "SomethingNew", format!("Freeing up {two} failed: disk full")),
    ];
    for (action, refusal, text) in listed {
        assert_eq!(told(action, refusal, true), text);
    }
    // No name at all: the daemon's words are all there is.
    let context = Context { several: true, ..Context::default() };
    assert_eq!(refusal_text_in(SyncAction::Free(two), None, "disk full", context), format!("Freeing up {two} failed: disk full"));
    // One path, or the one the refusal names: the sentence about one file.
    let one = "/r/own.txt";
    let about_one = [
        (SyncAction::Pin(one), "NotManaged", "“/r/own.txt” is not a OneDrive file: it is a file of your own in the sync folder, so there is nothing for KOneDrive to keep downloaded."),
        (SyncAction::Free(one), "NotManaged", "“/r/own.txt” is not a OneDrive file: it is a file of your own in the sync folder, and KOneDrive never frees the space of a file it could not download again."),
        (SyncAction::Free(one), "InUse", "“/r/own.txt” is open in another program, so its space cannot be freed right now. Close it there and try again."),
        (SyncAction::Free(one), "NotHydrated", "“/r/own.txt” is not downloaded, so there is no space to free — it already takes none."),
        (SyncAction::Unpin(one), "ModifiedLocally", "“/r/own.txt” was changed here and has not been uploaded, so freeing its space would lose your edits. It was left exactly as it is."),
        (SyncAction::Unpin(one), "Failed", "Unpinning “/r/own.txt” failed: disk full"),
    ];
    for (action, refusal, text) in about_one {
        assert_eq!(told(action, refusal, false), text);
    }
    // A sentence with no file in quotes is said of the list as it is.
    assert_eq!(
        told(SyncAction::Pin(two), "OutsideRoot", true),
        format!("{two}: only files and folders inside the sync folder (/r) can be kept on this device or freed up — not symbolic links, or anything outside it.")
    );
}
