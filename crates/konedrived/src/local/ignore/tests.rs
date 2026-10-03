use super::*;

fn ignored(name: &str) -> bool {
    IgnoreList::default().matches(OsStr::new(name))
}

#[test]
fn the_defaults_catch_editors_temporary_files_and_nothing_else() {
    for name in [
        ".report.txt.swp",
        "notes.txt~",
        ".~lock.budget.ods#",
        "~$budget.xlsx",
        "#draft.org#",
        ".#draft.org",
        ".goutputstream-ABC123",
        "file.kate-swp",
        "movie.mkv.part",
        "setup.exe.crdownload",
        "lu12345abc.tmp",
        ".fuse_hidden0001",
        ".nfs000123",
    ] {
        assert!(ignored(name), "{name}");
    }
    for name in ["report.txt", "swp", "a.swp.txt", "~notes", "lock.budget.ods#", "draft#", "tmp"] {
        assert!(!ignored(name), "{name}");
    }
}

#[test]
fn globs_have_classes_escapes_and_are_case_sensitive() {
    let list = IgnoreList::new(["[a-c]?.log", "[!x]z", "\\*star", "*.TMP"]);
    assert!(list.matches(OsStr::new("b1.log")));
    assert!(!list.matches(OsStr::new("d1.log")));
    assert!(list.matches(OsStr::new("yz")) && !list.matches(OsStr::new("xz")));
    assert!(list.matches(OsStr::new("*star")) && !list.matches(OsStr::new("astar")));
    assert!(list.matches(OsStr::new("A.TMP")) && !list.matches(OsStr::new("a.tmp")));
    assert!(IgnoreList::new(["[unclosed"]).matches(OsStr::new("[unclosed")));
    assert!(IgnoreList::new(["a*b*c"]).matches(OsStr::new("aXbYbZc")));
    assert!(!IgnoreList::new(Vec::<String>::new()).matches(OsStr::new("x.swp")));
}
