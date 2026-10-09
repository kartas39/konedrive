use std::fs;
use std::os::unix::fs::PermissionsExt;

use super::{to_mode_bits, remove_write_bits, restore_write_bits, WriteBits};
use crate::fuse::backing_directory::tests::empty_backing_directory;
use crate::fuse::backing_directory::EntryKind;

fn mode(path: &std::path::Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Without its write bits the folder takes nothing: none on a directory or a file, so
/// nothing is put into it by mistake while no mount lies over it. With them restored it is
/// its owner's again.
#[test]
fn a_folder_without_write_bits_takes_nothing_and_with_them_restored_is_its_owners() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("folder");
    fs::create_dir_all(folder.join("docs/deep")).unwrap();
    fs::write(folder.join("docs/a.txt"), b"a").unwrap();

    remove_write_bits(&folder);
    assert_eq!((mode(&folder), mode(&folder.join("docs")), mode(&folder.join("docs/deep"))), (0o555, 0o555, 0o555));
    assert_eq!(mode(&folder.join("docs/a.txt")), 0o444);
    assert!(fs::write(folder.join("docs/new.txt"), b"x").is_err(), "a directory without its write bit takes no file");

    restore_write_bits(&folder);
    assert_eq!((mode(&folder), mode(&folder.join("docs/deep")), mode(&folder.join("docs/a.txt"))), (0o755, 0o755, 0o644));
    fs::write(folder.join("docs/new.txt"), b"x").unwrap();
}

#[test]
fn the_modes_are_those_of_an_owner_who_may_or_may_not_change_the_entry() {
    assert_eq!(to_mode_bits(EntryKind::Directory, WriteBits::Present), 0o755);
    assert_eq!(to_mode_bits(EntryKind::File, WriteBits::Present), 0o644);
    assert_eq!(to_mode_bits(EntryKind::Directory, WriteBits::Removed), 0o555);
    assert_eq!(to_mode_bits(EntryKind::File, WriteBits::Removed), 0o444);
}

/// The path the write bits are restored through names the directory, and not the link the
/// kernel keeps for the descriptor: the walk that sets the modes looks at each path without
/// following links, and does nothing for what is neither a directory nor a file.
#[test]
fn the_path_of_the_root_directory_names_the_directory_and_not_a_link() {
    let (_dir, backing_directory) = empty_backing_directory();
    assert!(fs::symlink_metadata(backing_directory.resolve_root_directory_path()).unwrap().is_dir());
    assert!(fs::symlink_metadata(backing_directory.resolve_root_path()).unwrap().file_type().is_symlink());
}

/// With the mount over it the backing directory is reached by its descriptor alone.
#[test]
fn the_write_bits_of_the_backing_directory_are_restored_through_its_descriptor() {
    let (dir, backing_directory) = empty_backing_directory();
    fs::create_dir(dir.path().join("docs")).unwrap();
    fs::write(dir.path().join("docs/a.txt"), b"a").unwrap();
    remove_write_bits(dir.path());
    backing_directory.restore_write_bits();
    assert_eq!((mode(dir.path()), mode(&dir.path().join("docs")), mode(&dir.path().join("docs/a.txt"))), (0o755, 0o755, 0o644));
}
