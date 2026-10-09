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

/// The directory an older konedrive kept what it had misplaced in: the folder is that
/// konedrive's, wherever in it the directory is, and whatever else it holds.
#[test]
fn the_holding_directory_of_an_older_konedrive_makes_it_an_older_konedrives() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("docs").join(crate::folder::disk::HOLDING)).unwrap();
    fs::write(dir.path().join("own.txt"), b"mine").unwrap();
    assert!(holds_legacy_placeholders(dir.path()));
}

/// No mount is put over such a folder: its backing directory is not opened, and the refusal
/// names the folder.
#[test]
fn a_folder_of_an_older_konedrive_is_refused_as_a_backing_directory() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(crate::folder::disk::HOLDING)).unwrap();
    let refused = crate::fuse::backing_directory::BackingDirectory::open(dir.path()).err().expect("the folder is refused");
    assert_eq!(refused.to_string(), super::describe_refusal(dir.path()).to_string());
    assert!(refused.to_string().contains(&dir.path().display().to_string()));
}
