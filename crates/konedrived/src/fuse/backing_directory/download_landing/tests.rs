use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use konedrive_fs::placeholder::{State, XATTR_ITEM_ID, XATTR_STATE};

use super::{step, LandingOutcome, TargetContents, LandingAction};
use crate::fuse::backing_directory::tests::folder;
use crate::fuse::backing_directory::BackingDirectory;

const AT: &str = "docs/a.txt";

/// A download of item `A`, whole, landed at `docs/a.txt`.
fn land(under: &BackingDirectory) -> LandingOutcome {
    let mut file = under.unnamed().unwrap();
    file.write_all(b"downloaded").unwrap();
    under.land(&file, Path::new(AT), "A", UNIX_EPOCH + Duration::from_secs(1_000)).unwrap()
}

/// What stands at `docs/a.txt`: a file that carries `id` and is marked `state`, where given.
fn standing(dir: &Path, id: Option<&str>, state: Option<State>) {
    let _ = fs::remove_file(dir.join(AT));
    fs::write(dir.join(AT), b"standing").unwrap();
    if let Some(id) = id {
        xattr::set(dir.join(AT), XATTR_ITEM_ID, id.as_bytes()).unwrap();
    }
    if let Some(state) = state {
        xattr::set(dir.join(AT), XATTR_STATE, state.as_str().as_bytes()).unwrap();
    }
}

fn folder_with_docs() -> (tempfile::TempDir, BackingDirectory) {
    let (dir, under) = folder();
    fs::create_dir(dir.path().join("docs")).unwrap();
    (dir, under)
}

#[test]
fn nothing_in_the_way_and_the_download_gets_the_name() {
    assert_eq!(step(TargetContents::Nothing), LandingAction::Link);
}

#[test]
fn the_items_own_whole_copy_is_kept() {
    assert_eq!(step(TargetContents::CopyOfSameFile), LandingAction::Keep);
}

#[test]
fn a_file_that_is_not_whole_is_replaced() {
    assert_eq!(step(TargetContents::PartialFile), LandingAction::Replace);
}

#[test]
fn the_copy_of_another_item_and_a_file_of_the_users_own_are_never_replaced() {
    assert_eq!(step(TargetContents::CopyOfOtherFile), LandingAction::Refuse);
    assert_eq!(step(TargetContents::UsersOwnFile), LandingAction::Refuse);
}

#[test]
fn what_stands_at_a_place_is_told_by_the_id_it_carries_and_whether_it_is_whole() {
    let (dir, under) = folder_with_docs();
    let stands = |under: &BackingDirectory| under.stands(Path::new(AT), "A");
    assert_eq!(stands(&under), TargetContents::Nothing);
    standing(dir.path(), None, None);
    assert_eq!(stands(&under), TargetContents::UsersOwnFile);
    standing(dir.path(), Some("A"), None);
    assert_eq!(stands(&under), TargetContents::PartialFile);
    standing(dir.path(), Some("A"), Some(State::OnlineOnly));
    assert_eq!(stands(&under), TargetContents::PartialFile);
    standing(dir.path(), Some("A"), Some(State::Hydrated));
    assert_eq!(stands(&under), TargetContents::CopyOfSameFile);
    standing(dir.path(), Some("B"), Some(State::Hydrated));
    assert_eq!(stands(&under), TargetContents::CopyOfOtherFile);
    assert_eq!(under.stands(Path::new("docs"), "A"), TargetContents::UsersOwnFile);
}

/// LandingOutcome, the download is the item's copy: its content, its time, no write bit.
#[test]
fn a_download_lands_as_its_items_copy() {
    let (dir, under) = folder_with_docs();
    assert_eq!(land(&under), LandingOutcome::Placed);
    assert_eq!(fs::read(dir.path().join(AT)).unwrap(), b"downloaded");
    let meta = under.copy(Path::new(AT), "A").expect("the item's whole copy");
    assert_eq!((meta.mtime(), meta.permissions().mode() & 0o777), (1_000, 0o444));
}

#[test]
fn a_download_lands_over_a_file_that_is_not_whole() {
    let (dir, under) = folder_with_docs();
    standing(dir.path(), Some("B"), Some(State::OnlineOnly));
    assert_eq!(land(&under), LandingOutcome::Placed);
    assert_eq!(fs::read(dir.path().join(AT)).unwrap(), b"downloaded");
    assert!(under.has_copy(Path::new(AT), "A"));
}

#[test]
fn a_download_leaves_the_items_copy_that_stands_already() {
    let (dir, under) = folder_with_docs();
    standing(dir.path(), Some("A"), Some(State::Hydrated));
    assert_eq!(land(&under), LandingOutcome::Kept);
    assert_eq!(fs::read(dir.path().join(AT)).unwrap(), b"standing");
}

#[test]
fn a_download_is_refused_where_another_items_copy_or_a_file_of_the_users_stands() {
    let (dir, under) = folder_with_docs();
    standing(dir.path(), Some("B"), Some(State::Hydrated));
    assert_eq!(land(&under), LandingOutcome::Refused);
    assert!(under.has_copy(Path::new(AT), "B"));
    fs::remove_file(dir.path().join(AT)).unwrap();
    standing(dir.path(), None, None);
    assert_eq!(land(&under), LandingOutcome::Refused);
    assert_eq!(fs::read(dir.path().join(AT)).unwrap(), b"standing");
}

/// The directory the item lies in is the caller's to make first.
#[test]
fn a_download_does_not_land_in_a_directory_that_is_missing() {
    let (_dir, under) = folder();
    let file = under.unnamed().unwrap();
    assert!(under.land(&file, Path::new(AT), "A", UNIX_EPOCH).is_err());
}
