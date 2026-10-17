use std::ffi::OsStr;

use super::*;

fn name_of(name: &str, attempt: u32) -> String {
    make_conflict_copy_name(OsStr::new(name), "pc", attempt).into_string().unwrap()
}

#[test]
fn the_computers_name_goes_before_the_extension_and_a_later_attempt_adds_its_number() {
    assert_eq!(name_of("Report.docx", 1), "Report-pc.docx");
    assert_eq!(name_of("Report.docx", 2), "Report-pc-2.docx");
    assert_eq!(name_of("archive.tar.gz", 1), "archive.tar-pc.gz");
    assert_eq!(name_of(".bashrc", 1), ".bashrc-pc", "a leading dot begins no extension");
    assert_eq!(name_of("notes", 3), "notes-pc-3");
}

#[test]
fn a_name_too_long_for_a_directory_entry_is_cut_before_what_is_added_between_two_letters() {
    let long = format!("{}.txt", "é".repeat(126));
    let copy = name_of(&long, 2);
    assert_eq!(copy, format!("{}-pc-2.txt", "é".repeat(123)));
    assert!(copy.len() <= konedrive_fs::NAME_MAX);
}

#[test]
fn a_name_that_is_not_utf8_gets_a_conflict_copys_name_too() {
    use std::os::unix::ffi::OsStrExt;
    let copy = make_conflict_copy_name(OsStr::from_bytes(b"a\xff.txt"), "pc", 1);
    assert_eq!(copy.as_bytes(), b"a\xff-pc.txt");
}

#[test]
fn the_first_name_that_is_not_taken_as_onedrive_compares_names_is_the_free_one() {
    let taken = [FoldedName::of(OsStr::new("a-pc.txt")), FoldedName::of(OsStr::new("a-pc-2.txt"))];
    let free = find_free_conflict_copy_name(OsStr::new("A.txt"), "PC", |name| taken.contains(name));
    assert_eq!(free, Some("A-PC-3.txt".into()));
    assert_eq!(find_free_conflict_copy_name(OsStr::new("A.txt"), "PC", |_| false), Some("A-PC.txt".into()));
    assert_eq!(find_free_conflict_copy_name(OsStr::new("A.txt"), "PC", |_| true), None, "after {CONFLICT_COPY_NAME_TRIES} names");
}
