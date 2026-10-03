use std::os::unix::ffi::OsStrExt;

use super::*;

#[test]
fn names_onedrive_refuses_are_found_and_others_pass() {
    let cases = [
        ("a:b.txt", Some(Refused::Characters)),
        ("what?", Some(Refused::Characters)),
        ("pipe|.txt", Some(Refused::Characters)),
        (" lead", Some(Refused::Spaces)),
        ("trail ", Some(Refused::Spaces)),
        ("CON", Some(Refused::Reserved)),
        ("com7", Some(Refused::Reserved)),
        ("LPT0", Some(Refused::Reserved)),
        ("Desktop.ini", Some(Refused::Reserved)),
        (".lock", Some(Refused::Reserved)),
        ("my_vti_file", Some(Refused::Reserved)),
        ("~$doc.docx", Some(Refused::Reserved)),
        ("CON.txt", None),
        ("COM10", None),
        ("console", None),
        ("Отчёт 2024.docx", None),
        ("a b", None),
        (".hidden", None),
    ];
    for (name, expected) in cases {
        assert_eq!(refused(OsStr::new(name)), expected, "{name:?}");
    }
    assert_eq!(refused(OsStr::from_bytes(b"caf\xe9")), Some(Refused::NotUtf8));
}

#[test]
fn copies_are_named_after_the_machine_before_the_extension() {
    assert_eq!(copy_name("Report.docx", "fedora", 1), "Report-fedora.docx");
    assert_eq!(copy_name("archive.tar.gz", "fedora", 1), "archive.tar-fedora.gz");
    assert_eq!(copy_name(".bashrc", "fedora", 1), ".bashrc-fedora");
    assert_eq!(copy_name("notes", "fedora", 3), "notes-fedora-3");
    let long = "я".repeat(200) + ".txt";
    let copy = copy_name(&long, "fedora", 1);
    assert!(copy.len() <= 255 && copy.ends_with("-fedora.txt"), "{}", copy.len());
    assert_eq!(machine_name("work-laptop.example.org\n"), "work-laptop");
    assert_eq!(machine_name("a:b?c"), "a-b-c");
    assert_eq!(machine_name(""), "linux");
    assert_eq!(machine_name(&"x".repeat(40)).len(), 32);
}
