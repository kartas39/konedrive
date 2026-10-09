use std::fs;

use super::{list_directories, read_served_folders};

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
