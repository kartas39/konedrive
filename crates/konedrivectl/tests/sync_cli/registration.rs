use super::*;

// --- I1: the compiled binary, not just the library it is built from ------
//
// Every test above calls `konedrivectl::sync_status_text` or the raw proxy
// directly. Nothing exercised `main.rs` itself: argument parsing, the seven
// (now eight) `sync` subcommands' own success strings, or `absolute_str`'s
// error path. `TestBus::address()` makes that reachable without a session
// bus: point `DBUS_SESSION_BUS_ADDRESS` at the private one and run the real
// binary, exactly as a user's shell would (`common::run`).

/// Drives the whole offline workflow through the binary: register, populate,
/// hydrate, dehydrate, forget — checking both the success strings `main.rs`
/// prints and the *effect* of each command (via `sync state`), not just its
/// exit code. Checking only exit codes would miss the `Hydrate`/`Dehydrate`
/// arms being swapped, since both are `Ok(())` either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_drives_registration_populate_hydrate_and_dehydrate() {
    let f = harness().await;
    let addr = f._bus.address();

    let out = run(addr, &["sync", "status"]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).contains("(none)"), "{}", out_text(&out));

    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    let out = run(addr, &["sync", "register", root.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        out_text(&out).contains(&format!("Folder registered: {}", root.display())),
        "{}",
        out_text(&out)
    );

    let source = f.dir.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("note.txt"), b"hello").unwrap();
    let out = run(addr, &["sync", "populate-from", source.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).contains("Created 1 placeholders."), "{}", out_text(&out));

    let file = root.join("note.txt");
    let out = run(addr, &["sync", "state", file.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out_text(&out).trim(), "online-only", "{}", out_text(&out));

    let out = run(addr, &["sync", "hydrate", file.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).contains("Downloaded."), "{}", out_text(&out));

    let out = run(addr, &["sync", "state", file.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        out_text(&out).trim(),
        "hydrated",
        "swapping the Hydrate/Dehydrate arms would leave this online-only: {}",
        out_text(&out)
    );

    let out = run(addr, &["sync", "dehydrate", file.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).contains("Freed up."), "{}", out_text(&out));

    let out = run(addr, &["sync", "state", file.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        out_text(&out).trim(),
        "online-only",
        "swapping the Hydrate/Dehydrate arms would leave this hydrated: {}",
        out_text(&out)
    );

    let out = run(addr, &["sync", "forget"]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).contains("Folder forgotten."), "{}", out_text(&out));
}

/// I2's whole point, proven at the binary level: `register-without-
/// interception` needs no helper connection at all, and says plainly, every
/// time, that files in the folder read as zeros until hydrated by hand.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_register_without_interception_needs_no_helper_and_says_so() {
    let f = harness_with_helper(false).await;
    let addr = f._bus.address();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();

    let out = run(addr, &["sync", "register-without-interception", root.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        out_text(&out)
            .contains(&format!("Folder registered without interception: {}", root.display())),
        "{}",
        out_text(&out)
    );
    assert!(
        err_text(&out).to_lowercase().contains("zeros"),
        "the zero-read risk must be stated on every success, not just left in LastError: {}",
        err_text(&out)
    );

    let out = run(addr, &["sync", "status"]);
    assert!(out.status.success(), "{out:?}");
    assert!(out_text(&out).contains("no-interception"), "{}", out_text(&out));
}

/// `absolute_str`'s error path: a path that does not exist must be caught
/// before any D-Bus call is made, with a message that names the path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_reports_a_bad_path_before_touching_the_daemon() {
    let f = harness().await;
    let addr = f._bus.address();

    let out = run(addr, &["sync", "hydrate", "/no/such/path/konedrivectl-test"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(err_text(&out).contains("no such path"), "{}", err_text(&out));
}

/// `absolute_str` canonicalised every
/// path, so `sync register <symlink>` resolved the link and registered its
/// target — silently, while says a symbolic link is refused as a
/// root. The link's own name has to reach the daemon, which refuses it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_register_of_a_symlink_is_refused_not_resolved() {
    let f = harness_with_helper(false).await;
    let addr = f._bus.address();
    let target = f.dir.path().join("Target");
    std::fs::create_dir(&target).unwrap();
    let link = f.dir.path().join("Link");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let out = run(addr, &["sync", "register-without-interception", link.to_str().unwrap()]);
    assert!(!out.status.success(), "the symlink's target was registered: {out:?}");
    assert!(err_text(&out).contains("symbolic link"), "{}", err_text(&out));
    let status = run(addr, &["sync", "status"]);
    assert!(
        !out_text(&status).contains(target.to_str().unwrap()),
        "the target is registered: {}",
        out_text(&status)
    );
}

/// A named refusal (`NotEmpty`) must reach the terminal as a failure, never
/// as the bare "Folder registered" success line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_register_refusal_never_prints_a_bare_success() {
    let f = harness().await;
    let addr = f._bus.address();
    let root = f.dir.path().join("NotEmpty");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("x"), b"x").unwrap();

    let out = run(addr, &["sync", "register", root.to_str().unwrap()]);
    assert!(!out.status.success(), "{out:?}");
    assert!(!out_text(&out).contains("Folder registered"), "{}", out_text(&out));
    assert!(err_text(&out).contains("empty"), "{}", err_text(&out));
}

/// C1, at the binary a user actually runs: `register_root` returns `Ok(())`
/// even when startup recovery could not reset every file (`Folder.State`
/// flips to `error` instead) — before this task's fix, `main.rs` printed
/// "Folder registered: {path}" and exited 0 regardless, which is exactly
/// the failure C1 describes: the one thing this interface exists to make
/// visible, wearing a success message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_register_does_not_print_bare_success_when_recovery_failed() {
    let f = harness_refusing_clear_ignore().await;
    let addr = f._bus.address();
    let root = f.dir.path().join("Stuck");
    std::fs::create_dir(&root).unwrap();
    let _stuck_path = stuck_root(&root);

    let out = run(addr, &["sync", "register", root.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "a register that leaves the root unrecovered must not exit 0: {out:?}"
    );
    assert!(
        !out_text(&out).contains(&format!("Folder registered: {}", root.display())),
        "C1: an unqualified success line must not print when recovery failed: {}",
        out_text(&out)
    );
    assert!(
        err_text(&out).to_lowercase().contains("recover"),
        "{}",
        err_text(&out)
    );
}
