use super::UploadRules;
use crate::local::IgnoreList;

/// The rules of an account that ignores `*.tmp` and `cache`.
pub(in crate::mount) fn rules() -> UploadRules {
    UploadRules::new(IgnoreList::new(["*.tmp", "cache"]).shared())
}

#[test]
fn a_change_of_the_list_is_what_the_next_answer_goes_by() {
    let ignore = IgnoreList::new(["*.tmp"]).shared();
    let rules = UploadRules::new(std::sync::Arc::clone(&ignore));
    assert!(rules.is_name_ignored("a.tmp".as_ref()));
    *ignore.write().unwrap() = IgnoreList::new(["*.bak"]);
    assert!(!rules.is_name_ignored("a.tmp".as_ref()) && rules.is_name_ignored("a.bak".as_ref()));
}

#[test]
fn a_trash_directory_is_told() {
    let rules = rules();
    assert!(rules.is_trash(".Trash-1000") && rules.is_trash(".Trash") && !rules.is_trash("Trash"));
}

/// What a row of `items` keeps: the list's answer for the name alone.
#[test]
fn a_name_is_asked_by_itself_also_when_it_is_not_utf8() {
    use std::os::unix::ffi::OsStrExt;
    let rules = rules();
    assert!(rules.is_name_ignored("a.tmp".as_ref()));
    assert!(rules.is_name_ignored("cache".as_ref()));
    assert!(!rules.is_name_ignored("a.txt".as_ref()));
    assert!(rules.is_name_ignored(std::ffi::OsStr::from_bytes(b"\xff.tmp")));
}
