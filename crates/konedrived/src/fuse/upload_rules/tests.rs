use std::path::Path;

use super::UploadRules;
use crate::folder::disk::HOLDING;
use crate::local::IgnoreList;

/// The rules of an account that ignores `*.tmp` and `cache`, on the machine `fedora`.
pub(in crate::fuse) fn rules() -> UploadRules {
    UploadRules::new(IgnoreList::new(["*.tmp", "cache"]).shared(), "fedora".into())
}

fn none(_: &Path) -> bool {
    false
}

#[test]
fn a_name_on_the_ignore_list_stays_on_this_computer() {
    let rules = rules();
    assert!(rules.stays_local(Path::new(""), "a.tmp", none));
    assert!(rules.stays_local(Path::new("docs"), "cache", none));
    assert!(!rules.stays_local(Path::new("docs"), "a.txt", none));
}

#[test]
fn what_is_below_an_ignored_directory_without_an_item_stays_with_it() {
    let rules = rules();
    assert!(rules.stays_local(Path::new("cache"), "a.txt", none));
    assert!(rules.stays_local(Path::new("docs/cache/deep"), "a.txt", none));
    assert!(rules.in_local_dir(Path::new("docs/cache"), none));
    assert!(!rules.in_local_dir(Path::new("docs"), none) && !rules.in_local_dir(Path::new(""), none));
}

/// An item that came from OneDrive is sent whatever its name, and so is what is made in it.
#[test]
fn an_ignored_name_that_carries_an_item_holds_nothing_back() {
    let rules = rules();
    let carries = |at: &Path| at == Path::new("docs/cache");
    assert!(!rules.stays_local(Path::new("docs/cache"), "a.txt", carries));
    assert!(!rules.in_local_dir(Path::new("docs/cache"), carries));
    // Its own name is asked of the list all the same: it has no item.
    assert!(rules.stays_local(Path::new("docs/cache"), "a.tmp", carries));
    // Another directory of that name, which carries none, still stays.
    assert!(rules.in_local_dir(Path::new("cache/docs/cache"), carries));
}

/// The directory a thing stands in is asked by its chain alone: its own name is not a name
/// of the thing.
#[test]
fn a_directory_is_local_by_what_is_above_and_itself_not_by_what_it_holds() {
    let rules = UploadRules::new(IgnoreList::new(["*"]).shared(), "fedora".into());
    assert!(!rules.in_local_dir(Path::new(""), none));
    assert!(rules.in_local_dir(Path::new("docs"), none));
    assert!(rules.stays_local(Path::new(""), "a.txt", none));
}

#[test]
fn a_change_of_the_list_is_what_the_next_answer_goes_by() {
    let ignore = IgnoreList::new(["*.tmp"]).shared();
    let rules = UploadRules::new(std::sync::Arc::clone(&ignore), "fedora".into());
    assert!(rules.stays_local(Path::new(""), "a.tmp", none));
    *ignore.write().unwrap() = IgnoreList::new(["*.bak"]);
    assert!(!rules.stays_local(Path::new(""), "a.tmp", none) && rules.stays_local(Path::new(""), "a.bak", none));
}

#[test]
fn the_daemons_names_and_a_trash_directory_are_told() {
    let rules = rules();
    assert!(rules.daemon_owned(HOLDING) && !rules.daemon_owned("docs"));
    assert!(rules.is_trash(".Trash-1000") && rules.is_trash(".Trash") && !rules.is_trash("Trash"));
}

#[test]
fn a_copys_name_carries_the_machines() {
    let rules = rules();
    assert_eq!(rules.copy_name("Report.docx", 1), "Report-fedora.docx");
    assert_eq!(rules.copy_name("Report.docx", 2), "Report-fedora-2.docx");
}
