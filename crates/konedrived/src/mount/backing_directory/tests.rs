use std::fs;
use std::path::Path;

use konedrive_fs::placeholder::{State, XATTR_ROOT, XATTR_STATE};

use super::{remove_write_bits, BackingDirectory, OpenRefusal, LEGACY_STAGING_DIRECTORY};

/// A directory on disk with nothing in it, opened as a mount's backing directory is.
pub(super) fn empty_backing_directory() -> (tempfile::TempDir, BackingDirectory) {
    let dir = tempfile::tempdir().unwrap();
    let backing_directory = BackingDirectory::open(dir.path()).unwrap();
    (dir, backing_directory)
}

/// A path is reached through the descriptor: what is written by it is in the backing directory.
#[test]
fn a_path_of_the_backing_directory_leads_into_the_directory_itself() {
    let (dir, backing_directory) = empty_backing_directory();
    fs::write(backing_directory.resolve_path(Path::new("a.txt")), b"a").unwrap();
    assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"a");
    assert_eq!(fs::read_dir(backing_directory.resolve_path(Path::new(""))).unwrap().count(), 1);
}

#[test]
fn the_staging_directory_of_an_earlier_build_is_removed_when_the_backing_directory_is_opened() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(LEGACY_STAGING_DIRECTORY)).unwrap();
    fs::write(dir.path().join(LEGACY_STAGING_DIRECTORY).join("part"), b"x").unwrap();
    let backing_directory = BackingDirectory::open(dir.path()).unwrap();
    assert!(backing_directory.read_metadata(Path::new(LEGACY_STAGING_DIRECTORY)).is_none());
}

/// The empty path is the backing directory itself, which has attributes of its own.
#[test]
fn an_attribute_is_read_of_the_backing_directory_and_of_what_it_holds() {
    let (dir, backing_directory) = empty_backing_directory();
    xattr::set(dir.path(), XATTR_ROOT, b"R").unwrap();
    fs::write(dir.path().join("a.txt"), b"a").unwrap();
    xattr::set(dir.path().join("a.txt"), "user.test", b"v").unwrap();
    assert_eq!(backing_directory.read_attribute(Path::new(""), XATTR_ROOT).as_deref(), Some("R"));
    assert_eq!(backing_directory.read_attribute(Path::new("a.txt"), "user.test").as_deref(), Some("v"));
    assert_eq!(backing_directory.read_attribute(Path::new("a.txt"), XATTR_ROOT), None);
}

#[test]
fn the_backing_directorys_filesystem_says_how_much_space_it_has() {
    let (_dir, backing_directory) = empty_backing_directory();
    assert!(backing_directory.measure_space().unwrap().f_bsize > 0);
}

/// What an older konedrive left would be taken for downloaded copies or the user's own files.
#[test]
fn a_directory_that_holds_an_older_konedrives_placeholder_is_not_opened() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), b"").unwrap();
    xattr::set(dir.path().join("a.txt"), XATTR_STATE, State::OnlineOnly.as_str().as_bytes()).unwrap();
    assert!(matches!(BackingDirectory::open(dir.path()), Err(OpenRefusal::LegacyPlaceholders)));
}

/// The write bits of the directory were removed when the daemon last ended; with the mount over it, it is the daemon's.
#[test]
fn a_directory_without_write_bits_is_opened_and_has_them_restored() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("docs")).unwrap();
    remove_write_bits(dir.path());
    let backing_directory = BackingDirectory::open(dir.path()).unwrap();
    backing_directory.restore_write_bits();
    backing_directory.make_file(Path::new("docs/a.txt")).unwrap();
}
