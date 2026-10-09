use std::fs;
use std::os::unix::fs::PermissionsExt;

use super::{remove_write_bits, restore_write_bits};

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
