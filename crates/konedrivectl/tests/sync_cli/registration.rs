use super::*;

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
    // Refused before anything is mounted.
    let (f, _graph) = harness_onedrive().await;
    let addr = f._bus.address();
    let target = f.dir.path().join("Target");
    std::fs::create_dir(&target).unwrap();
    let link = f.dir.path().join("Link");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let out = run(addr, &["sync", "register", link.to_str().unwrap()]);
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
    // Refused before anything is mounted.
    let (f, _graph) = harness_onedrive().await;
    let addr = f._bus.address();
    let root = f.dir.path().join("NotEmpty");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("x"), b"x").unwrap();

    let out = run(addr, &["sync", "register", root.to_str().unwrap()]);
    assert!(!out.status.success(), "{out:?}");
    assert!(!out_text(&out).contains("Folder registered"), "{}", out_text(&out));
    assert!(err_text(&out).contains("empty"), "{}", err_text(&out));
}

