use std::fs;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::{list_directories, lists_fuse_mount_at, read_served_folders, tidy_folder, tidy_served_folders};

/// What a file manager is told to read again: the folder first, then every directory in it,
/// and no file.
#[test]
fn the_directories_of_a_folder_begin_with_the_folder_and_hold_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("folder");
    fs::create_dir_all(folder.join("a/b")).unwrap();
    fs::write(folder.join("a/f.txt"), b"f").unwrap();
    let found = list_directories(&folder);
    assert_eq!(found[0], folder);
    let mut rest: Vec<_> = found[1..].to_vec();
    rest.sort();
    assert_eq!(rest, [folder.join("a"), folder.join("a/b")]);
}

/// Only an account whose root shows OneDrive is served by a mount.
#[test]
fn the_served_folders_are_the_roots_that_show_onedrive() {
    let dir = tempfile::tempdir().unwrap();
    let config_file = dir.path().join("config.toml");
    fs::write(&config_file, "[[accounts]]\nroot = { path = \"/a\", source = \"onedrive\" }\n[[accounts]]\nroot = { path = \"/b\", source = \"other\" }\n[[accounts]]\nlabel = \"no root\"\n").unwrap();
    assert_eq!(read_served_folders(&config_file), [std::path::PathBuf::from("/a")]);
    assert!(read_served_folders(&dir.path().join("missing.toml")).is_empty());
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// A folder no mount lies over is left a plain directory without its write bits: it shows
/// the downloaded files and takes nothing.
#[test]
fn a_folder_that_is_tidied_loses_its_write_bits() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("folder");
    fs::create_dir_all(folder.join("docs")).unwrap();
    fs::write(folder.join("docs/a.txt"), b"a").unwrap();
    tidy_folder(&folder);
    assert_eq!((mode(&folder), mode(&folder.join("docs")), mode(&folder.join("docs/a.txt"))), (0o555, 0o555, 0o444));
    // The test's own directory is removed with its write bits.
    crate::fuse::backing_directory::restore_write_bits(&folder);
}

/// Every folder the configuration records as showing OneDrive is tidied, and no other.
#[test]
fn the_served_folders_of_a_configuration_are_tidied() {
    let dir = tempfile::tempdir().unwrap();
    let (served, other) = (dir.path().join("served"), dir.path().join("other"));
    fs::create_dir(&served).unwrap();
    fs::create_dir(&other).unwrap();
    let config_file = dir.path().join("config.toml");
    fs::write(&config_file, format!("[[accounts]]\nroot = {{ path = \"{}\", source = \"onedrive\" }}\n[[accounts]]\nroot = {{ path = \"{}\", source = \"other\" }}\n", served.display(), other.display())).unwrap();
    assert_eq!(tidy_served_folders(&config_file), [served.clone()]);
    assert_eq!((mode(&served), mode(&other)), (0o555, 0o755));
    crate::fuse::backing_directory::restore_write_bits(&served);
}

/// A folder that is still mounted keeps its write bits: its daemon runs. Whether one is
/// mounted is read off the kernel's table of mounts — a FUSE mount at the folder's own path,
/// and neither a mount of another kind there nor a FUSE mount elsewhere.
#[test]
fn a_folder_is_mounted_when_the_table_of_mounts_has_a_fuse_mount_at_its_path() {
    let mounts = "/dev/sda2 /home ext4 rw,relatime 0 0\nkonedrive /home/u/OneDrive fuse.konedrive rw,nosuid,nodev,relatime,user_id=1000,group_id=1000 0 0\nportal /run/user/1000/doc fuse.portal rw 0 0\n";
    assert!(lists_fuse_mount_at(mounts, Path::new("/home/u/OneDrive")));
    assert!(!lists_fuse_mount_at(mounts, Path::new("/home/u/OneDrive/docs")));
    assert!(!lists_fuse_mount_at(mounts, Path::new("/home")), "a mount of another kind");
    assert!(!lists_fuse_mount_at(mounts, Path::new("/home/u/Other")));
    assert!(!lists_fuse_mount_at("", Path::new("/home/u/OneDrive")));
}
