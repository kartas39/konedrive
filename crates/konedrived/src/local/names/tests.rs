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
