use std::path::Path;

use super::UploadRules;
use crate::folder::disk::HOLDING;
use crate::local::IgnoreList;

/// The rules of an account that ignores `*.tmp` and `cache`.
pub(in crate::fuse) fn rules() -> UploadRules {
    UploadRules::new(IgnoreList::new(["*.tmp", "cache"]).shared())
}

fn none(_: &Path) -> bool {
    false
}

#[test]
fn a_name_on_the_ignore_list_is_ignored() {
    let rules = rules();
    assert!(rules.is_ignored(Path::new(""), "a.tmp", none));
    assert!(rules.is_ignored(Path::new("docs"), "cache", none));
    assert!(!rules.is_ignored(Path::new("docs"), "a.txt", none));
}

#[test]
fn what_is_below_an_ignored_directory_that_is_not_in_onedrive_stays_with_it() {
    let rules = rules();
    assert!(rules.is_ignored(Path::new("cache"), "a.txt", none));
    assert!(rules.is_ignored(Path::new("docs/cache/deep"), "a.txt", none));
    assert!(rules.is_in_ignored_directory(Path::new("docs/cache"), none));
    assert!(!rules.is_in_ignored_directory(Path::new("docs"), none) && !rules.is_in_ignored_directory(Path::new(""), none));
}

/// A directory that came from OneDrive is sent whatever its name, and so is what is made in
/// it.
#[test]
fn an_ignored_name_that_is_in_onedrive_holds_nothing_back() {
    let rules = rules();
    let in_onedrive = |at: &Path| at == Path::new("docs/cache");
    assert!(!rules.is_ignored(Path::new("docs/cache"), "a.txt", in_onedrive));
    assert!(!rules.is_in_ignored_directory(Path::new("docs/cache"), in_onedrive));
    // Its own name is asked of the list all the same: it is not in OneDrive.
    assert!(rules.is_ignored(Path::new("docs/cache"), "a.tmp", in_onedrive));
    // Another directory of that name, which is not in OneDrive, still stays.
    assert!(rules.is_in_ignored_directory(Path::new("cache/docs/cache"), in_onedrive));
}

/// The directory a thing is in is asked by its chain alone: its own name is not a name of the
/// thing.
#[test]
fn a_directory_is_ignored_by_what_it_is_in_and_itself_not_by_what_it_holds() {
    let rules = UploadRules::new(IgnoreList::new(["*"]).shared());
    assert!(!rules.is_in_ignored_directory(Path::new(""), none));
    assert!(rules.is_in_ignored_directory(Path::new("docs"), none));
    assert!(rules.is_ignored(Path::new(""), "a.txt", none));
}

#[test]
fn a_change_of_the_list_is_what_the_next_answer_goes_by() {
    let ignore = IgnoreList::new(["*.tmp"]).shared();
    let rules = UploadRules::new(std::sync::Arc::clone(&ignore));
    assert!(rules.is_ignored(Path::new(""), "a.tmp", none));
    *ignore.write().unwrap() = IgnoreList::new(["*.bak"]);
    assert!(!rules.is_ignored(Path::new(""), "a.tmp", none) && rules.is_ignored(Path::new(""), "a.bak", none));
}

#[test]
fn the_daemons_names_and_a_trash_directory_are_told() {
    let rules = rules();
    assert!(rules.daemon_owned(HOLDING) && !rules.daemon_owned("docs"));
    assert!(rules.is_trash(".Trash-1000") && rules.is_trash(".Trash") && !rules.is_trash("Trash"));
}
