use std::fs;
use std::path::Path;

use crate::mount::backing_directory::tests::empty_backing_directory;
use crate::mount::backing_directory::EntryKind;

#[test]
fn a_disk_entry_is_the_file_or_directory_at_a_path_with_its_handle() {
    let (dir, backing_directory) = empty_backing_directory();
    fs::create_dir(dir.path().join("docs")).unwrap();
    fs::write(dir.path().join("docs/a.txt"), b"abc").unwrap();
    let (file, docs) = (backing_directory.find_disk_entry(Path::new("docs/a.txt")).unwrap(), backing_directory.find_disk_entry(Path::new("docs")).unwrap());
    assert_eq!((file.metadata.kind, file.metadata.size, docs.metadata.kind), (EntryKind::File, 3, EntryKind::Directory));
    assert_eq!(backing_directory.find_disk_entry(Path::new("docs/a.txt")).unwrap().handle, file.handle);
    assert!(backing_directory.find_disk_entry(Path::new("docs/none.txt")).is_none());
}

#[test]
fn two_files_have_two_file_handles() {
    let (dir, backing_directory) = empty_backing_directory();
    fs::write(dir.path().join("a.txt"), b"a").unwrap();
    fs::write(dir.path().join("b.txt"), b"a").unwrap();
    assert_ne!(backing_directory.find_disk_entry(Path::new("a.txt")).unwrap().handle, backing_directory.find_disk_entry(Path::new("b.txt")).unwrap().handle);
}

/// The handle goes with the file: the same one under its new name, and not the one of a
/// file made where it stood.
#[test]
fn a_renamed_file_keeps_its_file_handle() {
    let (dir, backing_directory) = empty_backing_directory();
    fs::write(dir.path().join("a.txt"), b"a").unwrap();
    let was = backing_directory.find_disk_entry(Path::new("a.txt")).unwrap();
    fs::rename(dir.path().join("a.txt"), dir.path().join("b.txt")).unwrap();
    fs::write(dir.path().join("a.txt"), b"a").unwrap();
    assert_eq!(backing_directory.find_disk_entry(Path::new("b.txt")).unwrap().handle, was.handle);
    assert_ne!(backing_directory.find_disk_entry(Path::new("a.txt")).unwrap().handle, was.handle);
}

/// A symbolic link is no disk entry, and its handle is its own, never that of what it points to.
#[test]
fn a_symbolic_link_is_not_followed_and_is_no_disk_entry() {
    let (dir, backing_directory) = empty_backing_directory();
    fs::write(dir.path().join("a.txt"), b"a").unwrap();
    std::os::unix::fs::symlink("a.txt", dir.path().join("link")).unwrap();
    assert!(backing_directory.find_disk_entry(Path::new("link")).is_none());
    let link = backing_directory.find_file_handle(Path::new("link")).unwrap();
    assert_ne!(link, backing_directory.find_disk_entry(Path::new("a.txt")).unwrap().handle);
}

#[test]
fn an_entry_is_marked_with_the_onedrive_id_it_was_given_until_it_is_cleared() {
    let (dir, backing_directory) = empty_backing_directory();
    fs::create_dir(dir.path().join("docs")).unwrap();
    assert_eq!(backing_directory.read_onedrive_id(Path::new("docs")), None);
    backing_directory.mark_with_onedrive_id(Path::new("docs"), "D").unwrap();
    assert_eq!(backing_directory.read_onedrive_id(Path::new("docs")).as_deref(), Some("D"));
    backing_directory.clear_onedrive_id(Path::new("docs"));
    assert_eq!(backing_directory.read_onedrive_id(Path::new("docs")), None);
    assert_eq!(backing_directory.read_onedrive_id(Path::new("none")), None);
}

/// What a change from OneDrive moves aside and what a walk collects to be uploaded: a file
/// or directory that is marked with no OneDrive id at all, whatever the store knows.
#[test]
fn an_unmarked_disk_entry_is_a_file_or_directory_marked_with_no_onedrive_id() {
    let (dir, backing_directory) = empty_backing_directory();
    assert!(backing_directory.find_unmarked_disk_entry(Path::new("own.txt")).is_none(), "nothing is there");
    fs::write(dir.path().join("own.txt"), b"mine").unwrap();
    fs::create_dir(dir.path().join("own")).unwrap();
    assert!(backing_directory.find_unmarked_disk_entry(Path::new("own.txt")).is_some());
    assert!(backing_directory.find_unmarked_disk_entry(Path::new("own")).is_some());
    // Marked with any id, also one no store has: a file just uploaded is not the user's own
    // to upload again.
    backing_directory.mark_with_onedrive_id(Path::new("own.txt"), "just-uploaded").unwrap();
    assert!(backing_directory.find_unmarked_disk_entry(Path::new("own.txt")).is_none());
    std::os::unix::fs::symlink("own", dir.path().join("link")).unwrap();
    assert!(backing_directory.find_unmarked_disk_entry(Path::new("link")).is_none(), "neither a file nor a directory");
}
