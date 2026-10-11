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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_register_while_signed_out_says_to_log_in() {
    let f = harness_signed_out().await;
    let addr = f._bus.address();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();

    let told = refused(addr, &["sync", "register", root.to_str().unwrap()]);
    assert!(told.contains("konedrivectl login"), "{told}");
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
async fn binary_register_of_a_non_empty_folder_says_to_choose_an_empty_one() {
    // Refused before anything is mounted.
    let (f, _graph) = harness_onedrive().await;
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
    // Refused before anything is mounted.
    let (f, _graph) = harness_onedrive().await;
    let root = f.dir.path().join("Theirs");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("x"), b"x").unwrap();
    xattr::set(&root, "user.konedrive.drive", b"D-OTHER").unwrap();

    let told = refused(f._bus.address(), &["sync", "register", root.to_str().unwrap()]);
    assert!(told.contains("holds the files of another OneDrive account's folder"), "{told}");
    assert!(told.contains("sign the account in first (`konedrivectl login`)"), "{told}");
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
    // Refused before anything is mounted.
    let (f, _graph) = harness_onedrive().await;
    let addr = f._bus.address();
    let file = f.dir.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();

    let told = refused(addr, &["sync", "register", file.to_str().unwrap()]);
    assert!(told.contains(file.to_str().unwrap()), "{told}");
    assert!(told.contains("cannot be used as the sync folder"), "{told}");
    assert!(told.contains("not a directory"), "the daemon's specific reason stays: {told}");
}

/// The daemon answers `NoRoot` to `UploadQueue.Changes`, `Folder.Pause`, `Resume` and the
/// like in two cases: no folder is registered at all, and the folder's sync has not opened
/// its store yet. Only the second is told to try again in a moment; with no folder, these
/// commands say what every other one says: no folder is registered, and how to register one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_an_upload_queue_command_with_no_folder_says_no_folder_is_registered() {
    let f = harness().await;
    let addr = f._bus.address();

    for command in ["upload-queue", "pause"] {
        let told = refused(addr, &["sync", command]);
        assert!(told.contains("no sync folder is registered"), "sync {command}: {told}");
        assert!(told.contains("konedrivectl sync register"), "sync {command}: {told}");
    }
}
