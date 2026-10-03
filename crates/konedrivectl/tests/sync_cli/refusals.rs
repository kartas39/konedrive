use super::*;

// --- Named refusals, explained ------------------------------------------
//
// Every refusal the folder can make arrives as its own D-Bus error name under
// `konedrive_dbus::ERROR_PREFIX`. Until these tests, `konedrivectl` read
// none of them: every refusal reached the terminal as anyhow's rendering of
// the raw `zbus::Error` — `Error: org.konedrive.Error.ModifiedLocally: the
// file was modified locally` — which is the daemon's message with a D-Bus
// name in front of it. Each test below drives one refusal through the real
// binary and asserts on what a person is told: what happened to *their*
// file, and what to do next. None of the phrases asserted on appears in the
// daemon's own message, so echoing that message cannot pass.

/// A registered root, populated from a source directory holding one
/// 8 KiB file, `doc.bin`. Returns the placeholder's path.
async fn populated(f: &Harness) -> PathBuf {
    let source = f.dir.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("doc.bin"), vec![7u8; 8192]).unwrap();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();
    f.proxy.folder.populate_from_directory(source.to_str().unwrap()).await.unwrap();
    root.join("doc.bin")
}

/// `ModifiedLocally` from `dehydrate`: the one refusal whose whole point is
/// that the alternative loses the user's work.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_dehydrate_of_a_locally_modified_file_says_the_edits_would_be_lost() {
    let f = harness().await;
    let addr = f._bus.address();
    let file = populated(&f).await;
    f.files.hydrate(file.to_str().unwrap()).await.unwrap();
    std::fs::write(&file, b"what the user typed").unwrap();

    let told = refused(addr, &["sync", "dehydrate", file.to_str().unwrap()]);
    assert!(told.contains(file.to_str().unwrap()), "name the file: {told}");
    assert!(told.contains("has not been uploaded"), "{told}");
    assert!(told.contains("lose your edits"), "{told}");
    assert_eq!(std::fs::read(&file).unwrap(), b"what the user typed");
}

/// `ModifiedLocally` from `hydrate`: the same refusal pointing the other
/// way — downloading would overwrite the edit rather than free it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_hydrate_of_a_locally_modified_file_says_the_edits_would_be_overwritten() {
    let f = harness().await;
    let addr = f._bus.address();
    let file = populated(&f).await;
    f.files.hydrate(file.to_str().unwrap()).await.unwrap();
    std::fs::write(&file, b"what the user typed").unwrap();

    let told = refused(addr, &["sync", "hydrate", file.to_str().unwrap()]);
    assert!(told.contains("has not been uploaded"), "{told}");
    assert!(told.contains("overwrite your edits"), "{told}");
    assert!(!told.contains("lose your edits"), "this is the hydrate wording, not dehydrate's: {told}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_dehydrate_of_an_online_only_file_says_there_is_nothing_to_free() {
    let f = harness().await;
    let addr = f._bus.address();
    let file = populated(&f).await;

    let told = refused(addr, &["sync", "dehydrate", file.to_str().unwrap()]);
    assert!(told.contains("no space to free"), "{told}");
    assert!(!told.contains("has not been uploaded"), "NotHydrated is not ModifiedLocally: {told}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_dehydrate_of_an_open_file_says_to_close_it() {
    let f = harness().await;
    let addr = f._bus.address();
    let file = populated(&f).await;
    f.files.hydrate(file.to_str().unwrap()).await.unwrap();
    let _held_open = std::fs::File::open(&file).unwrap();

    let told = refused(addr, &["sync", "dehydrate", file.to_str().unwrap()]);
    assert!(told.contains("open in another program"), "{told}");
    assert!(told.contains("Close it"), "{told}");
}

/// `NoHelper` from `register`: the refusal a user without the helper meets
/// first, so it has to name the way forward — starting the helper, in the
/// words `HelperState` gives (HS4). The mode without interception is named
/// only as the developer's, with its cost: it never shows OneDrive (HS2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_register_without_a_helper_says_how_to_start_it() {
    let f = harness_with_helper(false).await;
    let addr = f._bus.address();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();

    let told = refused(addr, &["sync", "register", root.to_str().unwrap()]);
    assert!(told.contains("was not registered"), "{told}");
    assert!(told.contains("The konedrive helper is not connected"), "the helper's advice closes it: {told}");
    assert!(told.contains("developer's mode"), "{told}");
    assert!(told.contains("zeros"), "the developer's mode's cost is said with it: {told}");

    let status = out_text(&run(addr, &["sync", "status"]));
    let helper = status.lines().find(|l| l.starts_with("Helper:")).unwrap_or_else(|| panic!("{status}"));
    assert_eq!(helper, "Helper:                 unknown — the konedrive helper is not connected", "{status}");
}

