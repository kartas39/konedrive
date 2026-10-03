use super::*;

// --- `dev export-access-token`, a development build's (`dev-tools`) -------

/// I1: an existing file at `--out` is replaced by a new inode, not
/// truncated in place. An fd opened before the export — the shape the
/// used to reproduce the bug — proves it: it must keep reading the
/// *old* content, byte for byte, forever, because `rename(2)` never touches
/// the inode a still-open fd already holds.
#[cfg(feature = "dev-tools")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_exports_the_access_token_and_nothing_else_readable_only_by_the_user() {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    let f = harness().await;
    f.account
        .tokens()
        .seed(&konedrive_graph::oauth::TokenResponse {
            access_token: "AT-EXPORT".into(),
            expires_in: 3600,
            refresh_token: Some("RT-NEVER".into()),
            scope: Some("Files.Read User.Read".into()),
        })
        .await;
    let out_file = f.dir.path().join("token");
    std::fs::write(&out_file, b"old").unwrap();
    std::fs::set_permissions(&out_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut held_open = std::fs::File::open(&out_file).unwrap();

    let out = run(f._bus.address(), &["dev", "export-access-token", "--out", out_file.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(std::fs::read_to_string(&out_file).unwrap(), "AT-EXPORT");
    assert_eq!(std::fs::metadata(&out_file).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(!out_text(&out).contains("AT-EXPORT"), "the token must never reach stdout: {}", out_text(&out));
    assert!(!err_text(&out).contains("AT-EXPORT"), "the token must never reach stderr: {}", err_text(&out));
    let mut still_reads = String::new();
    held_open.read_to_string(&mut still_reads).unwrap();
    assert_eq!(still_reads, "old", "an fd opened before the export must keep reading the old inode");
}

/// I1: `--out` naming a symlink — the 's exact reproduction — must
/// have the link itself replaced by `rename(2)`, never the file it points
/// to opened and truncated.
#[cfg(feature = "dev-tools")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_export_access_token_replaces_a_symlink_without_touching_its_target() {
    use std::os::unix::fs::PermissionsExt;
    let f = harness().await;
    f.account
        .tokens()
        .seed(&konedrive_graph::oauth::TokenResponse {
            access_token: "AT-EXPORT".into(),
            expires_in: 3600,
            refresh_token: Some("RT-NEVER".into()),
            scope: Some("Files.Read User.Read".into()),
        })
        .await;
    let target = f.dir.path().join("someone-elses-file");
    std::fs::write(&target, b"do not touch").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
    let link = f.dir.path().join("token-link");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let out = run(f._bus.address(), &["dev", "export-access-token", "--out", link.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        !std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(),
        "the link must be replaced by a regular file, not written through"
    );
    assert_eq!(std::fs::read_to_string(&link).unwrap(), "AT-EXPORT");
    assert_eq!(std::fs::metadata(&link).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "do not touch", "the old target must be untouched");
}

#[cfg(feature = "dev-tools")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_export_access_token_refused_while_signed_out_names_the_reason() {
    let f = harness_signed_out().await;
    let out_file = f.dir.path().join("token");

    let out = run(f._bus.address(), &["dev", "export-access-token", "--out", out_file.to_str().unwrap()]);
    assert!(!out.status.success(), "{out:?}");
    assert!(!out_file.exists(), "nothing must be written on a refusal: {out:?}");
    assert!(err_text(&out).to_lowercase().contains("signed in"), "{}", err_text(&out));
}
