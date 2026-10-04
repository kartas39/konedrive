use super::write_secret_atomically;

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

/// `rename(2)` replaces the symlink itself, so its target is never opened, truncated, or
/// written through.
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

/// An fd opened before the export keeps reading the old inode's content: `rename(2)` never
/// truncates it in place.
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
