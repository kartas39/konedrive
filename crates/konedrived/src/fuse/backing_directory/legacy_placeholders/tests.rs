use std::fs;

use konedrive_fs::placeholder::{State, XATTR_STATE};

use super::holds_legacy_placeholders;

/// A folder of downloaded copies and of the user's own files is one a mount goes over.
#[test]
fn whole_files_and_plain_files_are_not_an_older_konedrives() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("docs")).unwrap();
    fs::write(dir.path().join("docs/own.txt"), b"mine").unwrap();
    fs::write(dir.path().join("copy.txt"), b"whole").unwrap();
    xattr::set(dir.path().join("copy.txt"), XATTR_STATE, State::Hydrated.as_str().as_bytes()).unwrap();
    assert!(!holds_legacy_placeholders(dir.path()));
}

/// One placeholder anywhere in it is enough.
#[test]
fn a_placeholder_below_makes_it_an_older_konedrives() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("docs")).unwrap();
    fs::write(dir.path().join("docs/a.txt"), b"").unwrap();
    xattr::set(dir.path().join("docs/a.txt"), XATTR_STATE, State::OnlineOnly.as_str().as_bytes()).unwrap();
    assert!(holds_legacy_placeholders(dir.path()));
}
