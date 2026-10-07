use super::*;

#[test]
fn names_gather_per_directory_and_a_whole_directory_wins() {
    let mut batch = Batch::new();
    assert!(batch.is_empty());
    batch.name(Path::new(""), OsStr::new("."));
    assert_eq!(batch, Batch::new(), "the root's own event is not a change");
    batch.name(Path::new("d"), OsStr::new("a"));
    batch.name(Path::new("d/e"), OsStr::new("."));
    let mut names = Batch::new();
    names.name(Path::new("d"), OsStr::new("a"));
    names.name(Path::new("d"), OsStr::new("e"));
    assert_eq!(batch, names, "a directory's own event names it in its parent");
    let mut whole = Batch::new();
    whole.dir(Path::new("d"));
    batch.merge(whole.clone());
    batch.name(Path::new("d"), OsStr::new("b"));
    assert_eq!(batch, whole);
}
