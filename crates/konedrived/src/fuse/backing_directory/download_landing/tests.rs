use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use konedrive_fs::placeholder::{State, XATTR_ITEM_ID, XATTR_STATE};

use super::{choose_landing_action, LandingOutcome, TargetContents, LandingAction};
use crate::fuse::backing_directory::tests::empty_backing_directory;
use crate::fuse::backing_directory::BackingDirectory;

const AT: &str = "docs/a.txt";

/// A download of the OneDrive file `A`, whole, landed at `docs/a.txt`.
fn land(backing_directory: &BackingDirectory) -> LandingOutcome {
    let mut file = backing_directory.make_unnamed_file().unwrap();
    file.write_all(b"downloaded").unwrap();
    backing_directory.land(&file, Path::new(AT), "A", UNIX_EPOCH + Duration::from_secs(1_000)).unwrap()
}

/// What is at `docs/a.txt`: a file marked with the OneDrive id `id` and the state `state`, where given.
fn put_at_target(dir: &Path, id: Option<&str>, state: Option<State>) {
    let _ = fs::remove_file(dir.join(AT));
    fs::write(dir.join(AT), b"standing").unwrap();
    if let Some(id) = id {
        xattr::set(dir.join(AT), XATTR_ITEM_ID, id.as_bytes()).unwrap();
    }
    if let Some(state) = state {
        xattr::set(dir.join(AT), XATTR_STATE, state.as_str().as_bytes()).unwrap();
    }
}

fn backing_directory_with_docs() -> (tempfile::TempDir, BackingDirectory) {
    let (dir, backing_directory) = empty_backing_directory();
    fs::create_dir(dir.path().join("docs")).unwrap();
    (dir, backing_directory)
}

#[test]
fn nothing_in_the_way_and_the_download_gets_the_name() {
    assert_eq!(choose_landing_action(TargetContents::Nothing), LandingAction::Link);
}

#[test]
fn the_whole_copy_of_the_same_file_is_kept() {
    assert_eq!(choose_landing_action(TargetContents::CopyOfSameFile), LandingAction::Keep);
}

#[test]
fn a_file_that_is_not_whole_is_replaced() {
    assert_eq!(choose_landing_action(TargetContents::PartialFile), LandingAction::Replace);
}

#[test]
fn the_copy_of_another_file_and_a_file_of_the_users_own_are_never_replaced() {
    assert_eq!(choose_landing_action(TargetContents::CopyOfOtherFile), LandingAction::Refuse);
    assert_eq!(choose_landing_action(TargetContents::UsersOwnFile), LandingAction::Refuse);
}

#[test]
fn what_is_at_the_target_is_told_by_the_onedrive_id_it_is_marked_with_and_whether_it_is_whole() {
    let (dir, backing_directory) = backing_directory_with_docs();
    let target = |backing_directory: &BackingDirectory| backing_directory.inspect_target(Path::new(AT), "A");
    assert_eq!(target(&backing_directory), TargetContents::Nothing);
    put_at_target(dir.path(), None, None);
    assert_eq!(target(&backing_directory), TargetContents::UsersOwnFile);
    put_at_target(dir.path(), Some("A"), None);
    assert_eq!(target(&backing_directory), TargetContents::PartialFile);
    put_at_target(dir.path(), Some("A"), Some(State::OnlineOnly));
    assert_eq!(target(&backing_directory), TargetContents::PartialFile);
    put_at_target(dir.path(), Some("A"), Some(State::Hydrated));
    assert_eq!(target(&backing_directory), TargetContents::CopyOfSameFile);
    put_at_target(dir.path(), Some("B"), Some(State::Hydrated));
    assert_eq!(target(&backing_directory), TargetContents::CopyOfOtherFile);
    assert_eq!(backing_directory.inspect_target(Path::new("docs"), "A"), TargetContents::UsersOwnFile);
}

/// Landed, the download is the file's downloaded copy: its content, its time, no write bit.
#[test]
fn a_download_lands_as_the_files_downloaded_copy() {
    let (dir, backing_directory) = backing_directory_with_docs();
    assert_eq!(land(&backing_directory), LandingOutcome::Placed);
    assert_eq!(fs::read(dir.path().join(AT)).unwrap(), b"downloaded");
    let copy = backing_directory.find_downloaded_copy(Path::new(AT), "A").expect("the downloaded copy");
    let mode = backing_directory.read_metadata(Path::new(AT)).unwrap().permissions().mode() & 0o777;
    assert_eq!((copy.modified, mode), (UNIX_EPOCH + Duration::from_secs(1_000), 0o444));
}

#[test]
fn a_download_lands_over_a_file_that_is_not_whole() {
    let (dir, backing_directory) = backing_directory_with_docs();
    put_at_target(dir.path(), Some("B"), Some(State::OnlineOnly));
    assert_eq!(land(&backing_directory), LandingOutcome::Placed);
    assert_eq!(fs::read(dir.path().join(AT)).unwrap(), b"downloaded");
    assert!(backing_directory.find_downloaded_copy(Path::new(AT), "A").is_some());
}

#[test]
fn a_download_leaves_the_copy_of_the_same_file_that_is_there_already() {
    let (dir, backing_directory) = backing_directory_with_docs();
    put_at_target(dir.path(), Some("A"), Some(State::Hydrated));
    assert_eq!(land(&backing_directory), LandingOutcome::Kept);
    assert_eq!(fs::read(dir.path().join(AT)).unwrap(), b"standing");
}

#[test]
fn a_download_is_refused_where_another_files_copy_or_a_file_of_the_users_is() {
    let (dir, backing_directory) = backing_directory_with_docs();
    put_at_target(dir.path(), Some("B"), Some(State::Hydrated));
    assert_eq!(land(&backing_directory), LandingOutcome::Refused);
    assert!(backing_directory.find_downloaded_copy(Path::new(AT), "B").is_some());
    fs::remove_file(dir.path().join(AT)).unwrap();
    put_at_target(dir.path(), None, None);
    assert_eq!(land(&backing_directory), LandingOutcome::Refused);
    assert_eq!(fs::read(dir.path().join(AT)).unwrap(), b"standing");
}

/// The directory the file lies in is the caller's to make first.
#[test]
fn a_download_does_not_land_in_a_directory_that_is_missing() {
    let (_dir, backing_directory) = empty_backing_directory();
    let file = backing_directory.make_unnamed_file().unwrap();
    assert!(backing_directory.land(&file, Path::new(AT), "A", UNIX_EPOCH).is_err());
}
