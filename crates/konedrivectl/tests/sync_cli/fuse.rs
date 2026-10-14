//! A folder served through FUSE, driven as a user drives it: the daemon on a private bus, the
//! `konedrivectl` binary, and plain file calls on the folder.
//!
//! Ignored in an ordinary run: it mounts a filesystem. `tests/fuse-probe/run.sh cli` runs it in
//! the VM.

use std::os::unix::fs::MetadataExt;

use super::*;

fn state(path: &std::path::Path) -> String {
    xattr::get(path, "user.konedrive.state").ok().flatten().map(|v| String::from_utf8_lossy(&v).into_owned()).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "mounts a filesystem: run in the VM (tests/fuse-probe/run.sh cli)"]
async fn a_folder_served_through_fuse_is_driven_from_the_command_line() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let (f, graph) = harness_onedrive().await;
    Mock::given(method("GET"))
        .and(path("/me/drive/items/F"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "F", "name": "f.txt", "size": 3, "cTag": "c1", "file": {},
            "parentReference": {"id": "D"},
            "@microsoft.graph.downloadUrl": format!("{}/dl/F", graph.uri())
        })))
        .mount(&graph)
        .await;
    Mock::given(method("GET")).and(path("/dl/F")).respond_with(ResponseTemplate::new(200).set_body_bytes(b"abc".to_vec())).mount(&graph).await;
    let addr = f._bus.address();
    let root = f.dir.path().join("OneDrive");
    std::fs::create_dir(&root).unwrap();
    let file = root.join("docs/f.txt");
    let shown = |args: &[&str]| {
        let out = run(addr, args);
        format!("{}{}", out_text(&out), err_text(&out))
    };

    let registered = run(addr, &["sync", "register", root.to_str().unwrap()]);
    assert!(registered.status.success(), "{registered:?}");
    wait_for(|| file.symlink_metadata().is_ok()).await;
    tokio::time::sleep(Duration::from_millis(500)).await; // counters are coalesced

    // The folder is a mount.
    let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap();
    assert!(mounts.lines().any(|l| l.contains(root.to_str().unwrap()) && l.contains("fuse.konedrive")), "{mounts}");
    let status = shown(&["sync", "status"]);
    assert!(status.lines().any(|l| l == "State:                  ready"), "{status}");
    assert!(status.lines().any(|l| l.starts_with("Items:") && l.contains("3 in OneDrive") && l.contains("2 in the folder")), "{status}");

    // Not downloaded: its size is the item's, it takes no space, and nothing of it is on disk.
    let meta = file.metadata().unwrap();
    assert_eq!((meta.len(), meta.blocks()), (3, 0));
    assert_eq!(state(&file), "online-only");
    assert_eq!(shown(&["sync", "state", file.to_str().unwrap()]).trim(), "online-only");

    // Reading it gives its bytes at once, and — all of it being read — downloads it.
    assert_eq!(std::fs::read(&file).unwrap(), b"abc");
    wait_for(|| state(&file) == "hydrated").await;
    let activity = shown(&["sync", "activity"]);
    assert!(activity.contains("f.txt"), "{activity}");

    // Freed up, pinned (which downloads it again by itself), refused while pinned, unpinned.
    let freed = run(addr, &["sync", "dehydrate", file.to_str().unwrap()]);
    assert!(freed.status.success(), "{freed:?}");
    assert_eq!(state(&file), "online-only");
    let docs = root.join("docs");
    let pinned = run(addr, &["sync", "pin", docs.to_str().unwrap()]);
    assert!(pinned.status.success(), "{pinned:?}");
    wait_for(|| state(&file) == "hydrated").await;
    let refused = run(addr, &["sync", "dehydrate", file.to_str().unwrap()]);
    assert!(!refused.status.success() && err_text(&refused).contains("is kept on this device, so its space is not freed up"), "{refused:?}");
    let unpinned = run(addr, &["sync", "unpin", docs.to_str().unwrap()]);
    assert!(unpinned.status.success(), "{unpinned:?}");
    let space = run(addr, &["sync", "free-up-space"]);
    assert!(space.status.success(), "{space:?}");
    assert_eq!(state(&file), "online-only");

    // The folder says it takes no change before anything is tried: no write bit on a
    // directory or a file, and no leave to write.
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(root.metadata().unwrap().permissions().mode() & 0o222, 0);
    assert_eq!(docs.metadata().unwrap().permissions().mode() & 0o222, 0);
    assert_eq!(file.metadata().unwrap().permissions().mode() & 0o222, 0);
    let docs_c = std::ffi::CString::new(docs.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::access(docs_c.as_ptr(), libc::W_OK) }, -1);

    // A program cannot change the folder.
    assert_eq!(std::fs::write(root.join("new.txt"), b"x").unwrap_err().raw_os_error(), Some(libc::EROFS));

    // Forgotten: the mount goes, and the folder holds what was downloaded, which is nothing.
    let forgotten = run(addr, &["sync", "forget"]);
    assert!(forgotten.status.success(), "{forgotten:?}");
    let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap();
    assert!(!mounts.contains(root.to_str().unwrap()), "{mounts}");
    assert!(!file.exists());
}
