use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStringExt;

use super::FoldedName;

fn fold(name: &str) -> FoldedName {
    FoldedName::of(OsStr::new(name))
}

/// What is one name to OneDrive, by the client's rule: names that differ by the case of
/// their letters, in any script, and by the form Unicode has for one composed letter.
#[test]
fn names_that_differ_by_case_or_by_the_form_of_a_composed_letter_fold_to_one() {
    for (one, other) in [
        ("Report.TXT", "report.txt"),
        ("ÉCOLE", "école"),
        ("\u{c9}t\u{e9}", "E\u{301}te\u{301}"),
        ("e\u{301}", "\u{e9}"),
        ("ПРИВЕТ", "привет"),
        ("\u{212a}elvin", "kelvin"),
        ("\u{1e9e}", "\u{df}"),
        ("A\u{30a}", "\u{e5}"),
    ] {
        assert_eq!(fold(one), fold(other), "{one:?} and {other:?}");
    }
    assert_eq!(fold("Report.TXT").as_os_str(), OsStr::new("report.txt"));
    assert_eq!(fold("E\u{301}").as_os_str(), OsStr::new("\u{e9}"));
    assert_eq!(fold(fold("\u{c9}T\u{c9}").as_os_str().to_str().unwrap()), fold("\u{c9}T\u{c9}"), "a folded name folds to itself");
}

/// What the rule leaves as two names: letters Unicode calls compatible and not the same,
/// a letter and the two it is sometimes written as, and spaces and dots.
#[test]
fn names_that_differ_by_more_than_case_and_composition_stay_two() {
    for (one, other) in [("\u{fb01}le", "file"), ("\u{ff21}", "a"), ("stra\u{df}e", "strasse"), ("name", "name "), ("name", "name."), ("a b", "a\u{a0}b"), ("e", "\u{e9}")] {
        assert_ne!(fold(one), fold(other), "{one:?} and {other:?}");
    }
}

/// A name that is not UTF-8 folds to itself, its ASCII letters too.
#[test]
fn a_name_that_is_not_utf8_folds_to_itself() {
    let odd = OsString::from_vec(vec![b'O', 0xff, b'D']);
    assert_eq!(FoldedName::of(&odd).as_os_str(), odd.as_os_str());
    assert_ne!(FoldedName::of(&odd), FoldedName::of(&OsString::from_vec(vec![b'o', 0xff, b'd'])));
}