/// `NoHelper` from `dehydrate`: a root registered *with* interception whose
/// helper has since gone away. Freeing space then has to be refused (the
/// helper must resume watching the file first), and the file is untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_dehydrate_without_the_helper_changes_nothing_and_says_why() {
    let f = harness().await;
    let addr = f._bus.address();
    let file = populated(&f).await;
    f.files.hydrate(file.to_str().unwrap()).await.unwrap();
    f.service.set_link(None);

    let told = refused(addr, &["sync", "dehydrate", file.to_str().unwrap()]);
    assert!(told.contains("helper is not connected"), "{told}");
    assert!(told.contains("nothing was changed"), "{told}");
    assert!(
        !told.contains("register-without-interception"),
        "this is the dehydrate wording, not register's: {told}"
    );
    assert_eq!(std::fs::read(&file).unwrap(), vec![7u8; 8192]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_register_while_signed_out_says_to_log_in() {
    let f = harness_signed_out().await;
    let addr = f._bus.address();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();

    let told = refused(addr, &["sync", "register", root.to_str().unwrap()]);
    assert!(told.contains("konedrivectl login"), "{told}");
    assert!(told.contains("register-without-interception"), "{told}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_a_second_register_names_the_folder_already_registered() {
    let f = harness().await;
    let addr = f._bus.address();
    let first = f.dir.path().join("OneDrive");
    std::fs::create_dir(&first).unwrap();
    f.proxy.folder.register(first.to_str().unwrap()).await.unwrap();
    let second = f.dir.path().join("Another");
    std::fs::create_dir(&second).unwrap();

    let told = refused(addr, &["sync", "register", second.to_str().unwrap()]);
    assert!(told.contains(first.to_str().unwrap()), "name the folder that is in the way: {told}");
    assert!(told.contains("konedrivectl sync forget"), "{told}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_a_file_command_with_no_folder_says_how_to_register_one() {
    let f = harness().await;
    let addr = f._bus.address();
    let file = f.dir.path().join("doc.bin");
    std::fs::write(&file, b"x").unwrap();

    let told = refused(addr, &["sync", "hydrate", file.to_str().unwrap()]);
    assert!(told.contains("no sync folder is registered"), "{told}");
    assert!(told.contains("konedrivectl sync register"), "{told}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_hydrate_with_no_source_says_to_populate_first() {
    let f = harness().await;
    let addr = f._bus.address();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();
    let file = root.join("doc.bin");
    std::fs::write(&file, b"x").unwrap();

    let told = refused(addr, &["sync", "hydrate", file.to_str().unwrap()]);
    assert!(told.contains("konedrivectl sync populate-from"), "{told}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_a_file_outside_the_folder_is_named_as_outside_it() {
    let f = harness().await;
    let addr = f._bus.address();
    populated(&f).await;
    let outside = f.dir.path().join("elsewhere.bin");
    std::fs::write(&outside, b"not ours").unwrap();

    let told = refused(addr, &["sync", "hydrate", outside.to_str().unwrap()]);
    assert!(told.contains(outside.to_str().unwrap()), "{told}");
    let root = f.dir.path().join("OneDrive");
    assert!(told.contains(&format!("is not inside the sync folder ({})", root.display())), "{told}");
    assert_eq!(std::fs::read(&outside).unwrap(), b"not ours");

    // Inside the folder, but not a file: the folder that holds it is named.
    let told = refused(addr, &["sync", "hydrate", root.to_str().unwrap()]);
    assert!(told.contains("not a regular file inside the sync folder"), "{told}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_a_file_of_the_users_own_is_named_as_theirs() {
    let f = harness().await;
    let addr = f._bus.address();
    let file = populated(&f).await;
    let stray = file.with_file_name("stray.txt");
    std::fs::write(&stray, b"mine").unwrap();

    let told = refused(addr, &["sync", "hydrate", stray.to_str().unwrap()]);
    assert!(told.contains(stray.to_str().unwrap()), "{told}");
    assert!(told.contains("a file of your own"), "{told}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_register_of_a_non_empty_folder_says_to_choose_an_empty_one() {
    let f = harness().await;
    let addr = f._bus.address();
    let root = f.dir.path().join("NotEmpty");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("x"), b"x").unwrap();

    let told = refused(addr, &["sync", "register", root.to_str().unwrap()]);
    assert!(told.contains(root.to_str().unwrap()), "{told}");
    assert!(told.contains("choose an empty folder"), "{told}");
    assert!(!told.contains("another OneDrive account"), "{told}");
}

/// A folder with files in it that carries another drive (`user.konedrive.drive`) is
/// refused `NotEmpty` too, and said to be another account's, with the way back for this
/// account's own earlier folder: signing in first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_register_of_another_accounts_folder_says_whose_it_is() {
    let f = harness().await;
    let root = f.dir.path().join("Theirs");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("x"), b"x").unwrap();
    xattr::set(&root, "user.konedrive.drive", b"D-OTHER").unwrap();

    let told = refused(f._bus.address(), &["sync", "register", root.to_str().unwrap()]);
    assert!(told.contains("holds the files of another OneDrive account's folder"), "{told}");
    assert!(told.contains("sign the account in first (`konedrivectl login`)"), "{told}");
}

/// `account remove` refused for want of the helper changes nothing, and says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_remove_without_the_helper_changes_nothing() {
    let f = harness().await;
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    f.proxy.folder.register(root.to_str().unwrap()).await.unwrap();
    f.service.set_link(None);

    let told = refused(f._bus.address(), &["account", "remove", "Personal"]);
    assert!(told.contains("the account Personal was not removed and nothing was changed"), "{told}");
    let list = out_text(&run(f._bus.address(), &["account", "list"]));
    assert!(list.contains("Personal") && list.contains(root.to_str().unwrap()), "{list}");
}

/// The client ID cannot change while an account uses it; the refusal names the account and
/// how to sign it out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_set_client_id_while_signed_in_names_the_account() {
    let f = harness().await;
    let told = refused(f._bus.address(), &["set-client-id", "0f8fad5b-d9cb-469f-a165-70867728950e"]);
    assert!(told.contains("while Personal is signed in or signing in"), "{told}");
    assert!(told.contains("`konedrivectl --account Personal logout`"), "{told}");
}

/// `Unsupported` carries the daemon's specific reason (which feature is
/// missing, or that the path is not a directory at all); that detail is
/// kept, framed by what it means for the folder the user typed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_register_of_a_file_says_it_cannot_be_the_sync_folder_and_why() {
    let f = harness().await;
    let addr = f._bus.address();
    let file = f.dir.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();

    let told = refused(addr, &["sync", "register", file.to_str().unwrap()]);
    assert!(told.contains(file.to_str().unwrap()), "{told}");
    assert!(told.contains("cannot be used as the sync folder"), "{told}");
    assert!(told.contains("not a directory"), "the daemon's specific reason stays: {told}");
}

/// `Failed` has no name of its own, so its detail is all there is — but the
/// person still has to be told which file it was about.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_an_io_failure_names_the_file_it_was_about() {
    let f = harness().await;
    let addr = f._bus.address();
    let file = populated(&f).await;
    std::fs::remove_file(f.dir.path().join("source/doc.bin")).unwrap();

    let told = refused(addr, &["sync", "hydrate", file.to_str().unwrap()]);
    assert!(told.contains(&format!("downloading {} failed", file.display())), "{told}");
    assert!(told.contains("Input/output error"), "the cause stays: {told}");
}

/// Found while checking finding CL2. The daemon answers `NoRoot` to `UploadQueue.Changes`,
/// `Folder.Pause`, `Resume` and the like in two cases: no folder is registered at all, and
/// the folder's sync has not opened its store yet. The CLI has one sentence for both, the
/// second case's, so with no folder it says to try again in a moment — which never helps —
/// where every other command says that no folder is registered and how to register one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "shows CL2: with no folder registered, `sync outbox` and `sync pause` say to try again in a moment"]
async fn binary_an_outbox_command_with_no_folder_says_no_folder_is_registered() {
    let f = harness().await;
    let addr = f._bus.address();

    for command in ["outbox", "pause"] {
        let told = refused(addr, &["sync", command]);
        assert!(told.contains("no sync folder is registered"), "sync {command}: {told}");
        assert!(told.contains("konedrivectl sync register"), "sync {command}: {told}");
    }
}
