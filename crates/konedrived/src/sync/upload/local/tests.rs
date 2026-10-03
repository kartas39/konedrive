use super::*;

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
