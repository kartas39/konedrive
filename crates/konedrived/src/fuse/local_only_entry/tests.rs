//! The walk and the two questions on a backing directory, with rules that ignore `*.tmp`
//! and `cache`.

use std::fs;
use std::path::{Path, PathBuf};

use super::{collect_local_only_entries, find_upload_status, is_in_ignored_directory};
use crate::fuse::backing_directory::BackingDirectory;
use crate::fuse::directory_reference::DirectoryReference;
use crate::fuse::upload_rules::tests::rules;
use crate::fuse::upload_rules::UploadStatus;

fn backing_directory() -> (tempfile::TempDir, BackingDirectory) {
    let dir = tempfile::tempdir().unwrap();
    let backing_directory = BackingDirectory::open(dir.path()).unwrap();
    (dir, backing_directory)
}

fn root() -> DirectoryReference {
    DirectoryReference { onedrive_id: Some("R".into()), path: PathBuf::new() }
}

/// The paths of what a walk from `name` in the root collects, in its order.
fn collected(backing_directory: &BackingDirectory, name: &str) -> Vec<PathBuf> {
    collect_local_only_entries(backing_directory, &rules(), &root(), name).into_iter().map(|entry| entry.place.path()).collect()
}

#[test]
fn a_walk_collects_a_directory_first_and_then_all_it_holds() {
    let (dir, backing_directory) = backing_directory();
    fs::create_dir_all(dir.path().join("new/deep")).unwrap();
    fs::write(dir.path().join("new/deep/x.txt"), b"x").unwrap();
    let walked = collect_local_only_entries(&backing_directory, &rules(), &root(), "new");
    assert_eq!(walked.iter().map(|entry| entry.place.path()).collect::<Vec<_>>(), [PathBuf::from("new"), PathBuf::from("new/deep"), PathBuf::from("new/deep/x.txt")]);
    // The top is in the directory the walk was asked about; what is below it is found by
    // its path alone.
    assert_eq!(walked[0].place.directory, root());
    assert_eq!(walked[2].place.directory, DirectoryReference { onedrive_id: None, path: "new/deep".into() });
    assert_eq!(collected(&backing_directory, "none"), Vec::<PathBuf>::new(), "nothing is there");
}

#[test]
fn a_walk_passes_over_what_is_ignored_with_all_it_holds() {
    let (dir, backing_directory) = backing_directory();
    fs::create_dir_all(dir.path().join("new/cache")).unwrap();
    fs::write(dir.path().join("new/cache/kept.txt"), b"k").unwrap();
    fs::write(dir.path().join("new/x.tmp"), b"t").unwrap();
    fs::write(dir.path().join("new/x.txt"), b"x").unwrap();
    let mut walked = collected(&backing_directory, "new");
    walked.sort();
    assert_eq!(walked, [PathBuf::from("new"), PathBuf::from("new/x.txt")]);
    // And nothing at all from a top that is ignored itself.
    fs::create_dir(dir.path().join("cache")).unwrap();
    fs::write(dir.path().join("cache/x.txt"), b"x").unwrap();
    assert!(collected(&backing_directory, "cache").is_empty());
}

/// It is OneDrive's, or was just sent there: neither it nor what it holds is uploaded anew.
#[test]
fn a_walk_passes_over_what_is_marked_with_a_onedrive_id() {
    let (dir, backing_directory) = backing_directory();
    fs::create_dir_all(dir.path().join("new/sent")).unwrap();
    fs::write(dir.path().join("new/sent/inside.txt"), b"i").unwrap();
    fs::write(dir.path().join("new/sent.txt"), b"s").unwrap();
    backing_directory.mark_with_onedrive_id(Path::new("new/sent"), "S").unwrap();
    backing_directory.mark_with_onedrive_id(Path::new("new/sent.txt"), "T").unwrap();
    assert_eq!(collected(&backing_directory, "new"), [PathBuf::from("new")]);
    backing_directory.mark_with_onedrive_id(Path::new("new"), "N").unwrap();
    assert!(collected(&backing_directory, "new").is_empty());
}

/// A symbolic link has nothing OneDrive could hold.
#[test]
fn a_walk_passes_over_what_is_neither_a_file_nor_a_directory() {
    let (dir, backing_directory) = backing_directory();
    fs::create_dir(dir.path().join("new")).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", dir.path().join("new/link")).unwrap();
    assert_eq!(collected(&backing_directory, "new"), [PathBuf::from("new")]);
}

/// The rules are told which directory is OneDrive's by its mark: one that is, is never
/// ignored, whatever its name.
#[test]
fn what_is_in_a_directory_marked_with_a_onedrive_id_is_uploaded_whatever_the_directorys_name() {
    let (dir, backing_directory) = backing_directory();
    fs::create_dir(dir.path().join("cache")).unwrap();
    assert_eq!(find_upload_status(&backing_directory, &rules(), Path::new("cache"), "a.txt"), UploadStatus::Ignored);
    assert!(is_in_ignored_directory(&backing_directory, &rules(), Path::new("cache")));
    backing_directory.mark_with_onedrive_id(Path::new("cache"), "C").unwrap();
    assert_eq!(find_upload_status(&backing_directory, &rules(), Path::new("cache"), "a.txt"), UploadStatus::Uploaded);
    assert!(!is_in_ignored_directory(&backing_directory, &rules(), Path::new("cache")));
    // Its own name is on the list all the same.
    assert_eq!(find_upload_status(&backing_directory, &rules(), Path::new("cache"), "a.tmp"), UploadStatus::Ignored);
}
