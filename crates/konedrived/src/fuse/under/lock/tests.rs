use std::fs;
use std::os::unix::fs::PermissionsExt;

use super::{directories, set_tree};

fn mode(path: &std::path::Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Locked, the folder takes nothing: no write bit on a directory or a file, so nothing is put
/// into it by mistake while no mount lies over it. Unlocked, it is its owner's again.
#[test]
fn a_locked_folder_has_no_write_bit_and_an_unlocked_one_is_its_owners() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("folder");
    fs::create_dir_all(folder.join("docs/deep")).unwrap();
    fs::write(folder.join("docs/a.txt"), b"a").unwrap();

    set_tree(&folder, true);
    assert_eq!((mode(&folder), mode(&folder.join("docs")), mode(&folder.join("docs/deep"))), (0o555, 0o555, 0o555));
    assert_eq!(mode(&folder.join("docs/a.txt")), 0o444);
    assert!(fs::write(folder.join("docs/new.txt"), b"x").is_err(), "a locked directory takes no file");

    set_tree(&folder, false);
    assert_eq!((mode(&folder), mode(&folder.join("docs/deep")), mode(&folder.join("docs/a.txt"))), (0o755, 0o755, 0o644));
    fs::write(folder.join("docs/new.txt"), b"x").unwrap();
}

/// What a file manager is told to read again: the folder first, then every directory in it,
/// and no file.
#[test]
fn the_directories_of_a_folder_begin_with_the_folder_and_hold_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("folder");
    fs::create_dir_all(folder.join("a/b")).unwrap();
    fs::write(folder.join("a/f.txt"), b"f").unwrap();
    let found = directories(&folder);
    assert_eq!(found[0], folder);
    let mut rest: Vec<_> = found[1..].to_vec();
    rest.sort();
    assert_eq!(rest, [folder.join("a"), folder.join("a/b")]);
}
